//! Claude Code's `async` and `asyncRewake` hooks: they run in the background without holding up
//! their event, their decisions are ignored, an `async` hook's answer reaches the model's next
//! request, and an `asyncRewake` hook that exits 2 notifies the Loop, waking it when idle.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tickloop_harness_model::ModelRequest;
use tickloop_harness_policy::settings::{MemorySettingsStore, SettingsStore, SettingsUpdate};
use tickloop_harness_protocol::hooks::{
    ConfiguredHook, HookBackground, HookBackgroundPhase, HookEvent, HookList, HookRunOutcome,
    HookRunRecord, HookSource, HookTrust,
};
use tickloop_harness_protocol::{
    AgentHostOptions, ChatId, Content, Event, EventKind, ExecutionMode, RunId, TaskNotification,
};
use tickloop_harness_runtime::environment::memory::{MemoryCommandRun, MemoryEnvironment};
use tickloop_harness_runtime::{Agent, HarnessOptions, HarnessSeams};
use tickloop_harness_test_support::harness::ChosenModels;
use tickloop_harness_test_support::{ScriptedModel, call, config, next_event, text};
use tokio::sync::broadcast::Receiver;

const BUILD: &str = "cargo build";
const AUDIT: &str = "audit.sh";
const REVIEW: &str = "review.sh";
/// Long enough that a turn which waited for a hook could not finish before it.
const SLOW_HOOK: Duration = Duration::from_millis(1500);
/// How long a test waits for a background hook to end.
const HOOK_END_WAIT: Duration = Duration::from_secs(20);

fn background_hook(
    event: HookEvent,
    matcher: Option<&str>,
    command: &str,
    background: Option<HookBackground>,
) -> ConfiguredHook {
    ConfiguredHook {
        key: format!("{}:{command}", event.name()),
        source: HookSource::User,
        file: "/settings.json".into(),
        event,
        matcher: matcher.map(Into::into),
        command: command.into(),
        resolved_command: Some(command.into()),
        condition: None,
        timeout_secs: 600,
        background,
        plugin: None,
        hash: None,
        trust: HookTrust::Trusted,
    }
}

fn rewake(message: Option<&str>, summary: Option<&str>) -> Option<HookBackground> {
    Some(HookBackground::AsyncRewake {
        message: message.map(Into::into),
        summary: summary.map(Into::into),
    })
}

fn exits(code: i32, stdout: &str, stderr: &str, duration: Duration) -> MemoryCommandRun {
    MemoryCommandRun {
        stdout: stdout.as_bytes().to_vec(),
        stderr: stderr.as_bytes().to_vec(),
        exit_code: code,
        duration,
        ..Default::default()
    }
}

fn build_call() -> Vec<Content> {
    call("build", "Bash", json!({"command": BUILD}))
}

struct Bench {
    agent: Agent,
    chat: ChatId,
    environment: Arc<MemoryEnvironment>,
    model: Arc<ScriptedModel>,
    _directory: tempfile::TempDir,
}

/// What one turn showed.
#[derive(Default)]
struct Turn {
    results: Vec<String>,
    hook_runs: Vec<HookRunRecord>,
    interrupted: bool,
}

impl Bench {
    async fn start(replies: Vec<Vec<Content>>, hooks: Vec<ConfiguredHook>) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let folder = directory.path().canonicalize().unwrap();
        let environment = Arc::new(MemoryEnvironment::new());
        environment.define_shell(BUILD, Duration::ZERO, b"built".to_vec(), 0);
        let settings = Arc::new(MemorySettingsStore::default());
        settings
            .write(SettingsUpdate::TrustDirectory(folder.clone()))
            .unwrap();
        settings.set_hooks(HookList {
            hooks,
            ..Default::default()
        });
        let model = Arc::new(ScriptedModel::new(replies));
        let agent = Agent::with_seams(
            HarnessSeams {
                search: None,
                transcripts: None,
                environment: Some(environment.clone()),
                models: ChosenModels::new(model.clone()),
            },
            HarnessOptions {
                settings: Some(settings),
                host: AgentHostOptions::embedded_defaults(),
                ..HarnessOptions::local(config())
            },
        )
        .unwrap();
        let chat = agent.create_chat(folder).await.unwrap();
        agent
            .set_execution_mode(chat, ExecutionMode::FullAccess)
            .await
            .unwrap();
        Self {
            agent,
            chat,
            environment,
            model,
            _directory: directory,
        }
    }

    /// Submits `prompt` and follows its turn to its end.
    async fn turn(&self, events: &mut Receiver<Event>, prompt: &str) -> Turn {
        let receipt = self.agent.submit(self.chat, prompt.into()).await.unwrap();
        follow_turn(events, receipt.run_id).await
    }

    fn requests(&self) -> Vec<ModelRequest> {
        self.model.requests.lock().unwrap().clone()
    }
}

async fn follow_turn(events: &mut Receiver<Event>, run_id: RunId) -> Turn {
    let mut turn = Turn::default();
    loop {
        let event = next_event(events).await;
        if event.run_id != run_id {
            continue;
        }
        match event.kind {
            EventKind::ToolCompleted { result, .. } => turn.results.push(result.content),
            EventKind::HookRan { record } => turn.hook_runs.push(*record),
            EventKind::Completed { .. } => return turn,
            EventKind::Interrupted => {
                turn.interrupted = true;
                return turn;
            }
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
}

/// The record of `command`'s background run as it ended, from the Chat's activity history, so
/// an end that came during the turn is found as well.
async fn hook_end(bench: &Bench, command: &str) -> HookRunRecord {
    tokio::time::timeout(HOOK_END_WAIT, async {
        loop {
            let history = bench.agent.approval_history_view(bench.chat).await.unwrap();
            if let Some(record) = history.hook_runs.into_iter().find(|record| {
                record.command == command && record.background == Some(HookBackgroundPhase::Ended)
            }) {
                return record;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the background hook ends")
}

/// The next turn that starts without a prompt of its own.
async fn woken_turn(events: &mut Receiver<Event>, after: RunId) -> Turn {
    loop {
        let event = next_event(events).await;
        if event.run_id != after && matches!(event.kind, EventKind::Started) {
            return follow_turn(events, event.run_id).await;
        }
    }
}

fn texts(request: &ModelRequest) -> Vec<String> {
    request
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|content| match content {
            Content::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

/// The notifications the Chat's views were shown live so far, once those in flight arrived.
async fn shown_live(live: &mut Receiver<TaskNotification>) -> Vec<TaskNotification> {
    tokio::time::sleep(Duration::from_millis(200)).await;
    std::iter::from_fn(|| live.try_recv().ok()).collect()
}

fn assert_prefix_kept(earlier: &ModelRequest, later: &ModelRequest) {
    assert!(
        later.messages.len() > earlier.messages.len()
            && later.messages[..earlier.messages.len()] == earlier.messages[..],
        "a background hook only appends to the conversation"
    );
}

#[tokio::test]
async fn an_async_hook_never_holds_up_its_call_and_its_answer_reaches_the_next_request() {
    let bench = Bench::start(
        vec![build_call(), text("done"), text("noted")],
        vec![background_hook(
            HookEvent::PreToolUse,
            Some("Bash"),
            AUDIT,
            Some(HookBackground::Async),
        )],
    )
    .await;
    let answer = json!({
        "systemMessage": "audit passed",
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": "too late to refuse",
            "additionalContext": "lint is clean",
        },
    });
    bench.environment.define_command(AUDIT, move |spec| {
        let input: Value = serde_json::from_slice(&spec.stdin).unwrap();
        assert_eq!(input["hook_event_name"], "PreToolUse");
        assert_eq!(spec.timeout, Duration::from_secs(600));
        exits(0, &answer.to_string(), "", SLOW_HOOK)
    });
    let mut events = bench.agent.subscribe();
    let mut live = bench.agent.subscribe_task_notifications();
    let started = Instant::now();

    let turn = bench.turn(&mut events, "Build it.").await;

    assert_eq!(turn.results, ["built"], "the hook's deny is ignored");
    assert!(started.elapsed() < SLOW_HOOK, "the call did not wait");
    let [start] = turn.hook_runs.as_slice() else {
        panic!(
            "only the start is recorded during the turn: {:?}",
            turn.hook_runs
        );
    };
    assert_eq!(start.background, Some(HookBackgroundPhase::Started));
    assert_eq!(start.outcome, HookRunOutcome::Success);
    assert_eq!(start.tool_name.as_deref(), Some("Bash"));

    let end = hook_end(&bench, AUDIT).await;
    assert_eq!(end.outcome, HookRunOutcome::Success);
    assert_eq!(end.decision, None, "a background decision decides nothing");
    let reason = end.reason.clone().unwrap();
    assert!(
        reason.contains("ignored") && reason.contains("deny"),
        "{reason}"
    );
    assert_eq!(end.system_message.as_deref(), Some("audit passed"));
    assert_eq!(end.run_id, start.run_id);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        bench.requests().len(),
        2,
        "an async hook never wakes the Loop"
    );
    let history = bench.agent.approval_history_view(bench.chat).await.unwrap();
    assert_eq!(history.hook_runs, [end.clone(), start.clone()]);
    let shown = shown_live(&mut live).await;
    let [shown] = shown.as_slice() else {
        panic!("the views show the hook's notification once: {shown:?}");
    };
    assert_eq!(shown.chat_id, bench.chat);
    assert_eq!(
        shown.summary,
        "PreToolUse:Bash hook finished in the background"
    );

    bench.turn(&mut events, "Anything else?").await;
    let requests = bench.requests();
    let next = texts(&requests[2]).join("\n");
    assert!(next.contains("lint is clean"), "{next}");
    assert!(next.contains("audit passed"), "{next}");
    assert_prefix_kept(&requests[1], &requests[2]);
    bench.agent.shutdown().await;
}

#[tokio::test]
async fn an_async_rewake_exit_two_wakes_the_idle_loop_and_other_statuses_only_show() {
    let quiet = "quiet-review.sh";
    let broken = "broken-review.sh";
    let bench = Bench::start(
        vec![build_call(), text("done"), text("I will move the token.")],
        vec![
            background_hook(
                HookEvent::PostToolUse,
                Some("Bash"),
                REVIEW,
                rewake(
                    Some("Background security review:"),
                    Some("Security review found issues"),
                ),
            ),
            background_hook(HookEvent::PostToolUse, None, quiet, rewake(None, None)),
            background_hook(HookEvent::PostToolUse, None, broken, rewake(None, None)),
        ],
    )
    .await;
    bench.environment.define_command(REVIEW, |_| {
        exits(2, "", "hardcoded token in config.py\n", SLOW_HOOK)
    });
    bench.environment.define_command(quiet, |_| {
        exits(0, "{\"decision\": \"block\"}", "", Duration::ZERO)
    });
    bench
        .environment
        .define_command(broken, |_| exits(1, "", "crashed", Duration::ZERO));
    let mut events = bench.agent.subscribe();
    let mut live = bench.agent.subscribe_task_notifications();

    let started = Instant::now();
    let first = bench
        .agent
        .submit(bench.chat, "Build it.".into())
        .await
        .unwrap();
    let turn = follow_turn(&mut events, first.run_id).await;
    assert_eq!(turn.results, ["built"]);
    assert!(started.elapsed() < SLOW_HOOK, "the turn did not wait");
    assert_eq!(bench.requests().len(), 2);

    let woken = tokio::time::timeout(HOOK_END_WAIT, woken_turn(&mut events, first.run_id))
        .await
        .expect("exit status 2 wakes the idle Loop");
    assert!(!woken.interrupted);
    let requests = bench.requests();
    assert_eq!(requests.len(), 3, "only the exit status 2 wakes the Loop");
    let notification = texts(&requests[2]).last().cloned().unwrap();
    assert!(
        notification.starts_with("<task-notification>"),
        "{notification}"
    );
    assert!(
        notification.contains("Security review found issues"),
        "{notification}"
    );
    assert!(
        notification.contains("Background security review: hardcoded token in config.py"),
        "{notification}"
    );
    assert!(!notification.contains("crashed"));
    assert_prefix_kept(&requests[1], &requests[2]);
    let shown = shown_live(&mut live).await;
    let [shown] = shown.as_slice() else {
        panic!("the views show only the rewake, once: {shown:?}");
    };
    assert_eq!(shown.chat_id, bench.chat);
    assert_eq!(shown.summary, "Security review found issues");

    let history = bench.agent.approval_history_view(bench.chat).await.unwrap();
    let ended = |command: &str| {
        history
            .hook_runs
            .iter()
            .find(|run| {
                run.command == command && run.background == Some(HookBackgroundPhase::Ended)
            })
            .unwrap_or_else(|| panic!("{command} ended"))
            .clone()
    };
    assert_eq!(ended(REVIEW).outcome, HookRunOutcome::Blocking);
    assert_eq!(ended(REVIEW).exit_code, Some(2));
    assert_eq!(ended(quiet).outcome, HookRunOutcome::Success);
    assert_eq!(ended(quiet).decision, None);
    assert_eq!(ended(broken).outcome, HookRunOutcome::NonBlockingError);
    bench.agent.shutdown().await;
}

#[tokio::test]
async fn esc_leaves_background_hooks_running_and_stop_cancels_them() {
    for stops in [false, true] {
        let bench = Bench::start(
            vec![build_call(), text("done")],
            vec![
                background_hook(
                    HookEvent::PreToolUse,
                    None,
                    AUDIT,
                    Some(HookBackground::Async),
                ),
                background_hook(HookEvent::PreToolUse, None, "hang", None),
            ],
        )
        .await;
        bench
            .environment
            .define_command(AUDIT, |_| exits(0, "", "", SLOW_HOOK));
        let hanging = Arc::new(tokio::sync::Notify::new());
        let notify = hanging.clone();
        bench.environment.define_command("hang", move |_| {
            notify.notify_one();
            exits(0, "", "", Duration::from_secs(60))
        });
        let agent = bench.agent.clone();
        let chat = bench.chat;
        let interrupter = tokio::spawn(async move {
            hanging.notified().await;
            if stops {
                agent.stop(chat).await.unwrap();
            } else {
                agent.interrupt(chat).await.unwrap();
            }
        });
        let mut events = bench.agent.subscribe();
        let started = Instant::now();

        let turn = bench.turn(&mut events, "Build it.").await;
        interrupter.await.unwrap();
        assert!(turn.interrupted, "stop={stops}");

        let end = hook_end(&bench, AUDIT).await;
        if stops {
            assert_eq!(end.outcome, HookRunOutcome::Cancelled);
            assert!(started.elapsed() < SLOW_HOOK, "/stop ends it at once");
        } else {
            assert_eq!(
                end.outcome,
                HookRunOutcome::Success,
                "Esc leaves it running"
            );
            assert!(started.elapsed() >= SLOW_HOOK);
        }
        bench.agent.shutdown().await;
    }
}

#[tokio::test]
async fn a_background_hook_times_out_as_any_hook_does() {
    let mut slow = background_hook(HookEvent::PostToolUse, None, REVIEW, rewake(None, None));
    slow.timeout_secs = 1;
    let bench = Bench::start(vec![build_call(), text("done")], vec![slow]).await;
    bench.environment.define_command(REVIEW, |spec| {
        assert_eq!(spec.timeout, Duration::from_secs(1));
        exits(2, "", "too late", Duration::from_secs(30))
    });
    let mut events = bench.agent.subscribe();

    bench.turn(&mut events, "Build it.").await;
    let end = hook_end(&bench, REVIEW).await;

    assert_eq!(end.outcome, HookRunOutcome::TimedOut);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(bench.requests().len(), 2, "a timed-out hook wakes nothing");
    bench.agent.shutdown().await;
}

#[tokio::test]
async fn the_same_command_runs_both_in_the_foreground_and_in_the_background() {
    let bench = Bench::start(
        vec![build_call(), text("done")],
        vec![
            background_hook(HookEvent::PostToolUse, None, AUDIT, None),
            background_hook(
                HookEvent::PostToolUse,
                None,
                AUDIT,
                Some(HookBackground::Async),
            ),
        ],
    )
    .await;
    let runs = Arc::new(AtomicUsize::new(0));
    let counted = runs.clone();
    bench.environment.define_command(AUDIT, move |_| {
        counted.fetch_add(1, Ordering::SeqCst);
        exits(0, "", "", Duration::ZERO)
    });
    let mut events = bench.agent.subscribe();

    let turn = bench.turn(&mut events, "Build it.").await;
    hook_end(&bench, AUDIT).await;

    assert_eq!(runs.load(Ordering::SeqCst), 2);
    assert!(
        turn.hook_runs.iter().any(|run| run.background.is_none()),
        "the foreground run is not merged into the background one"
    );
    bench.agent.shutdown().await;
}

/// The line with which Claude Code 2.1.302 lets a running hook go on in the background, as
/// security-guidance's `SessionStart` bootstrap prints it.
const ASYNC_FIRST_LINE: &str = "{\"async\": true, \"asyncTimeout\": 180000}\n";

#[tokio::test]
async fn a_first_line_asking_for_async_sends_a_running_hook_to_the_background() {
    let bootstrap = "ensure_agent_sdk.sh";
    let bench = Bench::start(
        vec![text("hello"), text("noted")],
        vec![background_hook(
            HookEvent::SessionStart,
            None,
            bootstrap,
            None,
        )],
    )
    .await;
    let answer = json!({
        "metrics": {"sdk_bootstrap": 0},
        "systemMessage": "SDK ready",
        "hookSpecificOutput": {
            "hookEventName": "SessionStart",
            "additionalContext": "the commit reviewer is available",
        },
    });
    bench
        .environment
        .define_command(bootstrap, move |_| MemoryCommandRun {
            early_stdout: ASYNC_FIRST_LINE.as_bytes().to_vec(),
            ..exits(0, &answer.to_string(), "", SLOW_HOOK)
        });
    let mut events = bench.agent.subscribe();
    let started = Instant::now();

    let turn = bench.turn(&mut events, "Hi.").await;

    assert!(started.elapsed() < SLOW_HOOK, "the session did not wait");
    let [start] = turn.hook_runs.as_slice() else {
        panic!(
            "only the start is recorded during the turn: {:?}",
            turn.hook_runs
        );
    };
    assert_eq!(start.event, HookEvent::SessionStart);
    assert_eq!(start.background, Some(HookBackgroundPhase::Started));
    assert_eq!(start.outcome, HookRunOutcome::Success);
    assert!(
        !texts(&bench.requests()[0])
            .join("\n")
            .contains("commit reviewer"),
        "the first request did not wait for the hook's answer"
    );

    let end = hook_end(&bench, bootstrap).await;
    assert_eq!(end.outcome, HookRunOutcome::Success);
    assert_eq!(end.reason, None, "its output reads as its JSON answer");
    assert_eq!(end.system_message.as_deref(), Some("SDK ready"));
    assert!(!end.output.contains("\"async\""), "{}", end.output);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        bench.requests().len(),
        1,
        "an async hook never wakes the Loop"
    );

    bench.turn(&mut events, "Anything else?").await;
    let requests = bench.requests();
    let next = texts(&requests[1]).join("\n");
    assert!(next.contains("the commit reviewer is available"), "{next}");
    assert!(next.contains("SDK ready"), "{next}");
    assert_prefix_kept(&requests[0], &requests[1]);
    bench.agent.shutdown().await;
}

#[tokio::test]
async fn a_hook_that_asked_for_async_decides_nothing_however_soon_it_ends() {
    for duration in [Duration::ZERO, SLOW_HOOK] {
        let bench = Bench::start(
            vec![build_call(), text("done")],
            vec![background_hook(
                HookEvent::PreToolUse,
                Some("Bash"),
                AUDIT,
                None,
            )],
        )
        .await;
        let deny = json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": "too late to refuse",
            },
        });
        bench
            .environment
            .define_command(AUDIT, move |_| MemoryCommandRun {
                early_stdout: ASYNC_FIRST_LINE.as_bytes().to_vec(),
                ..exits(0, &deny.to_string(), "", duration)
            });
        let mut events = bench.agent.subscribe();

        let turn = bench.turn(&mut events, "Build it.").await;

        assert_eq!(turn.results, ["built"], "the deny is ignored, {duration:?}");
        assert_eq!(
            turn.hook_runs[0].background,
            Some(HookBackgroundPhase::Started),
            "{duration:?}"
        );
        let end = hook_end(&bench, AUDIT).await;
        assert_eq!(end.decision, None, "{duration:?}");
        assert!(
            end.reason
                .as_deref()
                .is_some_and(|reason| reason.contains("ignored")),
            "{:?}",
            end.reason
        );
        bench.agent.shutdown().await;
    }
}

#[tokio::test]
async fn esc_leaves_a_hook_that_asked_for_async_running_and_stop_ends_it() {
    for stops in [false, true] {
        let bench = Bench::start(
            vec![build_call(), text("done")],
            vec![background_hook(
                HookEvent::PreToolUse,
                Some("Bash"),
                AUDIT,
                None,
            )],
        )
        .await;
        bench
            .environment
            .define_command(AUDIT, |_| MemoryCommandRun {
                early_stdout: ASYNC_FIRST_LINE.as_bytes().to_vec(),
                ..exits(0, "", "", SLOW_HOOK)
            });
        let mut events = bench.agent.subscribe();
        let started = Instant::now();

        let turn = bench.turn(&mut events, "Build it.").await;
        assert_eq!(turn.results, ["built"]);
        if stops {
            bench.agent.stop(bench.chat).await.unwrap();
        } else {
            bench.agent.interrupt(bench.chat).await.unwrap();
        }

        let end = hook_end(&bench, AUDIT).await;
        if stops {
            assert_eq!(end.outcome, HookRunOutcome::Cancelled);
            assert!(started.elapsed() < SLOW_HOOK, "/stop ends it at once");
        } else {
            assert_eq!(
                end.outcome,
                HookRunOutcome::Success,
                "Esc leaves it running"
            );
            assert!(started.elapsed() >= SLOW_HOOK);
        }
        bench.agent.shutdown().await;
    }
}

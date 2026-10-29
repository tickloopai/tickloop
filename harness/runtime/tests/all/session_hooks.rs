//! Claude Code–compatible `UserPromptSubmit`, `SessionStart`, `SessionEnd`, `Stop`,
//! `SubagentStop`, `StopFailure` and `PreCompact` hooks on the Loop's lifecycle, run through the
//! reference Execution Environment's one-shot commands.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;

use serde_json::{Value, json};
use tickloop_harness_model::{ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest};
use tickloop_harness_policy::settings::{MemorySettingsStore, SettingsStore, SettingsUpdate};
use tickloop_harness_protocol::hooks::{
    ConfiguredHook, HookEvent, HookList, HookRunOutcome, HookRunRecord, HookSource, HookTrust,
};
use tickloop_harness_protocol::{
    AgentHostOptions, ChatId, Content, EventKind, ExecutionMode, GoalAction, GoalStatus, Message,
    Receipt, Role, RunId,
};
use tickloop_harness_runtime::environment::memory::{MemoryCommandRun, MemoryEnvironment};
use tickloop_harness_runtime::transcript::{JsonlTranscriptStore, TranscriptStore};
use tickloop_harness_runtime::{Agent, HarnessOptions, HarnessSeams};
use tickloop_harness_test_support::harness::ChosenModels;
use tickloop_harness_test_support::routed::RoutedModel;
use tickloop_harness_test_support::{ScriptedModel, call, config, next_event, text};
use tokio::sync::{Notify, broadcast};
use tokio_util::sync::CancellationToken;

/// Requests start with the system prompt and the user context.
const FIRST_CONVERSATION_MESSAGE: usize = 2;
const RALPH_PROMPT: &str = "Keep improving the README until it is perfect.";

fn user_hook(event: HookEvent, matcher: Option<&str>, command: &str) -> ConfiguredHook {
    ConfiguredHook {
        key: format!("{}:{}:{command}", event.name(), matcher.unwrap_or_default()),
        hash: None,
        source: HookSource::User,
        file: "/settings.json".into(),
        event,
        matcher: matcher.map(Into::into),
        command: command.into(),
        resolved_command: Some(command.into()),
        sensitive_values: Vec::new(),
        condition: None,
        timeout_secs: 600,
        background: None,
        plugin: None,
        trust: HookTrust::Trusted,
    }
}

fn exits(code: i32, stdout: &str, stderr: &str) -> MemoryCommandRun {
    MemoryCommandRun {
        stdout: stdout.as_bytes().to_vec(),
        stderr: stderr.as_bytes().to_vec(),
        exit_code: code,
        duration: Duration::ZERO,
        ..Default::default()
    }
}

fn answer(stdout: Value) -> MemoryCommandRun {
    exits(0, &stdout.to_string(), "")
}

fn context_answer(event: HookEvent, context: &str) -> MemoryCommandRun {
    answer(json!({
        "hookSpecificOutput": {"hookEventName": event.name(), "additionalContext": context}
    }))
}

/// The Bench's model, which keeps the request it is told to hold until the test releases it.
struct GatedModel {
    inner: Arc<dyn ModelClient>,
    holds_next: AtomicBool,
    entered: Notify,
    release: Notify,
}

#[async_trait::async_trait]
impl ModelClient for GatedModel {
    async fn generate_title(
        &self,
        request: ModelRequest,
        deltas: tokio::sync::mpsc::UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        self.inner.generate_title(request, deltas, cancel).await
    }

    fn identity(&self) -> tickloop_harness_protocol::ModelIdentity {
        self.inner.identity()
    }

    async fn complete(
        &self,
        request: ModelRequest,
        deltas: tokio::sync::mpsc::UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        if self.holds_next.swap(false, Ordering::SeqCst) {
            self.entered.notify_one();
            tokio::select! {
                () = cancel.cancelled() => {}
                () = self.release.notified() => {}
            }
        }
        self.inner.complete(request, deltas, cancel).await
    }
}

/// One Agent with a Folder, the reference environment and hooks from the Settings Store.
struct Bench {
    agent: Agent,
    chat: ChatId,
    environment: Arc<MemoryEnvironment>,
    settings: Arc<MemorySettingsStore>,
    model: Arc<ScriptedModel>,
    routed: Option<Arc<RoutedModel>>,
    gate: Arc<GatedModel>,
    hook_inputs: Arc<Mutex<Vec<(String, Value)>>>,
    directory: tempfile::TempDir,
}

impl Bench {
    async fn start(replies: Vec<Vec<Content>>, hooks: Vec<ConfiguredHook>) -> Self {
        Self::start_with(replies, None, hooks, false).await
    }

    async fn start_with(
        replies: Vec<Vec<Content>>,
        routed: Option<Arc<RoutedModel>>,
        hooks: Vec<ConfiguredHook>,
        keeps_transcripts: bool,
    ) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let folder = directory.path().canonicalize().unwrap();
        let environment = Arc::new(MemoryEnvironment::new());
        let settings = Arc::new(MemorySettingsStore::default());
        settings
            .write(SettingsUpdate::TrustDirectory(folder.clone()))
            .unwrap();
        settings.set_hooks(HookList {
            hooks,
            ..Default::default()
        });
        let model = Arc::new(ScriptedModel::new(replies));
        let gate = Arc::new(GatedModel {
            inner: match &routed {
                Some(routed) => routed.clone(),
                None => model.clone(),
            },
            holds_next: AtomicBool::new(false),
            entered: Notify::new(),
            release: Notify::new(),
        });
        let agent = build_agent(
            &directory,
            &environment,
            &settings,
            gate.clone(),
            keeps_transcripts,
        );
        let chat = agent.create_chat(folder).await.unwrap();
        agent
            .set_execution_mode(chat, ExecutionMode::FullAccess)
            .await
            .unwrap();
        Self {
            agent,
            chat,
            environment,
            settings,
            model,
            routed,
            gate,
            hook_inputs: Arc::default(),
            directory,
        }
    }

    /// Scripts `command` and remembers each input it reads.
    fn define(
        &self,
        command: &str,
        run: impl Fn(&Value) -> MemoryCommandRun + Send + Sync + 'static,
    ) {
        let inputs = self.hook_inputs.clone();
        let name = command.to_owned();
        self.environment.define_command(command, move |spec| {
            let input: Value = serde_json::from_slice(&spec.stdin).unwrap();
            inputs.lock().unwrap().push((name.clone(), input.clone()));
            run(&input)
        });
    }

    fn inputs_of(&self, command: &str) -> Vec<Value> {
        self.hook_inputs
            .lock()
            .unwrap()
            .iter()
            .filter(|(name, _)| name == command)
            .map(|(_, input)| input.clone())
            .collect()
    }

    fn requests(&self) -> Vec<ModelRequest> {
        match &self.routed {
            Some(routed) => routed.requests.lock().unwrap().clone(),
            None => self.model.requests.lock().unwrap().clone(),
        }
    }

    async fn run(&self, prompt: &str) -> Observed {
        let mut events = self.agent.subscribe();
        let receipt = self.agent.submit(self.chat, prompt.into()).await.unwrap();
        observe(&mut events, &receipt).await
    }

    /// Runs `prompt`, queueing `queued` to its turn while its first model request is in flight.
    async fn run_queueing(&self, prompt: &str, queued: &[&str]) -> QueuedRun {
        let mut events = self.agent.subscribe();
        self.gate.holds_next.store(true, Ordering::SeqCst);
        let receipt = self.agent.submit(self.chat, prompt.into()).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), self.gate.entered.notified())
            .await
            .expect("the first model request starts");
        let mut receipts = Vec::new();
        for input in queued {
            receipts.push(self.agent.submit(self.chat, (*input).into()).await.unwrap());
        }
        self.gate.release.notify_one();
        let observed = observe(&mut events, &receipt).await;
        QueuedRun {
            receipt,
            observed,
            queued: receipts,
            events,
        }
    }
}

/// A run with input queued to it while it went on.
struct QueuedRun {
    receipt: Receipt,
    observed: Observed,
    queued: Vec<Receipt>,
    events: broadcast::Receiver<tickloop_harness_protocol::Event>,
}

fn build_agent(
    directory: &tempfile::TempDir,
    environment: &Arc<MemoryEnvironment>,
    settings: &Arc<MemorySettingsStore>,
    model: Arc<dyn ModelClient>,
    keeps_transcripts: bool,
) -> Agent {
    Agent::with_seams(
        HarnessSeams {
            search: None,
            transcripts: keeps_transcripts.then(|| {
                Arc::new(JsonlTranscriptStore::open(directory.path().join("transcripts")).unwrap())
                    as Arc<dyn TranscriptStore>
            }),
            environment: Some(environment.clone()),
            models: ChosenModels::new(model),
        },
        HarnessOptions {
            settings: Some(settings.clone()),
            host: AgentHostOptions::embedded_defaults(),
            ..HarnessOptions::local(config())
        },
    )
    .unwrap()
}

#[derive(Default)]
struct Observed {
    hook_runs: Vec<HookRunRecord>,
    /// Each queued input that joined a run, with the run it joined.
    merged: Vec<(RunId, RunId)>,
    /// How often views were told to redraw the conversation.
    rewinds: usize,
    interrupted: bool,
    failed: Option<String>,
}

async fn observe(
    events: &mut tokio::sync::broadcast::Receiver<tickloop_harness_protocol::Event>,
    receipt: &Receipt,
) -> Observed {
    let mut observed = Observed::default();
    loop {
        let event = next_event(events).await;
        if event.chat_id != receipt.chat_id {
            continue;
        }
        match event.kind {
            EventKind::HookRan { record } => observed.hook_runs.push(*record),
            EventKind::InputMerged { into_run_id } => {
                observed.merged.push((event.run_id, into_run_id));
            }
            EventKind::ConversationRewound { .. } => observed.rewinds += 1,
            EventKind::Completed { .. } if event.run_id == receipt.run_id => break,
            EventKind::Interrupted if event.run_id == receipt.run_id => {
                observed.interrupted = true;
                break;
            }
            EventKind::Failed(error) if event.run_id == receipt.run_id => {
                observed.failed = Some(error);
                break;
            }
            _ => {}
        }
    }
    observed
}

/// The host notes a request's conversation ends with, after its last assistant message.
fn trailing_texts(request: &ModelRequest) -> Vec<String> {
    let start = request
        .messages
        .iter()
        .rposition(|message| message.role == Role::Assistant)
        .map_or(FIRST_CONVERSATION_MESSAGE, |index| index + 1);
    request.messages[start..]
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|content| match content {
            Content::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

/// Every request repeats the one before it byte for byte, so hook notes never break the
/// prompt cache, and no request recorded a cache break.
async fn assert_prefix_stable(bench: &Bench) {
    let requests = bench.requests();
    for pair in requests.windows(2) {
        let (earlier, later) = (&pair[0].messages, &pair[1].messages);
        assert!(
            later.len() > earlier.len() && later[..earlier.len()] == earlier[..],
            "a later request changed the conversation an earlier one sent"
        );
    }
    let ledger = bench
        .agent
        .snapshot(bench.chat)
        .await
        .unwrap()
        .request_usage;
    assert!(
        ledger
            .entries()
            .iter()
            .all(|record| record.cache_break.is_none()),
        "{ledger:?}"
    );
}

fn conversation_texts(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|content| match content {
            Content::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn user_prompt_submit_context_and_plain_output_join_the_prompt_at_the_end() {
    let bench = Bench::start(
        vec![text("first"), text("second")],
        vec![
            user_hook(HookEvent::UserPromptSubmit, None, "context.sh"),
            user_hook(HookEvent::UserPromptSubmit, None, "plain.sh"),
        ],
    )
    .await;
    bench.define("context.sh", |input| {
        context_answer(
            HookEvent::UserPromptSubmit,
            &format!("prompt length {}", input["prompt"].as_str().unwrap().len()),
        )
    });
    bench.define("plain.sh", |_| exits(0, "branch main\n", ""));
    bench.run("Explain the code.").await;
    bench.run("And the tests.").await;

    let input = &bench.inputs_of("context.sh")[0];
    assert_eq!(input["hook_event_name"], "UserPromptSubmit");
    assert_eq!(input["prompt"], "Explain the code.");
    assert_eq!(input["session_id"], bench.chat.uuid().to_string());
    let first = &bench.requests()[0];
    let trailing = trailing_texts(first);
    assert_eq!(trailing[0], "Explain the code.");
    let context = trailing
        .iter()
        .position(|text| {
            text.contains("UserPromptSubmit hook additional context: prompt length 17")
        })
        .expect("the context follows the prompt");
    assert!(trailing[context].starts_with("<system-reminder>\n"));
    assert!(
        trailing[context + 1].contains("UserPromptSubmit hook success: branch main"),
        "{trailing:?}"
    );
    assert_prefix_stable(&bench).await;
    let snapshot = bench.agent.snapshot(bench.chat).await.unwrap();
    let notes = snapshot
        .host_notes
        .values()
        .filter(|note| {
            **note == tickloop_harness_protocol::HostNote::Hook(HookEvent::UserPromptSubmit)
        })
        .count();
    assert_eq!(notes, 4, "the notes are hidden as host notes");
    bench.agent.shutdown().await;
}

#[tokio::test]
async fn a_blocked_prompt_never_enters_the_conversation_and_the_user_sees_why() {
    let bench = Bench::start_with(
        vec![text("answered")],
        None,
        vec![user_hook(HookEvent::UserPromptSubmit, None, "secrets.sh")],
        true,
    )
    .await;
    bench.define("secrets.sh", |input| {
        if input["prompt"].as_str().unwrap().contains("password") {
            exits(2, "", "prompts must not carry passwords\n")
        } else {
            exits(0, "", "")
        }
    });
    let observed = bench.run("My password is hunter2.").await;
    assert!(observed.failed.is_none() && !observed.interrupted);
    assert!(bench.requests().is_empty(), "the model never reads it");
    let run = &observed.hook_runs[0];
    assert_eq!(run.outcome, HookRunOutcome::Blocking);
    assert_eq!(
        run.reason.as_deref(),
        Some("[secrets.sh]: prompts must not carry passwords")
    );
    let snapshot = bench.agent.snapshot(bench.chat).await.unwrap();
    assert!(
        !conversation_texts(&snapshot.messages)
            .iter()
            .any(|text| text.contains("hunter2")),
        "{:?}",
        snapshot.messages
    );

    bench.run("Say hello.").await;
    let request = &bench.requests()[0];
    assert!(
        !conversation_texts(&request.messages)
            .iter()
            .any(|text| text.contains("hunter2"))
    );
    assert_eq!(trailing_texts(request)[0], "Say hello.");
    let title = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let snapshot = bench.agent.snapshot(bench.chat).await.unwrap();
            if let Some(title) = snapshot.generated_title {
                return title;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the prompt that passed its hooks names the Chat");
    assert!(
        format!("{title:?}").contains("Say hello") && !format!("{title:?}").contains("hunter2"),
        "the title model never reads a blocked prompt: {title:?}"
    );
    bench.agent.shutdown().await;

    let model = Arc::new(ScriptedModel::new(Vec::new()));
    let reloaded = build_agent(
        &bench.directory,
        &bench.environment,
        &bench.settings,
        model,
        true,
    );
    let texts = conversation_texts(&reloaded.resume_chat(bench.chat).await.unwrap().messages);
    assert!(
        !texts.iter().any(|text| text.contains("hunter2")),
        "a load keeps it out too: {texts:?}"
    );
    assert!(texts.iter().any(|text| text == "Say hello."));
    reloaded.shutdown().await;
}

#[tokio::test]
async fn continue_false_keeps_the_prompt_and_ends_the_turn_unanswered() {
    let bench = Bench::start(
        vec![text("never asked")],
        vec![user_hook(HookEvent::UserPromptSubmit, None, "pause.sh")],
    )
    .await;
    bench.define("pause.sh", |_| {
        answer(json!({"continue": false, "stopReason": "Paused for review"}))
    });
    let observed = bench.run("Deploy it.").await;
    assert!(observed.failed.is_none());
    assert!(bench.requests().is_empty());
    let snapshot = bench.agent.snapshot(bench.chat).await.unwrap();
    let texts = conversation_texts(&snapshot.messages);
    assert_eq!(texts[0], "Deploy it.");
    assert!(texts[1].contains("Paused for review"), "{texts:?}");
    bench.agent.shutdown().await;
}

#[tokio::test]
async fn session_start_runs_once_for_a_new_chat_and_its_context_follows_the_prompt() {
    let bench = Bench::start(
        vec![text("one"), text("two")],
        vec![
            user_hook(HookEvent::SessionStart, Some("startup"), "style.sh"),
            user_hook(HookEvent::SessionStart, Some("resume|compact"), "other.sh"),
            user_hook(HookEvent::SessionStart, None, "broken.sh"),
        ],
    )
    .await;
    bench.define("style.sh", |_| {
        context_answer(HookEvent::SessionStart, "Explain your choices as you work.")
    });
    bench.define("other.sh", |_| exits(0, "must not run", ""));
    bench.define("broken.sh", |_| exits(2, "", "no such style\n"));
    let observed = bench.run("Refactor it.").await;
    bench.run("Again.").await;

    assert_eq!(bench.inputs_of("style.sh").len(), 1, "once per session");
    assert!(bench.inputs_of("other.sh").is_empty());
    let input = &bench.inputs_of("style.sh")[0];
    assert_eq!(input["hook_event_name"], "SessionStart");
    assert_eq!(input["source"], "startup");
    assert_eq!(input["model"], config().model);
    let trailing = trailing_texts(&bench.requests()[0]);
    assert_eq!(trailing[0], "Refactor it.");
    assert!(
        trailing.iter().any(|text| text.contains(
            "SessionStart:startup hook additional context: Explain your choices as you work."
        )),
        "{trailing:?}"
    );
    assert!(
        !trailing.iter().any(|text| text.contains("no such style")),
        "a SessionStart block only tells the user"
    );
    assert!(
        observed
            .hook_runs
            .iter()
            .any(|run| run.outcome == HookRunOutcome::Blocking)
    );
    assert_prefix_stable(&bench).await;
    bench.agent.shutdown().await;
}

#[tokio::test]
async fn session_start_tells_hooks_a_chat_was_cleared() {
    let bench = Bench::start(
        vec![text("fresh")],
        vec![user_hook(
            HookEvent::SessionStart,
            Some("clear"),
            "clear.sh",
        )],
    )
    .await;
    bench.define("clear.sh", |_| exits(0, "cleared context\n", ""));
    bench
        .agent
        .start_session_after_clear(bench.chat)
        .await
        .unwrap();
    bench.run("Start over.").await;
    assert_eq!(bench.inputs_of("clear.sh")[0]["source"], "clear");
    assert!(
        trailing_texts(&bench.requests()[0])
            .iter()
            .any(|text| text.contains("SessionStart:clear hook success: cleared context"))
    );
    bench.agent.shutdown().await;
}

#[tokio::test]
async fn pre_compact_reads_its_trigger_adds_instructions_and_compaction_runs_session_start() {
    let bench = Bench::start(
        vec![text("one"), text("Summary of the work."), text("two")],
        vec![
            user_hook(HookEvent::PreCompact, Some("manual"), "keep.sh"),
            user_hook(HookEvent::PreCompact, Some("auto"), "auto-only.sh"),
            user_hook(HookEvent::SessionStart, Some("compact"), "after.sh"),
        ],
    )
    .await;
    bench.define("keep.sh", |_| exits(0, "Keep every file path.\n", ""));
    bench.define("auto-only.sh", |_| exits(2, "", "must not run"));
    bench.define("after.sh", |_| {
        context_answer(HookEvent::SessionStart, "Re-read CONTRIBUTING.md.")
    });
    bench.run("Do the work.").await;
    bench
        .agent
        .compact_with_instructions(bench.chat, Some("Focus on tests.".into()))
        .await
        .unwrap();
    let input = &bench.inputs_of("keep.sh")[0];
    assert_eq!(input["hook_event_name"], "PreCompact");
    assert_eq!(input["trigger"], "manual");
    assert_eq!(input["custom_instructions"], "Focus on tests.");
    assert!(bench.inputs_of("auto-only.sh").is_empty());
    let summary_request = &bench.requests()[1];
    let instructions = trailing_texts(summary_request).join("\n");
    assert!(
        instructions.contains("Focus on tests.\n\nKeep every file path."),
        "{instructions}"
    );

    bench.run("Continue.").await;
    assert_eq!(bench.inputs_of("after.sh")[0]["source"], "compact");
    assert!(trailing_texts(&bench.requests()[2]).iter().any(|text| {
        text.contains("SessionStart:compact hook additional context: Re-read CONTRIBUTING.md.")
    }));
    bench.agent.shutdown().await;
}

#[tokio::test]
async fn a_pre_compact_block_cancels_the_compaction() {
    let bench = Bench::start(
        vec![text("one")],
        vec![user_hook(HookEvent::PreCompact, None, "hold.sh")],
    )
    .await;
    bench.define("hold.sh", |_| exits(2, "", "a review is in progress\n"));
    bench.run("Do the work.").await;
    let before = bench.agent.snapshot(bench.chat).await.unwrap().messages;
    let error = bench.agent.compact(bench.chat).await.unwrap_err();
    assert!(
        error.to_string().contains("a review is in progress"),
        "{error}"
    );
    assert_eq!(bench.requests().len(), 1, "no summary was asked for");
    assert_eq!(
        bench.agent.snapshot(bench.chat).await.unwrap().messages,
        before
    );
    bench.agent.shutdown().await;
}

#[tokio::test]
async fn a_stop_block_goes_on_with_its_reason_and_the_next_input_says_it_is_active() {
    let bench = Bench::start(
        vec![text("first"), text("second")],
        vec![user_hook(HookEvent::Stop, None, "check.sh")],
    )
    .await;
    bench.define("check.sh", |input| {
        if input["stop_hook_active"] == json!(true) {
            exits(0, "", "")
        } else {
            answer(json!({"decision": "block", "reason": "Run the tests first."}))
        }
    });
    let observed = bench.run("Fix the bug.").await;
    assert!(observed.failed.is_none());
    let inputs = bench.inputs_of("check.sh");
    assert_eq!(inputs.len(), 2);
    assert_eq!(inputs[0]["hook_event_name"], "Stop");
    assert_eq!(inputs[0]["stop_hook_active"], false);
    assert_eq!(inputs[0]["last_assistant_message"], "first");
    assert_eq!(inputs[1]["stop_hook_active"], true);
    assert_eq!(inputs[1]["last_assistant_message"], "second");
    let requests = bench.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        trailing_texts(&requests[1])
            .iter()
            .any(|text| text.contains("Stop hook feedback:\nRun the tests first.")),
        "{:?}",
        trailing_texts(&requests[1])
    );
    assert_prefix_stable(&bench).await;

    bench.model.enqueue(text("third"));
    bench.run("Next.").await;
    assert_eq!(
        bench.inputs_of("check.sh")[2]["stop_hook_active"],
        false,
        "a new turn starts afresh"
    );
    bench.agent.shutdown().await;
}

#[tokio::test]
async fn stop_hook_errors_and_continue_false_end_the_turn() {
    for (code, stdout) in [(1, ""), (0, r#"{"continue":false,"stopReason":"Done"}"#)] {
        let bench = Bench::start(
            vec![text("only")],
            vec![user_hook(HookEvent::Stop, None, "stop.sh")],
        )
        .await;
        bench.define("stop.sh", move |_| exits(code, stdout, "oops"));
        let observed = bench.run("Go.").await;
        assert!(observed.failed.is_none());
        assert_eq!(bench.requests().len(), 1);
        bench.agent.shutdown().await;
    }
}

/// ralph-loop's Stop hook: every turn's end feeds the same prompt back until the model says
/// its completion promise, which this model never does.
#[tokio::test]
async fn a_ralph_loop_stop_hook_keeps_going_until_esc_or_stop_ends_it() {
    for stops in [false, true] {
        let routed = Arc::new(RoutedModel::new(|_| text("Still improving the README.")));
        let bench = Bench::start_with(
            Vec::new(),
            Some(routed),
            vec![user_hook(HookEvent::Stop, None, "ralph.sh")],
            false,
        )
        .await;
        let iterations = Arc::new(AtomicUsize::new(0));
        let reached = Arc::new(tokio::sync::Notify::new());
        let counter = iterations.clone();
        let notify = reached.clone();
        let gate = bench.gate.clone();
        bench.define("ralph.sh", move |_| {
            if counter.fetch_add(1, Ordering::SeqCst) + 1 == 5 {
                // The request the fifth block goes on with waits for the interruption, so the
                // loop stays under the cap on consecutive blocks.
                gate.holds_next.store(true, Ordering::SeqCst);
                notify.notify_one();
            }
            answer(json!({
                "decision": "block",
                "reason": RALPH_PROMPT,
                "systemMessage": "Ralph iteration"
            }))
        });
        let mut events = bench.agent.subscribe();
        let receipt = bench
            .agent
            .submit(bench.chat, RALPH_PROMPT.into())
            .await
            .unwrap();
        let agent = bench.agent.clone();
        let chat = bench.chat;
        let interrupter = tokio::spawn(async move {
            reached.notified().await;
            agent
                .manage_goal(
                    chat,
                    GoalAction::Set {
                        objective: "The README is perfect.".into(),
                        token_budget: None,
                        replace: false,
                    },
                )
                .await
                .unwrap();
            if stops {
                agent.stop(chat).await.unwrap();
            } else {
                agent.interrupt(chat).await.unwrap();
            }
        });
        let observed =
            tokio::time::timeout(Duration::from_secs(20), observe(&mut events, &receipt))
                .await
                .expect("the loop ends when interrupted");
        interrupter.await.unwrap();
        assert!(observed.interrupted, "stop={stops}");
        let ran = iterations.load(Ordering::SeqCst);
        assert_eq!(ran, 5);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            iterations.load(Ordering::SeqCst),
            ran,
            "nothing goes on after the interruption"
        );
        let goal = bench
            .agent
            .snapshot(bench.chat)
            .await
            .unwrap()
            .goal
            .unwrap();
        assert!(
            matches!(goal.status, GoalStatus::Paused { .. }),
            "Esc and /stop pause the goal the loop works on: {:?}",
            goal.status
        );
        let inputs = bench.inputs_of("ralph.sh");
        assert_eq!(inputs[0]["stop_hook_active"], false);
        assert!(
            inputs[1..]
                .iter()
                .all(|input| input["stop_hook_active"] == true)
        );
        assert!(
            observed
                .hook_runs
                .iter()
                .filter(|run| run.outcome != HookRunOutcome::Cancelled)
                .all(|run| run.system_message.as_deref() == Some("Ralph iteration"))
        );
        bench.agent.shutdown().await;
    }
}

/// ralph-loop's Stop hook, which lets the turn end once it has gone on.
fn ralph_until_active(input: &Value) -> MemoryCommandRun {
    if input["stop_hook_active"] == json!(true) {
        exits(0, "", "")
    } else {
        answer(json!({"decision": "block", "reason": RALPH_PROMPT}))
    }
}

fn texts_of(request: &ModelRequest) -> Vec<String> {
    conversation_texts(&request.messages)
}

/// Claude Code 2.1.302's most consecutive blocks of a turn's end before the model calls a tool.
const STOP_BLOCK_CAP: usize = 8;

fn always_block(_: &Value) -> MemoryCommandRun {
    answer(json!({"decision": "block", "reason": RALPH_PROMPT}))
}

fn cap_warning(blocks: usize) -> String {
    format!(
        "A hook blocked the turn from ending {blocks} consecutive times — overriding and ending \
         turn. For Stop/SubagentStop hooks, check stop_hook_active in the input and return \
         success while it's true."
    )
}

/// The reasons the runs' records give, in order.
fn run_reasons(observed: &Observed) -> Vec<String> {
    observed
        .hook_runs
        .iter()
        .map(|run| run.reason.clone().unwrap_or_default())
        .collect()
}

/// Only the last record warns, after its own reason, that its block was overridden.
fn assert_only_last_block_overridden(reasons: &[String], blocks: usize) {
    let (last, earlier) = reasons.split_last().unwrap();
    assert_eq!(*last, format!("{RALPH_PROMPT}\n{}", cap_warning(blocks)));
    assert!(
        earlier.iter().all(|reason| reason == RALPH_PROMPT),
        "{earlier:?}"
    );
}

/// As in Claude Code 2.1.302, Stop hooks may block a turn's end 8 consecutive times: the ninth
/// block is overridden with a warning and the turn ends; the next turn counts afresh.
#[tokio::test]
async fn the_ninth_consecutive_stop_block_is_overridden_and_ends_the_turn() {
    let routed = Arc::new(RoutedModel::new(|_| text("Still improving the README.")));
    let bench = Bench::start_with(
        Vec::new(),
        Some(routed),
        vec![user_hook(HookEvent::Stop, None, "ralph.sh")],
        false,
    )
    .await;
    bench.define("ralph.sh", always_block);
    let observed = tokio::time::timeout(Duration::from_secs(20), bench.run(RALPH_PROMPT))
        .await
        .expect("the cap ends the turn");
    assert!(observed.failed.is_none() && !observed.interrupted);
    assert_eq!(bench.inputs_of("ralph.sh").len(), STOP_BLOCK_CAP + 1);
    assert_eq!(
        bench.requests().len(),
        STOP_BLOCK_CAP + 1,
        "the overridden block's reason is never sent"
    );
    assert_only_last_block_overridden(&run_reasons(&observed), STOP_BLOCK_CAP + 1);
    assert_prefix_stable(&bench).await;

    let next = tokio::time::timeout(Duration::from_secs(20), bench.run("Next."))
        .await
        .expect("the cap ends the next turn too");
    assert_eq!(
        bench.inputs_of("ralph.sh").len(),
        2 * (STOP_BLOCK_CAP + 1),
        "a new turn counts afresh"
    );
    assert_only_last_block_overridden(&run_reasons(&next), STOP_BLOCK_CAP + 1);
    bench.agent.shutdown().await;
}

/// Claude Code counts only blocks with no tool call between them: a tool call starts the count
/// again.
#[tokio::test]
async fn a_tool_call_starts_the_count_of_consecutive_stop_blocks_again() {
    const BLOCKS_BEFORE_TOOL: usize = 5;
    let mut replies = vec![text("Still improving the README."); BLOCKS_BEFORE_TOOL];
    replies.push(call("build", "Bash", json!({"command": "cargo build"})));
    replies.extend(vec![
        text("Still improving the README.");
        STOP_BLOCK_CAP + 1
    ]);
    let bench = Bench::start(replies, vec![user_hook(HookEvent::Stop, None, "ralph.sh")]).await;
    bench
        .environment
        .define_shell("cargo build", Duration::ZERO, b"built".to_vec(), 0);
    bench.define("ralph.sh", always_block);
    let observed = tokio::time::timeout(Duration::from_secs(20), bench.run(RALPH_PROMPT))
        .await
        .expect("the cap ends the turn");
    assert!(observed.failed.is_none() && !observed.interrupted);
    assert_eq!(
        bench.inputs_of("ralph.sh").len(),
        BLOCKS_BEFORE_TOOL + STOP_BLOCK_CAP + 1
    );
    assert_eq!(
        bench.requests().len(),
        BLOCKS_BEFORE_TOOL + 1 + STOP_BLOCK_CAP + 1
    );
    assert_only_last_block_overridden(&run_reasons(&observed), STOP_BLOCK_CAP + 1);
    bench.agent.shutdown().await;
}

/// A ralph-style loop whose every turn calls a tool never reaches the cap, however often its
/// Stop hook blocks.
#[tokio::test]
async fn a_loop_calling_a_tool_every_turn_is_never_capped() {
    const ITERATIONS: usize = 2 * STOP_BLOCK_CAP;
    let mut replies = Vec::new();
    for iteration in 0..ITERATIONS {
        replies.push(call(
            &format!("build-{iteration}"),
            "Bash",
            json!({"command": "cargo build"}),
        ));
        replies.push(text("Still improving the README."));
    }
    let bench = Bench::start(replies, vec![user_hook(HookEvent::Stop, None, "ralph.sh")]).await;
    bench
        .environment
        .define_shell("cargo build", Duration::ZERO, b"built".to_vec(), 0);
    let runs = Arc::new(AtomicUsize::new(0));
    let counter = runs.clone();
    bench.define("ralph.sh", move |input| {
        if counter.fetch_add(1, Ordering::SeqCst) + 1 == ITERATIONS {
            exits(0, "", "")
        } else {
            always_block(input)
        }
    });
    let observed = tokio::time::timeout(Duration::from_secs(20), bench.run(RALPH_PROMPT))
        .await
        .expect("the loop ends when its hook lets it");
    assert!(observed.failed.is_none() && !observed.interrupted);
    assert_eq!(runs.load(Ordering::SeqCst), ITERATIONS);
    assert_eq!(bench.requests().len(), 2 * ITERATIONS);
    assert!(
        run_reasons(&observed)
            .iter()
            .all(|reason| !reason.contains("overriding")),
        "{:?}",
        run_reasons(&observed)
    );
    bench.agent.shutdown().await;
}

#[tokio::test]
async fn a_stop_block_takes_in_input_queued_meanwhile_after_its_feedback() {
    const QUEUED: &str = "Also update the changelog.";
    let bench = Bench::start(
        vec![
            text("Improved the README."),
            text("Updated the changelog too."),
        ],
        vec![
            user_hook(HookEvent::Stop, None, "ralph.sh"),
            user_hook(HookEvent::UserPromptSubmit, None, "context.sh"),
        ],
    )
    .await;
    bench.define("ralph.sh", ralph_until_active);
    bench.define("context.sh", |input| {
        context_answer(
            HookEvent::UserPromptSubmit,
            &format!("checked {}", input["prompt"].as_str().unwrap()),
        )
    });
    let run = bench.run_queueing(RALPH_PROMPT, &[QUEUED]).await;
    assert!(run.observed.failed.is_none() && !run.observed.interrupted);
    assert_eq!(
        run.observed.merged,
        [(run.queued[0].run_id, run.receipt.run_id)],
        "the queued input joins the turn the Stop hook keeps going"
    );
    let prompts = bench
        .inputs_of("context.sh")
        .iter()
        .map(|input| input["prompt"].clone())
        .collect::<Vec<_>>();
    assert_eq!(prompts, [json!(RALPH_PROMPT), json!(QUEUED)]);
    let requests = bench.requests();
    assert_eq!(requests.len(), 2, "no turn of its own");
    let trailing = trailing_texts(&requests[1]);
    let feedback = trailing
        .iter()
        .position(|text| text.contains(&format!("Stop hook feedback:\n{RALPH_PROMPT}")))
        .expect("the Stop hook's reason goes on first");
    let queued = trailing
        .iter()
        .position(|text| text == QUEUED)
        .expect("the model reads the queued input in the continuation");
    assert!(feedback < queued, "{trailing:?}");
    assert!(
        trailing[queued + 1].contains(&format!(
            "UserPromptSubmit hook additional context: checked {QUEUED}"
        )),
        "its hooks' context follows it: {trailing:?}"
    );
    assert!(
        bench
            .agent
            .snapshot(bench.chat)
            .await
            .unwrap()
            .queued_inputs()
            .is_empty()
    );
    assert_prefix_stable(&bench).await;
    bench.agent.shutdown().await;
}

#[tokio::test]
async fn a_queued_prompt_a_hook_blocks_is_taken_back_and_the_next_one_joins() {
    const LEAKED: &str = "My password is hunter2.";
    const QUEUED: &str = "Also update the changelog.";
    let bench = Bench::start_with(
        vec![
            text("Improved the README."),
            text("Updated the changelog too."),
        ],
        None,
        vec![
            user_hook(HookEvent::Stop, None, "ralph.sh"),
            user_hook(HookEvent::UserPromptSubmit, None, "secrets.sh"),
        ],
        true,
    )
    .await;
    bench.define("ralph.sh", ralph_until_active);
    bench.define("secrets.sh", |input| {
        if input["prompt"].as_str().unwrap().contains("password") {
            exits(2, "", "prompts must not carry passwords\n")
        } else {
            exits(0, "", "")
        }
    });
    let run = bench.run_queueing(RALPH_PROMPT, &[LEAKED, QUEUED]).await;
    assert!(run.observed.failed.is_none() && !run.observed.interrupted);
    assert_eq!(
        run.observed.merged,
        [(run.queued[1].run_id, run.receipt.run_id)],
        "only the prompt its hooks let through joins"
    );
    assert_eq!(
        run.observed.rewinds, 0,
        "views never showed it, so they do not redraw mid-turn"
    );
    let blocked = run
        .observed
        .hook_runs
        .iter()
        .find(|record| {
            record.event == HookEvent::UserPromptSubmit
                && record.outcome == HookRunOutcome::Blocking
        })
        .expect("the user sees why");
    assert_eq!(
        blocked.reason.as_deref(),
        Some("[secrets.sh]: prompts must not carry passwords")
    );
    let requests = bench.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        requests.iter().all(|request| !texts_of(request)
            .iter()
            .any(|text| text.contains("hunter2"))),
        "the model never reads it"
    );
    let trailing = trailing_texts(&requests[1]);
    assert_eq!(
        trailing.last().map(String::as_str),
        Some(QUEUED),
        "{trailing:?}"
    );
    let snapshot = bench.agent.snapshot(bench.chat).await.unwrap();
    assert!(snapshot.queued_inputs().is_empty());
    let texts = conversation_texts(&snapshot.messages);
    assert!(
        !texts.iter().any(|text| text.contains("hunter2")),
        "{texts:?}"
    );
    assert!(texts.iter().any(|text| text == QUEUED));
    assert_prefix_stable(&bench).await;
    bench.agent.shutdown().await;

    let reloaded = build_agent(
        &bench.directory,
        &bench.environment,
        &bench.settings,
        Arc::new(ScriptedModel::new(Vec::new())),
        true,
    );
    let texts = conversation_texts(&reloaded.resume_chat(bench.chat).await.unwrap().messages);
    assert!(
        !texts.iter().any(|text| text.contains("hunter2")),
        "a load keeps it out too: {texts:?}"
    );
    assert!(texts.iter().any(|text| text == QUEUED));
    assert!(
        texts
            .iter()
            .any(|text| text == "Updated the changelog too.")
    );
    reloaded.shutdown().await;
}

#[tokio::test]
async fn input_merged_after_a_tool_call_runs_its_prompt_hooks() {
    const QUEUED: &str = "Then run the tests.";
    let bench = Bench::start(
        vec![
            call("build", "Bash", json!({"command": "cargo build"})),
            text("Built; the tests are next."),
        ],
        vec![user_hook(HookEvent::UserPromptSubmit, None, "context.sh")],
    )
    .await;
    bench
        .environment
        .define_shell("cargo build", Duration::ZERO, b"built".to_vec(), 0);
    bench.define("context.sh", |input| {
        context_answer(
            HookEvent::UserPromptSubmit,
            &format!("checked {}", input["prompt"].as_str().unwrap()),
        )
    });
    let run = bench.run_queueing("Build it.", &[QUEUED]).await;
    assert!(run.observed.failed.is_none());
    assert_eq!(
        run.observed.merged,
        [(run.queued[0].run_id, run.receipt.run_id)]
    );
    let prompts = bench
        .inputs_of("context.sh")
        .iter()
        .map(|input| input["prompt"].clone())
        .collect::<Vec<_>>();
    assert_eq!(prompts, [json!("Build it."), json!(QUEUED)]);
    let trailing = trailing_texts(&bench.requests()[1]);
    let queued = trailing
        .iter()
        .position(|text| text == QUEUED)
        .expect("the queued input follows the tool's result");
    assert!(
        trailing[queued + 1].contains(&format!(
            "UserPromptSubmit hook additional context: checked {QUEUED}"
        )),
        "{trailing:?}"
    );
    assert_prefix_stable(&bench).await;
    bench.agent.shutdown().await;
}

/// Claude Code leaves a queued prompt whose hook ends its turn queued until the running turn
/// ends; its own turn then keeps it unanswered.
#[tokio::test]
async fn a_queued_prompt_whose_hook_ends_the_turn_waits_for_a_turn_of_its_own() {
    const HELD: &str = "Deploy it.";
    let bench = Bench::start(
        vec![
            call("build", "Bash", json!({"command": "cargo build"})),
            text("Built."),
        ],
        vec![user_hook(HookEvent::UserPromptSubmit, None, "review.sh")],
    )
    .await;
    bench
        .environment
        .define_shell("cargo build", Duration::ZERO, b"built".to_vec(), 0);
    bench.define("review.sh", |input| {
        if input["prompt"] == HELD {
            answer(json!({"continue": false, "stopReason": "Deploys wait for review"}))
        } else {
            exits(0, "", "")
        }
    });
    let mut run = bench.run_queueing("Build it.", &[HELD]).await;
    assert!(run.observed.failed.is_none());
    assert!(
        run.observed.merged.is_empty(),
        "it does not join the running turn"
    );
    let held = observe(&mut run.events, &run.queued[0]).await;
    assert!(held.failed.is_none());
    let requests = bench.requests();
    assert_eq!(requests.len(), 2, "the model is never asked to answer it");
    assert!(
        requests
            .iter()
            .all(|request| !texts_of(request).iter().any(|text| text == HELD))
    );
    let texts = conversation_texts(&bench.agent.snapshot(bench.chat).await.unwrap().messages);
    let kept = texts
        .iter()
        .position(|text| text == HELD)
        .expect("its own turn keeps it");
    assert_eq!(
        texts[kept - 1],
        "Built.",
        "after the turn it waited for: {texts:?}"
    );
    assert!(
        texts[kept + 1].contains("Deploys wait for review"),
        "{texts:?}"
    );
    bench.agent.shutdown().await;
}

fn is_child_request(request: &ModelRequest) -> bool {
    request
        .messages
        .iter()
        .find(|message| tickloop_harness_test_support::is_conversation(message))
        .and_then(|message| message.content.first())
        .is_some_and(|content| {
            matches!(content, Content::Text { text } if text.starts_with("<agent-message>"))
        })
}

#[tokio::test]
async fn subagent_stop_blocks_a_subagents_turn_end_before_its_result_reaches_the_parent() {
    let child_turns = Arc::new(AtomicUsize::new(0));
    let turns = child_turns.clone();
    let routed = Arc::new(RoutedModel::new(move |request| {
        if is_child_request(request) {
            let turn = turns.fetch_add(1, Ordering::SeqCst) + 1;
            text(&format!("child report {turn}"))
        } else if tickloop_harness_test_support::routed::has_tool_result(request, "delegate") {
            text("parent done")
        } else {
            call("delegate", "Subagent", json!({"task": "Check the build"}))
        }
    }));
    let bench = Bench::start_with(
        Vec::new(),
        Some(routed),
        vec![user_hook(HookEvent::SubagentStop, None, "verify.sh")],
        false,
    )
    .await;
    bench.define("verify.sh", |input| {
        if input["stop_hook_active"] == json!(true) {
            exits(0, "", "")
        } else {
            exits(2, "", "Include the build log.")
        }
    });
    let mut events = bench.agent.subscribe();
    bench
        .agent
        .submit(bench.chat, "Delegate the check.".into())
        .await
        .unwrap();
    let reaches_parent = |request: &ModelRequest| {
        !is_child_request(request)
            && conversation_texts(&request.messages)
                .iter()
                .any(|text| text.contains("child report 2"))
    };
    tokio::time::timeout(Duration::from_secs(20), async {
        while !bench.requests().iter().any(reaches_parent) {
            let _ = next_event(&mut events).await;
        }
    })
    .await
    .expect("the parent reads the report written after the block");
    let inputs = bench.inputs_of("verify.sh");
    assert_eq!(inputs.len(), 2);
    assert_eq!(inputs[0]["hook_event_name"], "SubagentStop");
    assert_eq!(inputs[0]["stop_hook_active"], false);
    assert_eq!(inputs[0]["last_assistant_message"], "child report 1");
    assert_eq!(inputs[0]["agent_type"], "general-purpose");
    assert!(inputs[0].get("agent_transcript_path").is_some());
    assert_ne!(inputs[0]["agent_id"], bench.chat.uuid().to_string());
    assert_eq!(inputs[1]["stop_hook_active"], true);
    assert_eq!(child_turns.load(Ordering::SeqCst), 2);
    let child_feedback = bench
        .requests()
        .into_iter()
        .rfind(is_child_request)
        .map(|request| trailing_texts(&request))
        .unwrap();
    assert!(
        child_feedback.iter().any(|text| {
            text.contains("SubagentStop hook feedback:\n[verify.sh]: Include the build log.")
        }),
        "{child_feedback:?}"
    );
    assert!(
        !bench
            .requests()
            .iter()
            .filter(|request| !is_child_request(request))
            .any(|request| conversation_texts(&request.messages)
                .iter()
                .any(|text| text.contains("child report 1"))),
        "the blocked report never reaches the parent"
    );
    bench.agent.shutdown().await;
}

#[tokio::test]
async fn stop_failure_tells_hooks_of_a_model_error_and_stop_does_not_run() {
    let bench = Bench::start(
        Vec::new(),
        vec![
            user_hook(HookEvent::StopFailure, None, "alert.sh"),
            user_hook(HookEvent::Stop, None, "stop.sh"),
        ],
    )
    .await;
    let alerted = Arc::new(tokio::sync::Notify::new());
    let notify = alerted.clone();
    bench.define("alert.sh", move |_| {
        notify.notify_one();
        exits(0, "", "")
    });
    bench.define("stop.sh", |_| exits(0, "", ""));
    let observed = bench.run("Go.").await;
    assert!(observed.failed.is_some());
    tokio::time::timeout(Duration::from_secs(5), alerted.notified())
        .await
        .expect("StopFailure runs");
    let input = &bench.inputs_of("alert.sh")[0];
    assert_eq!(input["hook_event_name"], "StopFailure");
    assert_eq!(input["error"], "invalid_request");
    assert!(
        input["error_details"]
            .as_str()
            .unwrap()
            .contains("script exhausted")
    );
    assert!(bench.inputs_of("stop.sh").is_empty());
    bench.agent.shutdown().await;
}

#[tokio::test]
async fn session_end_runs_when_the_worker_ends_and_a_loaded_chat_resumes_its_session() {
    let bench = Bench::start_with(
        vec![text("before")],
        None,
        vec![
            user_hook(HookEvent::SessionEnd, None, "end.sh"),
            user_hook(HookEvent::SessionStart, None, "start.sh"),
        ],
        true,
    )
    .await;
    bench.define("end.sh", |_| exits(1, "", "cleanup failed"));
    bench.define("start.sh", |_| exits(0, "", ""));
    bench.run("Remember this.").await;
    assert!(bench.inputs_of("end.sh").is_empty(), "the session goes on");
    bench.agent.shutdown().await;
    let ended = bench.inputs_of("end.sh");
    assert_eq!(
        ended.len(),
        1,
        "a failing SessionEnd hook only tells the user"
    );
    assert_eq!(ended[0]["hook_event_name"], "SessionEnd");
    assert_eq!(ended[0]["reason"], "other");
    assert_eq!(ended[0]["session_id"], bench.chat.uuid().to_string());

    let model = Arc::new(ScriptedModel::new(vec![text("after")]));
    let resumed = build_agent(
        &bench.directory,
        &bench.environment,
        &bench.settings,
        model,
        true,
    );
    resumed.resume_chat(bench.chat).await.unwrap();
    let mut events = resumed.subscribe();
    let receipt = resumed
        .submit(bench.chat, "What did I ask?".into())
        .await
        .unwrap();
    observe(&mut events, &receipt).await;
    let sources = bench
        .inputs_of("start.sh")
        .iter()
        .map(|input| input["source"].clone())
        .collect::<Vec<_>>();
    assert_eq!(sources, [json!("startup"), json!("resume")]);
    resumed.shutdown().await;
}

#[tokio::test]
async fn subagent_stop_runs_before_an_auto_subagent_hands_its_report_back() {
    use tickloop_harness_test_support::auto_classification;
    use tickloop_harness_test_support::routed::{has_tool_result, state};
    let routed = Arc::new(RoutedModel::new(|request| {
        if !state(request)["delegation"].is_null() {
            if has_tool_result(request, "first-report") {
                call(
                    "second-report",
                    "SubagentHandback",
                    json!({"message": "report with the build log"}),
                )
            } else {
                call(
                    "first-report",
                    "SubagentHandback",
                    json!({"message": "report without the log"}),
                )
            }
        } else if has_tool_result(request, "delegate") {
            text("parent done")
        } else {
            call("delegate", "Subagent", json!({"task": "Check the build"}))
        }
    }));
    let bench = Bench::start_with(
        Vec::new(),
        Some(routed),
        vec![user_hook(HookEvent::SubagentStop, None, "verify.sh")],
        false,
    )
    .await;
    bench
        .agent
        .set_auto_reviewer(Arc::new(RoutedModel::new(|_| {
            text(&auto_classification(false).to_string())
        })))
        .await;
    bench
        .agent
        .set_execution_mode(bench.chat, ExecutionMode::Auto)
        .await
        .unwrap();
    bench.define("verify.sh", |input| {
        if input["stop_hook_active"] == json!(true) {
            exits(0, "", "")
        } else {
            answer(json!({"decision": "block", "reason": "Attach the build log."}))
        }
    });
    let mut events = bench.agent.subscribe();
    bench
        .agent
        .submit(bench.chat, "Delegate the check.".into())
        .await
        .unwrap();
    let reaches_parent = |request: &ModelRequest| {
        state(request)["delegation"].is_null()
            && format!("{:?}", request.messages).contains("report with the build log")
    };
    tokio::time::timeout(Duration::from_secs(20), async {
        while !bench.requests().iter().any(reaches_parent) {
            let _ = next_event(&mut events).await;
        }
    })
    .await
    .expect("the parent reads the report sent after the block");
    let inputs = bench.inputs_of("verify.sh");
    assert_eq!(inputs.len(), 2);
    assert_eq!(inputs[0]["hook_event_name"], "SubagentStop");
    assert_eq!(inputs[1]["stop_hook_active"], true);
    let first_result = bench
        .requests()
        .into_iter()
        .filter(|request| !state(request)["delegation"].is_null())
        .flat_map(|request| request.messages)
        .flat_map(|message| message.content)
        .find_map(|content| match content {
            Content::ToolResult {
                tool_use_id,
                content,
                ..
            } if tool_use_id == "first-report" => Some(content.text().into_owned()),
            _ => None,
        });
    assert_eq!(
        first_result.as_deref(),
        Some("SubagentStop hook feedback:\nAttach the build log.")
    );
    assert!(
        !bench
            .requests()
            .iter()
            .filter(|request| state(request)["delegation"].is_null())
            .any(|request| format!("{:?}", request.messages).contains("report without the log")),
        "the blocked report never reaches the parent"
    );
    bench.agent.shutdown().await;
}

/// An Auto Subagent's hand-back is no tool call that starts the count again: SubagentStop may
/// block it 8 consecutive times, and the ninth block is overridden so the report goes back.
#[tokio::test]
async fn the_ninth_consecutive_subagent_stop_block_lets_the_report_go_back() {
    use tickloop_harness_test_support::auto_classification;
    use tickloop_harness_test_support::routed::{has_tool_result, state};
    let routed = Arc::new(RoutedModel::new(|request| {
        if !state(request)["delegation"].is_null() {
            call(
                &format!("report-{}", request.messages.len()),
                "SubagentHandback",
                json!({"message": "the build report"}),
            )
        } else if has_tool_result(request, "delegate") {
            text("parent done")
        } else {
            call("delegate", "Subagent", json!({"task": "Check the build"}))
        }
    }));
    let bench = Bench::start_with(
        Vec::new(),
        Some(routed),
        vec![user_hook(HookEvent::SubagentStop, None, "verify.sh")],
        false,
    )
    .await;
    bench
        .agent
        .set_auto_reviewer(Arc::new(RoutedModel::new(|_| {
            text(&auto_classification(false).to_string())
        })))
        .await;
    bench
        .agent
        .set_execution_mode(bench.chat, ExecutionMode::Auto)
        .await
        .unwrap();
    bench.define("verify.sh", |_| {
        answer(json!({"decision": "block", "reason": RALPH_PROMPT}))
    });
    let mut events = bench.agent.subscribe();
    bench
        .agent
        .submit(bench.chat, "Delegate the check.".into())
        .await
        .unwrap();
    let reaches_parent = |request: &ModelRequest| {
        state(request)["delegation"].is_null()
            && format!("{:?}", request.messages).contains("the build report")
    };
    let mut records = Vec::new();
    tokio::time::timeout(Duration::from_secs(20), async {
        while !bench.requests().iter().any(reaches_parent) {
            if let EventKind::HookRan { record } = next_event(&mut events).await.kind {
                records.push(*record);
            }
        }
    })
    .await
    .expect("the parent reads the report once the block is overridden");
    assert_eq!(bench.inputs_of("verify.sh").len(), STOP_BLOCK_CAP + 1);
    let reasons = records
        .iter()
        .map(|record| record.reason.clone().unwrap_or_default())
        .collect::<Vec<_>>();
    assert_only_last_block_overridden(&reasons, STOP_BLOCK_CAP + 1);
    bench.agent.shutdown().await;
}

/// Claude Code queues an `asyncRewake` notification as a Stop hook's feedback, so the turn it
/// starts reads `stop_hook_active: true` at its end.
#[tokio::test]
async fn a_turn_an_async_rewake_hook_starts_tells_stop_hooks_they_are_active() {
    let mut review = user_hook(HookEvent::PostToolUse, Some("Bash"), "review.sh");
    review.background = Some(
        tickloop_harness_protocol::hooks::HookBackground::AsyncRewake {
            message: None,
            summary: None,
        },
    );
    let bench = Bench::start(
        vec![
            call("build", "Bash", json!({"command": "cargo build"})),
            text("built"),
            text("moved the token"),
        ],
        vec![review, user_hook(HookEvent::Stop, None, "check.sh")],
    )
    .await;
    bench
        .environment
        .define_shell("cargo build", Duration::ZERO, b"built".to_vec(), 0);
    bench
        .environment
        .define_command("review.sh", move |_| MemoryCommandRun {
            duration: Duration::from_millis(300),
            ..exits(2, "", "hardcoded token in config.py")
        });
    bench.define("check.sh", |_| exits(0, "", ""));
    let mut events = bench.agent.subscribe();
    let receipt = bench
        .agent
        .submit(bench.chat, "Build it.".into())
        .await
        .unwrap();
    observe(&mut events, &receipt).await;
    tokio::time::timeout(Duration::from_secs(20), async {
        while bench.inputs_of("check.sh").len() < 2 {
            let _ = next_event(&mut events).await;
        }
    })
    .await
    .expect("the rewake starts a turn whose end runs the Stop hook");
    let inputs = bench.inputs_of("check.sh");
    assert_eq!(inputs[0]["stop_hook_active"], false);
    assert_eq!(inputs[1]["stop_hook_active"], true);
    assert_eq!(inputs[1]["last_assistant_message"], "moved the token");
    bench.agent.shutdown().await;
}

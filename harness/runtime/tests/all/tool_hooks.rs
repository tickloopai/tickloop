//! Claude Code–compatible `PreToolUse`, `PostToolUse` and `PostToolUseFailure` hooks in the tool
//! pipeline, run through the reference Execution Environment's one-shot commands.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tickloop_harness_policy::settings::{MemorySettingsStore, SettingsStore, SettingsUpdate};
use tickloop_harness_protocol::hooks::{
    ConfiguredHook, HookDecision, HookEvent, HookList, HookPluginDirectories, HookRunOutcome,
    HookRunRecord, HookSkipNotice, HookSource, HookTrust,
};
use tickloop_harness_protocol::{
    AgentHostOptions, ApprovalPath, ApprovalRecord, ChatId, Content, Decision,
    DirectApprovalReason, EventKind, ExecutionMode, PermissionApprovalSource, PermissionRequest,
    PermissionRule, PromptPolicy, Role, ToolResult,
};
use tickloop_harness_protocol::{HostEndpointIdentity, HostToolAuthority};
use tickloop_harness_runtime::environment::ExecutionTools;
use tickloop_harness_runtime::environment::memory::{
    MemoryCommandRun, MemoryEnvironment, counter_environment,
};
use tickloop_harness_runtime::transcript::{JsonlTranscriptStore, TranscriptStore};
use tickloop_harness_runtime::{Agent, HarnessOptions, HarnessSeams};
use tickloop_harness_test_support::harness::ChosenModels;
use tickloop_harness_test_support::routed::{RoutedModel, has_tool_result};
use tickloop_harness_test_support::{
    ScriptedModel, auto_classification, call, config, next_event, text,
};

const BUILD: &str = "cargo build";
const SAFE_BUILD: &str = "cargo check";
const GUARD: &str = "guard.sh";
const PLUGIN: &str = "guard@market";
/// What the model reads for a call an interruption stopped before it was admitted.
const NOT_EXECUTED: &str = "Interrupted before tool admission; the operation was not executed.";

fn hook(
    source: HookSource,
    event: HookEvent,
    matcher: Option<&str>,
    command: &str,
) -> ConfiguredHook {
    let plugin = matches!(source, HookSource::Plugin { .. }).then(|| HookPluginDirectories {
        root: "/plugins/guard".into(),
        data: "/plugin-data/guard".into(),
    });
    ConfiguredHook {
        key: format!("{}:{}:{command}", event.name(), matcher.unwrap_or_default()),
        hash: (source != HookSource::User).then(|| "hash".into()),
        source,
        file: "/settings.json".into(),
        event,
        matcher: matcher.map(Into::into),
        command: command.into(),
        resolved_command: Some(command.into()),
        sensitive_values: Vec::new(),
        condition: None,
        timeout_secs: 600,
        background: None,
        plugin,
        trust: HookTrust::Trusted,
    }
}

fn user_hook(event: HookEvent, matcher: Option<&str>, command: &str) -> ConfiguredHook {
    hook(HookSource::User, event, matcher, command)
}

fn plugin_hook(event: HookEvent, matcher: Option<&str>, command: &str) -> ConfiguredHook {
    hook(
        HookSource::Plugin { id: PLUGIN.into() },
        event,
        matcher,
        command,
    )
}

fn answer(stdout: Value) -> MemoryCommandRun {
    exits(0, &stdout.to_string(), "")
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

fn pre_tool_answer(fields: Value) -> MemoryCommandRun {
    let mut specific = json!({"hookEventName": "PreToolUse"});
    specific
        .as_object_mut()
        .unwrap()
        .extend(fields.as_object().unwrap().clone());
    answer(json!({ "hookSpecificOutput": specific }))
}

/// A hook command's name, the input it read and the variables it got.
type RecordedInput = (String, Value, std::collections::BTreeMap<String, String>);

/// One Agent with a Folder, the reference environment and hooks from the Settings Store.
struct Bench {
    agent: Agent,
    chat: ChatId,
    environment: Arc<MemoryEnvironment>,
    model: Arc<ScriptedModel>,
    reviewer: Arc<ScriptedModel>,
    hook_inputs: Arc<Mutex<Vec<RecordedInput>>>,
    folder: std::path::PathBuf,
    _directory: tempfile::TempDir,
}

struct Options {
    mode: ExecutionMode,
    prompt_policy: PromptPolicy,
    replies: Vec<Vec<Content>>,
    /// Answers in place of the scripted replies.
    routed: Option<Arc<RoutedModel>>,
    /// Keeps the Chat in a local Transcript store, so hooks get a transcript path.
    keeps_transcripts: bool,
    /// Offers the environment's Tick tools, with the counter Tick installed.
    offers_ticks: bool,
}

impl Options {
    fn new(mode: ExecutionMode, replies: Vec<Vec<Content>>) -> Self {
        Self {
            mode,
            prompt_policy: PromptPolicy::Interactive,
            replies,
            routed: None,
            keeps_transcripts: false,
            offers_ticks: false,
        }
    }
}

impl Bench {
    async fn start(options: Options, hooks: Vec<ConfiguredHook>) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let folder = directory.path().canonicalize().unwrap();
        std::fs::write(folder.join("notes.txt"), "hello").unwrap();
        let environment = Arc::new(if options.offers_ticks {
            counter_environment()
        } else {
            MemoryEnvironment::new()
        });
        environment.define_shell(BUILD, Duration::ZERO, b"built".to_vec(), 0);
        environment.define_shell(SAFE_BUILD, Duration::ZERO, b"checked".to_vec(), 0);
        environment.define_shell("false", Duration::ZERO, b"failed".to_vec(), 1);
        let settings = Arc::new(MemorySettingsStore::default());
        settings
            .write(SettingsUpdate::TrustDirectory(folder.clone()))
            .unwrap();
        settings.set_hooks(HookList {
            hooks,
            ..Default::default()
        });
        let model = Arc::new(ScriptedModel::new(options.replies));
        let reviewer = Arc::new(ScriptedModel::new(vec![
            text(&auto_classification(false).to_string()),
            text(&auto_classification(false).to_string()),
        ]));
        let agent = Agent::with_seams(
            HarnessSeams {
                search: None,
                transcripts: options.keeps_transcripts.then(|| {
                    Arc::new(
                        JsonlTranscriptStore::open(directory.path().join("transcripts")).unwrap(),
                    ) as Arc<dyn TranscriptStore>
                }),
                environment: Some(environment.clone()),
                models: ChosenModels::new(match &options.routed {
                    Some(routed) => routed.clone(),
                    None => model.clone(),
                }),
            },
            HarnessOptions {
                settings: Some(settings),
                host: AgentHostOptions {
                    prompt_policy: options.prompt_policy,
                    ..AgentHostOptions::embedded_defaults()
                },
                ..HarnessOptions::local(config())
            },
        )
        .unwrap();
        agent.set_auto_reviewer(reviewer.clone()).await;
        if options.offers_ticks {
            agent
                .install_host_tools(Arc::new(ExecutionTools::new(
                    environment.clone(),
                    HostToolAuthority::Native {
                        endpoint: HostEndpointIdentity::TrustedAdapter {
                            scope: tickloop_ids::generate(),
                        },
                    },
                )))
                .unwrap();
        }
        let chat = agent.create_chat(folder.clone()).await.unwrap();
        agent.set_execution_mode(chat, options.mode).await.unwrap();
        Self {
            agent,
            chat,
            environment,
            model,
            reviewer,
            hook_inputs: Arc::default(),
            folder,
            _directory: directory,
        }
    }

    /// Scripts `command` and remembers each input it reads and the variables it gets.
    fn define(
        &self,
        command: &str,
        run: impl Fn(&Value) -> MemoryCommandRun + Send + Sync + 'static,
    ) {
        let inputs = self.hook_inputs.clone();
        let name = command.to_owned();
        self.environment.define_command(command, move |spec| {
            let input: Value = serde_json::from_slice(&spec.stdin).unwrap();
            assert_eq!(spec.timeout, Duration::from_secs(600));
            inputs
                .lock()
                .unwrap()
                .push((name.clone(), input.clone(), spec.env.clone()));
            run(&input)
        });
    }

    /// The variables the first hook that ran got.
    fn first_environment(&self) -> std::collections::BTreeMap<String, String> {
        self.hook_inputs.lock().unwrap()[0].2.clone()
    }

    fn inputs_of(&self, command: &str) -> Vec<Value> {
        self.hook_inputs
            .lock()
            .unwrap()
            .iter()
            .filter(|(name, ..)| name == command)
            .map(|(_, input, _)| input.clone())
            .collect()
    }

    async fn run(&self, prompt: &str, allow: bool) -> Observed {
        let mut events = self.agent.subscribe();
        let receipt = self.agent.submit(self.chat, prompt.into()).await.unwrap();
        let mut observed = Observed::default();
        loop {
            let event = next_event(&mut events).await;
            if event.run_id != receipt.run_id {
                continue;
            }
            match event.kind {
                EventKind::PermissionRequested { request } => {
                    self.agent
                        .answer_permission(self.chat, request.id, allow)
                        .await
                        .unwrap();
                    observed.requests.push(request);
                }
                EventKind::ApprovalRecorded { record } => observed.approvals.push(*record),
                EventKind::ToolStarted { name, input, .. } => observed.started.push((name, input)),
                EventKind::ToolCompleted { result, .. } => observed.results.push(result),
                EventKind::HookRan { record } => observed.hook_runs.push(*record),
                EventKind::HooksSkipped { notice } => observed.skipped.push(notice),
                EventKind::Completed { .. } => break,
                EventKind::Interrupted => {
                    observed.interrupted = true;
                    break;
                }
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
        observed
    }

    /// The results of tool calls as the model read them in its last request.
    fn model_tool_results(&self) -> Vec<String> {
        let requests = self.model.requests.lock().unwrap();
        requests
            .last()
            .unwrap()
            .messages
            .iter()
            .filter(|message| message.role == Role::User)
            .flat_map(|message| &message.content)
            .filter_map(|content| match content {
                Content::ToolResult { content, .. } => Some(content.text().into_owned()),
                _ => None,
            })
            .collect()
    }

    fn reviewed(&self) -> bool {
        !self.reviewer.requests.lock().unwrap().is_empty()
    }
}

#[derive(Default)]
struct Observed {
    requests: Vec<PermissionRequest>,
    approvals: Vec<ApprovalRecord>,
    started: Vec<(String, Value)>,
    results: Vec<ToolResult>,
    hook_runs: Vec<HookRunRecord>,
    skipped: Vec<HookSkipNotice>,
    interrupted: bool,
}

fn build_call() -> Vec<Content> {
    call("build", "Bash", json!({"command": BUILD}))
}

#[tokio::test]
async fn pre_tool_use_reads_claude_code_input_and_its_exit_two_refuses_the_call_unprepared() {
    let bench = Bench::start(
        Options::new(ExecutionMode::AcceptEdits, vec![build_call(), text("done")]),
        vec![user_hook(HookEvent::PreToolUse, Some("Bash"), GUARD)],
    )
    .await;
    bench.define(GUARD, |_| exits(2, "", "builds are frozen\n"));
    let observed = bench.run("Build it.", true).await;

    assert!(observed.started.is_empty(), "the refused call never runs");
    assert!(
        observed.requests.is_empty(),
        "a refused call is not asked about"
    );
    let result = &observed.results[0];
    assert!(result.is_error);
    assert_eq!(
        result.content,
        "PreToolUse:Bash hook error: [guard.sh]: builds are frozen"
    );
    let input = &bench.inputs_of(GUARD)[0];
    assert_eq!(input["hook_event_name"], "PreToolUse");
    assert_eq!(input["tool_name"], "Bash");
    assert_eq!(input["tool_input"], json!({"command": BUILD}));
    assert_eq!(input["tool_use_id"], "build");
    assert_eq!(input["session_id"], bench.chat.uuid().to_string());
    assert_eq!(input["cwd"], bench.folder.to_string_lossy().as_ref());
    assert_eq!(input["permission_mode"], "acceptEdits");
    assert_eq!(input["transcript_path"], "");
    assert!(input.get("agent_id").is_none());
    let env = bench.first_environment();
    assert_eq!(
        env.keys().collect::<Vec<_>>(),
        ["CLAUDE_PROJECT_DIR"],
        "a user hook gets only the project directory"
    );
    assert_eq!(env["CLAUDE_PROJECT_DIR"], bench.folder.to_string_lossy());
    let run = &observed.hook_runs[0];
    assert_eq!(run.outcome, HookRunOutcome::Blocking);
    assert_eq!(run.decision, Some(HookDecision::Deny));
    assert_eq!(run.exit_code, Some(2));
    assert_eq!(run.tool_name.as_deref(), Some("Bash"));
    let history = bench.agent.approval_history_view(bench.chat).await.unwrap();
    assert_eq!(history.hook_runs, observed.hook_runs);
    bench.agent.shutdown().await;
}

#[tokio::test]
async fn a_plugin_hook_may_deny_and_gets_its_directories_but_its_allow_admits_nothing() {
    let bench = Bench::start(
        Options::new(ExecutionMode::AcceptEdits, vec![build_call(), text("done")]),
        vec![plugin_hook(HookEvent::PreToolUse, None, GUARD)],
    )
    .await;
    bench.define(GUARD, |_| answer(json!({"decision": "approve"})));
    let observed = bench.run("Build it.", true).await;
    assert_eq!(observed.requests.len(), 1, "a plugin's allow still asks");
    assert_eq!(
        observed.hook_runs[0].decision,
        Some(HookDecision::IgnoredAllow)
    );
    let env = bench.first_environment();
    for (name, value) in [
        ("TICKLOOP_PLUGIN_ROOT", "/plugins/guard"),
        ("CLAUDE_PLUGIN_ROOT", "/plugins/guard"),
        ("TICKLOOP_PLUGIN_DATA", "/plugin-data/guard"),
        ("CLAUDE_PLUGIN_DATA", "/plugin-data/guard"),
    ] {
        assert_eq!(env[name], value);
    }
    assert_eq!(env.len(), 5);
    bench.agent.shutdown().await;

    let bench = Bench::start(
        Options::new(ExecutionMode::FullAccess, vec![build_call(), text("done")]),
        vec![plugin_hook(HookEvent::PreToolUse, None, GUARD)],
    )
    .await;
    bench.define(GUARD, |_| {
        pre_tool_answer(
            json!({"permissionDecision": "deny", "permissionDecisionReason": "use make"}),
        )
    });
    let observed = bench.run("Build it.", true).await;
    assert!(observed.started.is_empty());
    assert_eq!(
        observed.results[0].content,
        "PreToolUse:Bash hook error: use make"
    );
    assert_eq!(observed.hook_runs[0].decision, Some(HookDecision::Deny));
    bench.agent.shutdown().await;
}

#[tokio::test]
async fn an_ask_reaches_a_person_even_in_auto_and_a_host_that_cannot_ask_refuses() {
    let read = |bench: &Bench| {
        bench.model.enqueue(call(
            "read",
            "Read",
            json!({"file_path": bench.folder.join("notes.txt")}),
        ));
        bench.model.enqueue(text("done"));
    };
    let bench = Bench::start(
        Options::new(ExecutionMode::Auto, Vec::new()),
        vec![plugin_hook(HookEvent::PreToolUse, Some("Read"), GUARD)],
    )
    .await;
    read(&bench);
    bench.define(GUARD, |_| {
        pre_tool_answer(json!({"permissionDecision": "ask", "permissionDecisionReason": "Confirm reading notes"}))
    });
    let observed = bench.run("Read the notes.", true).await;
    assert_eq!(observed.requests.len(), 1);
    let request = &observed.requests[0];
    assert_eq!(request.approval_source, PermissionApprovalSource::Hook);
    assert_eq!(
        request.approval_reason.as_deref(),
        Some("Confirm reading notes")
    );
    assert!(!bench.reviewed(), "an ask skips the Auto reviewer");
    assert!(!observed.results[0].is_error);
    assert_eq!(observed.approvals[0].path, ApprovalPath::Human);
    bench.agent.shutdown().await;

    let bench = Bench::start(
        Options {
            prompt_policy: PromptPolicy::DenyOnPrompt,
            ..Options::new(ExecutionMode::Auto, Vec::new())
        },
        vec![user_hook(HookEvent::PreToolUse, Some("Read"), GUARD)],
    )
    .await;
    read(&bench);
    bench.define(GUARD, |_| {
        pre_tool_answer(json!({"permissionDecision": "ask"}))
    });
    let observed = bench.run("Read the notes.", true).await;
    assert!(observed.requests.is_empty());
    assert!(observed.results[0].is_error);
    assert!(observed.started.is_empty());
    bench.agent.shutdown().await;
}

#[tokio::test]
async fn a_user_hooks_allow_admits_after_the_rules_and_before_auto_review() {
    for mode in [ExecutionMode::AcceptEdits, ExecutionMode::Auto] {
        let bench = Bench::start(
            Options::new(mode, vec![build_call(), text("done")]),
            vec![user_hook(HookEvent::PreToolUse, Some("Bash"), GUARD)],
        )
        .await;
        bench.define(GUARD, |_| {
            pre_tool_answer(json!({"permissionDecision": "allow", "permissionDecisionReason": "builds are fine"}))
        });
        let observed = bench.run("Build it.", false).await;
        assert!(observed.requests.is_empty(), "{mode:?}");
        assert!(!bench.reviewed(), "{mode:?}");
        assert_eq!(observed.results[0].content, "built", "{mode:?}");
        let approval = &observed.approvals[0];
        assert_eq!(approval.path, ApprovalPath::Direct);
        assert_eq!(approval.direct_reason, Some(DirectApprovalReason::Hook));
        assert_eq!(observed.hook_runs[0].decision, Some(HookDecision::Allow));
        bench.agent.shutdown().await;
    }
}

#[tokio::test]
async fn explicit_ask_and_deny_rules_win_over_a_users_allow() {
    for (decision, asks) in [(Decision::Ask, true), (Decision::Deny, false)] {
        let bench = Bench::start(
            Options::new(ExecutionMode::Auto, vec![build_call(), text("done")]),
            vec![user_hook(HookEvent::PreToolUse, Some("Bash"), GUARD)],
        )
        .await;
        bench
            .agent
            .set_permission_rules(
                bench.chat,
                vec![PermissionRule {
                    id: tickloop_ids::generate(),
                    decision,
                    tool: "Bash".into(),
                    specifier: None,
                    input: None,
                    loop_id: None,
                    persistent: false,
                }],
            )
            .await
            .unwrap();
        bench.define(GUARD, |_| answer(json!({"decision": "approve"})));
        let observed = bench.run("Build it.", false).await;
        assert_eq!(observed.requests.len(), usize::from(asks), "{decision:?}");
        assert!(observed.started.is_empty(), "{decision:?}");
        assert!(observed.results[0].is_error, "{decision:?}");
        bench.agent.shutdown().await;
    }
}

#[tokio::test]
async fn updated_input_is_what_the_call_is_prepared_asked_about_and_run_with() {
    let bench = Bench::start(
        Options::new(ExecutionMode::AcceptEdits, vec![build_call(), text("done")]),
        vec![plugin_hook(HookEvent::PreToolUse, Some("Bash"), GUARD)],
    )
    .await;
    bench.define(GUARD, |_| {
        pre_tool_answer(json!({"updatedInput": {"command": SAFE_BUILD}}))
    });
    let observed = bench.run("Build it.", true).await;
    assert_eq!(observed.requests.len(), 1);
    assert_eq!(observed.requests[0].input["command"], SAFE_BUILD);
    assert_eq!(observed.started.len(), 1);
    assert_eq!(observed.started[0].1["command"], SAFE_BUILD);
    assert_eq!(observed.results[0].content, "checked");
    let record = &observed.hook_runs[0];
    assert!(
        record
            .updated_input
            .as_deref()
            .unwrap()
            .contains(SAFE_BUILD)
    );
    let snapshot = bench.agent.snapshot(bench.chat).await.unwrap();
    let original = snapshot
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .find_map(|content| match content {
            Content::ToolUse { input, .. } => Some(input.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        original,
        json!({"command": BUILD}),
        "the conversation keeps the call as made"
    );
    bench.agent.shutdown().await;
}

#[tokio::test]
async fn post_tool_use_feedback_follows_the_result_and_failure_hooks_only_add_context() {
    let bench = Bench::start(
        Options::new(
            ExecutionMode::FullAccess,
            vec![
                build_call(),
                call("fail", "Bash", json!({"command": "false"})),
                text("done"),
            ],
        ),
        vec![
            user_hook(HookEvent::PostToolUse, Some("Bash"), "format.sh"),
            plugin_hook(HookEvent::PostToolUseFailure, Some("Bash"), "explain.sh"),
        ],
    )
    .await;
    bench.define("format.sh", |_| {
        answer(json!({
            "decision": "block",
            "reason": "lint failed",
            "systemMessage": "Formatter complained",
            "hookSpecificOutput": {"hookEventName": "PostToolUse", "additionalContext": "run cargo fmt"},
        }))
    });
    bench.define("explain.sh", |_| {
        answer(json!({
            "decision": "block",
            "reason": "ignored for failures",
            "hookSpecificOutput": {"hookEventName": "PostToolUseFailure", "additionalContext": "check the exit code"},
        }))
    });
    let observed = bench.run("Build it.", true).await;
    assert_eq!(
        observed.results[0].content, "built",
        "the shown result is the tool's own"
    );
    let results = bench.model_tool_results();
    assert!(results[0].starts_with("built"));
    assert!(results[0].contains("PostToolUse:Bash hook blocking error: lint failed"));
    assert!(results[0].contains("PostToolUse:Bash hook additional context: run cargo fmt"));
    assert!(
        results[1].contains("PostToolUseFailure:Bash hook additional context: check the exit code")
    );
    assert!(!results[1].contains("ignored for failures"));
    let success = &bench.inputs_of("format.sh")[0];
    assert_eq!(success["tool_response"], "built");
    let failure = &bench.inputs_of("explain.sh")[0];
    assert_eq!(failure["hook_event_name"], "PostToolUseFailure");
    assert!(failure["error"].as_str().unwrap().contains("failed"));
    assert_eq!(
        bench.inputs_of("format.sh").len(),
        1,
        "a failure runs no PostToolUse"
    );
    assert_eq!(
        observed.hook_runs[0].system_message.as_deref(),
        Some("Formatter complained")
    );
    bench.agent.shutdown().await;
}

#[tokio::test]
async fn continue_false_ends_the_turn_after_the_batch() {
    let bench = Bench::start(
        Options::new(
            ExecutionMode::FullAccess,
            vec![build_call(), text("never asked")],
        ),
        vec![user_hook(HookEvent::PostToolUse, None, "stop.sh")],
    )
    .await;
    bench.define("stop.sh", |_| {
        answer(json!({"continue": false, "stopReason": "Enough for today"}))
    });
    let observed = bench.run("Build it.", true).await;
    assert!(!observed.interrupted);
    assert_eq!(bench.model.requests.lock().unwrap().len(), 1);
    assert_eq!(
        observed.hook_runs[0].reason.as_deref(),
        Some("Enough for today")
    );
    bench.agent.shutdown().await;
}

#[tokio::test]
async fn unconfirmed_hooks_are_skipped_with_one_notice_and_nothing_waits() {
    let mut pending = plugin_hook(HookEvent::PreToolUse, None, GUARD);
    pending.trust = HookTrust::Pending;
    let mut modified = hook(
        HookSource::Repository,
        HookEvent::PreToolUse,
        None,
        "repo.sh",
    );
    modified.trust = HookTrust::Modified;
    let mut disabled = user_hook(HookEvent::PreToolUse, None, "off.sh");
    disabled.trust = HookTrust::Disabled;
    for prompt_policy in [PromptPolicy::Interactive, PromptPolicy::DenyOnPrompt] {
        let bench = Bench::start(
            Options {
                prompt_policy,
                ..Options::new(
                    ExecutionMode::FullAccess,
                    vec![build_call(), build_call_again(), text("done")],
                )
            },
            vec![pending.clone(), modified.clone(), disabled.clone()],
        )
        .await;
        for command in [GUARD, "repo.sh", "off.sh"] {
            bench.define(command, |_| exits(2, "", "must not run"));
        }
        let observed = bench.run("Build twice.", true).await;
        assert!(bench.hook_inputs.lock().unwrap().is_empty());
        assert_eq!(observed.results.len(), 2);
        assert!(observed.results.iter().all(|result| !result.is_error));
        assert_eq!(
            observed.skipped.len(),
            1,
            "announced once per hook and Chat"
        );
        assert_eq!(observed.skipped[0].unconfirmed, 2);
        assert!(observed.hook_runs.is_empty());
        bench.agent.shutdown().await;
    }
}

fn build_call_again() -> Vec<Content> {
    call("again", "Bash", json!({"command": BUILD}))
}

#[tokio::test]
async fn hooks_run_in_parallel_once_per_command_and_source() {
    let bench = Bench::start(
        Options::new(ExecutionMode::FullAccess, vec![build_call(), text("done")]),
        vec![
            user_hook(HookEvent::PreToolUse, Some("Bash"), "slow-a"),
            user_hook(HookEvent::PreToolUse, Some("Ba.*"), "slow-a"),
            user_hook(HookEvent::PreToolUse, None, "slow-b"),
            plugin_hook(HookEvent::PreToolUse, None, "slow-a"),
            user_hook(HookEvent::PreToolUse, Some("Read"), "unmatched"),
        ],
    )
    .await;
    let counts = Arc::new(AtomicUsize::new(0));
    for command in ["slow-a", "slow-b", "unmatched"] {
        let counts = counts.clone();
        bench.define(command, move |_| {
            counts.fetch_add(1, Ordering::SeqCst);
            MemoryCommandRun {
                duration: Duration::from_millis(400),
                ..exits(0, "", "")
            }
        });
    }
    let started = Instant::now();
    let observed = bench.run("Build it.", true).await;
    assert!(
        started.elapsed() < Duration::from_millis(1_100),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(
        counts.load(Ordering::SeqCst),
        3,
        "user slow-a once, slow-b, plugin slow-a"
    );
    assert!(bench.inputs_of("unmatched").is_empty());
    assert_eq!(observed.results[0].content, "built");
    bench.agent.shutdown().await;
}

#[tokio::test]
async fn timeouts_and_other_statuses_only_warn() {
    let mut slow = user_hook(HookEvent::PreToolUse, None, "slow");
    slow.timeout_secs = 1;
    let bench = Bench::start(
        Options::new(ExecutionMode::FullAccess, vec![build_call(), text("done")]),
        vec![slow, user_hook(HookEvent::PreToolUse, None, "broken")],
    )
    .await;
    bench
        .environment
        .define_command("slow", |_| MemoryCommandRun {
            duration: Duration::from_secs(30),
            ..exits(0, "", "")
        });
    bench.define("broken", |_| exits(1, "", "segfault"));
    let observed = bench.run("Build it.", true).await;
    assert_eq!(observed.results[0].content, "built");
    let outcomes = observed
        .hook_runs
        .iter()
        .map(|run| run.outcome)
        .collect::<Vec<_>>();
    assert!(outcomes.contains(&HookRunOutcome::TimedOut));
    assert!(outcomes.contains(&HookRunOutcome::NonBlockingError));
    bench.agent.shutdown().await;
}

#[tokio::test]
async fn esc_and_stop_end_a_running_pre_tool_use_hook_and_the_call_never_runs() {
    for stops in [false, true] {
        let bench = Bench::start(
            Options::new(
                ExecutionMode::FullAccess,
                vec![build_call(), text("not built")],
            ),
            vec![user_hook(HookEvent::PreToolUse, None, "hang")],
        )
        .await;
        let started = Arc::new(tokio::sync::Notify::new());
        let notify = started.clone();
        bench.define("hang", move |_| {
            notify.notify_one();
            MemoryCommandRun {
                duration: Duration::from_secs(60),
                ..exits(0, "", "")
            }
        });
        let agent = bench.agent.clone();
        let chat = bench.chat;
        let interrupter = tokio::spawn(async move {
            started.notified().await;
            tokio::time::sleep(Duration::from_millis(50)).await;
            if stops {
                agent.stop(chat).await.unwrap();
            } else {
                agent.interrupt(chat).await.unwrap();
            }
        });
        let observed = tokio::time::timeout(Duration::from_secs(10), bench.run("Build it.", true))
            .await
            .expect("the hook ends at once");
        interrupter.await.unwrap();
        assert!(observed.interrupted, "stop={stops}");
        assert!(observed.started.is_empty(), "stop={stops}");
        assert_eq!(observed.hook_runs[0].outcome, HookRunOutcome::Cancelled);
        assert_eq!(
            observed
                .results
                .iter()
                .map(|result| result.content.as_str())
                .collect::<Vec<_>>(),
            [NOT_EXECUTED],
            "the interface shows the call as not run, stop={stops}"
        );
        bench.run("Did it build?", true).await;
        let results = bench.model_tool_results();
        assert_eq!(results, [NOT_EXECUTED], "stop={stops}");
        bench.agent.shutdown().await;
    }
}

#[tokio::test]
async fn agent_and_task_matchers_select_the_subagent_tool() {
    for matcher in ["Agent", "Task"] {
        let bench = Bench::start(
            Options::new(
                ExecutionMode::FullAccess,
                vec![
                    call("delegate", "Subagent", json!({"task": "Look around"})),
                    text("done"),
                ],
            ),
            vec![user_hook(HookEvent::PreToolUse, Some(matcher), GUARD)],
        )
        .await;
        bench.define(GUARD, |_| exits(2, "", "no delegation"));
        let observed = bench.run("Delegate.", true).await;
        assert_eq!(
            observed.results[0].content,
            "PreToolUse:Subagent hook error: [guard.sh]: no delegation",
            "{matcher}"
        );
        assert_eq!(bench.inputs_of(GUARD)[0]["tool_name"], "Subagent");
        bench.agent.shutdown().await;
    }
}

#[tokio::test]
async fn hooks_without_a_decision_leave_ordinary_auto_review_unchanged() {
    let bench = Bench::start(
        Options::new(ExecutionMode::Auto, vec![build_call(), text("done")]),
        vec![
            user_hook(HookEvent::PreToolUse, None, "observe.sh"),
            plugin_hook(HookEvent::PreToolUse, None, GUARD),
        ],
    )
    .await;
    bench.define("observe.sh", |_| exits(0, "noted\n", ""));
    bench.define(GUARD, |_| {
        pre_tool_answer(json!({"permissionDecision": "allow"}))
    });
    let observed = bench.run("Build it.", false).await;
    assert!(bench.reviewed(), "a plugin's allow does not skip review");
    assert!(observed.requests.is_empty());
    assert_eq!(observed.results[0].content, "built");
    assert_ne!(
        observed.approvals[0].direct_reason,
        Some(DirectApprovalReason::Hook)
    );
    bench.agent.shutdown().await;
}

#[tokio::test]
async fn hooks_read_the_claude_code_transcript_of_a_stored_chat() {
    let bench = Bench::start(
        Options {
            keeps_transcripts: true,
            ..Options::new(ExecutionMode::FullAccess, vec![build_call(), text("done")])
        },
        vec![user_hook(HookEvent::PreToolUse, None, GUARD)],
    )
    .await;
    let transcripts = Arc::new(Mutex::new(Vec::new()));
    let seen = transcripts.clone();
    bench.define(GUARD, move |input| {
        let path = input["transcript_path"].as_str().unwrap().to_owned();
        seen.lock()
            .unwrap()
            .push(std::fs::read_to_string(&path).unwrap_or_default());
        exits(0, "", "")
    });
    bench.run("Build the project.", true).await;
    let transcript = transcripts.lock().unwrap()[0].clone();
    assert!(transcript.contains("Build the project."), "{transcript}");
    assert!(transcript.contains("\"type\":\"tool_use\""), "{transcript}");
    bench.agent.shutdown().await;
}

fn is_child_request(request: &tickloop_harness_model::ModelRequest) -> bool {
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
async fn a_subagent_runs_trusted_hooks_as_an_agent_and_skips_unconfirmed_ones_without_waiting() {
    let routed = Arc::new(RoutedModel::new(|request| {
        if is_child_request(request) {
            if has_tool_result(request, "child-build") {
                text("built")
            } else {
                call("child-build", "Bash", json!({"command": BUILD}))
            }
        } else if has_tool_result(request, "delegate") {
            text("waiting")
        } else {
            call("delegate", "Subagent", json!({"task": "Build the project"}))
        }
    }));
    let mut pending = plugin_hook(HookEvent::PreToolUse, Some("Bash"), GUARD);
    pending.trust = HookTrust::Pending;
    let bench = Bench::start(
        Options {
            routed: Some(routed),
            ..Options::new(ExecutionMode::FullAccess, Vec::new())
        },
        vec![
            user_hook(HookEvent::PreToolUse, Some("Bash"), "observe.sh"),
            pending,
        ],
    )
    .await;
    bench.define("observe.sh", |_| exits(0, "", ""));
    bench.define(GUARD, |_| exits(2, "", "must not run"));
    let mut events = bench.agent.subscribe();
    bench
        .agent
        .submit(bench.chat, "Delegate the build.".into())
        .await
        .unwrap();
    let mut child_skip = None;
    let mut child_built = false;
    tokio::time::timeout(Duration::from_secs(20), async {
        while child_skip.is_none() || !child_built {
            let event = next_event(&mut events).await;
            if event.chat_id == bench.chat {
                continue;
            }
            match event.kind {
                EventKind::HooksSkipped { notice } => child_skip = Some(notice),
                EventKind::ToolCompleted { result, .. } => child_built = result.content == "built",
                _ => {}
            }
        }
    })
    .await
    .expect("the child neither waits nor stalls");
    assert_eq!(child_skip.unwrap().unconfirmed, 1);
    let input = &bench.inputs_of("observe.sh")[0];
    assert_eq!(input["agent_type"], "general-purpose");
    assert_ne!(input["agent_id"], bench.chat.uuid().to_string());
    assert_eq!(input["session_id"], input["agent_id"]);
    assert!(bench.inputs_of(GUARD).is_empty());
    bench.agent.shutdown().await;
}

#[tokio::test]
async fn an_if_condition_runs_the_hook_only_for_calls_its_permission_rule_matches() {
    const PUBLISH: &str = "git push origin main";
    let mut guarded = user_hook(HookEvent::PreToolUse, Some("Bash"), GUARD);
    guarded.condition = Some("Bash(git push*)".into());
    let bench = Bench::start(
        Options::new(
            ExecutionMode::FullAccess,
            vec![
                build_call(),
                call("publish", "Bash", json!({"command": PUBLISH})),
                text("done"),
            ],
        ),
        vec![guarded],
    )
    .await;
    bench
        .environment
        .define_shell(PUBLISH, Duration::ZERO, b"published".to_vec(), 0);
    bench.define(GUARD, |_| exits(2, "", "not on Fridays"));
    let observed = bench.run("Build and publish.", true).await;
    assert_eq!(observed.results[0].content, "built");
    assert_eq!(
        observed.results[1].content,
        "PreToolUse:Bash hook error: [guard.sh]: not on Fridays"
    );
    let inputs = bench.inputs_of(GUARD);
    assert_eq!(inputs.len(), 1);
    assert_eq!(inputs[0]["tool_input"]["command"], PUBLISH);
    bench.agent.shutdown().await;
}

fn open_counter(id: &str) -> Vec<Content> {
    call(
        id,
        "TickManage",
        json!({"action": "open", "command": ["/counter"], "cwd": "/", "protocol": true}),
    )
}

#[tokio::test]
async fn host_tool_calls_go_through_the_same_hook_gate() {
    let ticks = |mode| Options {
        offers_ticks: true,
        ..Options::new(mode, vec![open_counter("tick"), text("done")])
    };

    let bench = Bench::start(
        ticks(ExecutionMode::Auto),
        vec![user_hook(HookEvent::PreToolUse, Some("TickManage"), GUARD)],
    )
    .await;
    bench.define(GUARD, |_| {
        pre_tool_answer(json!({"permissionDecision": "ask"}))
    });
    let observed = bench.run("Open the counter.", true).await;
    assert_eq!(observed.requests.len(), 1);
    assert_eq!(
        observed.requests[0].approval_source,
        PermissionApprovalSource::Hook
    );
    assert!(!bench.reviewed());
    assert!(!observed.results[0].is_error, "{:?}", observed.results[0]);
    bench.agent.shutdown().await;

    let bench = Bench::start(
        ticks(ExecutionMode::Auto),
        vec![user_hook(HookEvent::PreToolUse, Some("TickManage"), GUARD)],
    )
    .await;
    bench.define(GUARD, |_| {
        pre_tool_answer(json!({"permissionDecision": "allow"}))
    });
    let observed = bench.run("Open the counter.", false).await;
    assert!(observed.requests.is_empty());
    assert!(!bench.reviewed(), "a user's allow admits before review");
    assert!(!observed.results[0].is_error, "{:?}", observed.results[0]);
    assert_eq!(
        observed.approvals[0].direct_reason,
        Some(DirectApprovalReason::Hook)
    );
    bench.agent.shutdown().await;

    let bench = Bench::start(
        ticks(ExecutionMode::Auto),
        vec![
            user_hook(HookEvent::PreToolUse, Some("TickManage"), GUARD),
            plugin_hook(HookEvent::PreToolUse, Some("TickManage"), "rewrite.sh"),
        ],
    )
    .await;
    bench.define(GUARD, |_| {
        pre_tool_answer(json!({"permissionDecision": "allow"}))
    });
    bench.define("rewrite.sh", |input| {
        let mut rewritten = input["tool_input"].clone();
        rewritten["cwd"] = json!("/tmp");
        pre_tool_answer(json!({"updatedInput": rewritten}))
    });
    let observed = bench.run("Open the counter.", false).await;
    assert!(
        bench.reviewed(),
        "a user's allow never covers input a plugin rewrote"
    );
    assert_ne!(
        observed.approvals[0].direct_reason,
        Some(DirectApprovalReason::Hook)
    );
    bench.agent.shutdown().await;
}

#[tokio::test]
async fn a_rewrite_may_not_change_a_tick_calls_action() {
    let bench = Bench::start(
        Options {
            offers_ticks: true,
            ..Options::new(
                ExecutionMode::FullAccess,
                vec![open_counter("tick"), text("done")],
            )
        },
        vec![plugin_hook(
            HookEvent::PreToolUse,
            Some("TickManage"),
            "rewrite.sh",
        )],
    )
    .await;
    bench.define("rewrite.sh", |_| {
        pre_tool_answer(
            json!({"updatedInput": {"action": "subscribe", "tick_instance_id": "hosted:x"}}),
        )
    });
    let observed = bench.run("Open the counter.", true).await;
    assert_eq!(
        observed.results[0].content,
        "A PreToolUse hook's updatedInput changed the action of TickManage; the call was not run."
    );
    bench.agent.shutdown().await;
}

#[tokio::test]
async fn an_unrestricted_host_still_asks_when_a_hook_asks() {
    let bench = Bench::start(
        Options {
            prompt_policy: PromptPolicy::Unrestricted,
            ..Options::new(ExecutionMode::AcceptEdits, vec![build_call(), text("done")])
        },
        vec![plugin_hook(HookEvent::PreToolUse, Some("Bash"), GUARD)],
    )
    .await;
    bench.define(GUARD, |_| {
        pre_tool_answer(json!({"permissionDecision": "ask"}))
    });
    let observed = bench.run("Build it.", false).await;
    assert_eq!(observed.requests.len(), 1);
    assert!(observed.started.is_empty());
    assert!(observed.results[0].is_error);
    bench.agent.shutdown().await;
}

const HOOK_SECRET_SENTINEL: &str = "HOOK_SECRET_SENTINEL";

/// A plugin hook whose command was given the sensitive [`HOOK_SECRET_SENTINEL`].
fn secret_hook(event: HookEvent, script: &str) -> ConfiguredHook {
    ConfiguredHook {
        command: format!("{script} ${{user_config.token}}"),
        resolved_command: Some(format!("{script} {HOOK_SECRET_SENTINEL}")),
        sensitive_values: vec![HOOK_SECRET_SENTINEL.into()],
        ..plugin_hook(event, Some("Bash"), script)
    }
}

/// Fails when anything the model, the Chat's views or its records got holds the secret.
async fn assert_secret_kept(bench: &Bench, observed: &Observed) {
    let requests = format!("{:?}", bench.model.requests.lock().unwrap());
    let snapshot = format!("{:?}", bench.agent.snapshot(bench.chat).await.unwrap());
    let history = format!(
        "{:?}",
        bench.agent.approval_history_view(bench.chat).await.unwrap()
    );
    let shown = format!("{:?} {:?}", observed.results, observed.hook_runs);
    for (what, text) in [
        ("model requests", requests),
        ("snapshot", snapshot),
        ("activity history", history),
        ("events", shown),
    ] {
        assert!(!text.contains(HOOK_SECRET_SENTINEL), "{what}: {text}");
    }
}

#[tokio::test]
async fn a_sensitive_plugin_value_a_hook_prints_never_reaches_the_model_views_or_records() {
    let denying = "deny.sh";
    let blocking = "block.sh";
    let bench = Bench::start(
        Options::new(ExecutionMode::FullAccess, vec![build_call(), text("done")]),
        vec![
            secret_hook(HookEvent::PreToolUse, denying),
            secret_hook(HookEvent::PreToolUse, blocking),
        ],
    )
    .await;
    bench.define(&format!("{denying} {HOOK_SECRET_SENTINEL}"), |_| {
        let mut run = pre_tool_answer(json!({
            "permissionDecision": "deny",
            "permissionDecisionReason": format!("token {HOOK_SECRET_SENTINEL} is stale"),
            "additionalContext": format!("context {HOOK_SECRET_SENTINEL}"),
            "updatedInput": {"command": format!("{BUILD} --token {HOOK_SECRET_SENTINEL}")},
        }));
        run.stderr = format!("debug {HOOK_SECRET_SENTINEL}").into_bytes();
        run
    });
    bench.define(&format!("{blocking} {HOOK_SECRET_SENTINEL}"), |_| {
        exits(2, "", &format!("rejected {HOOK_SECRET_SENTINEL}\n"))
    });

    let observed = bench.run("Build it.", true).await;

    assert!(observed.started.is_empty());
    let refusal = &observed.results[0].content;
    assert!(refusal.contains("[redacted]"), "{refusal}");
    assert_secret_kept(&bench, &observed).await;
    bench.agent.shutdown().await;

    let bench = Bench::start(
        Options::new(ExecutionMode::FullAccess, vec![build_call(), text("done")]),
        vec![secret_hook(HookEvent::PostToolUse, GUARD)],
    )
    .await;
    bench.define(&format!("{GUARD} {HOOK_SECRET_SENTINEL}"), |_| {
        answer(json!({
            "systemMessage": format!("saw {HOOK_SECRET_SENTINEL}"),
            "hookSpecificOutput": {
                "hookEventName": "PostToolUse",
                "additionalContext": format!("context {HOOK_SECRET_SENTINEL}"),
            },
        }))
    });

    let observed = bench.run("Build it.", true).await;

    let requests = format!("{:?}", bench.model.requests.lock().unwrap());
    assert!(requests.contains("context [redacted]"), "{requests}");
    assert_secret_kept(&bench, &observed).await;
    bench.agent.shutdown().await;
}

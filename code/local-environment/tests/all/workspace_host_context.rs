use std::{
    path::Path,
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tickloop_code_settings::local_agent::LocalAgent;

use async_trait::async_trait;
use serde_json::{Value, json};
use tickloop_code_local_environment::hosted::{
    tools::HostedTickTools,
    worker_control::{DaemonControl, WorkerControlGrant},
};
use tickloop_harness::model::{ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest};
use tickloop_harness::protocol::{
    Content, EventKind, HerdrInteraction, HerdrPane, HostCallContext, ModelIdentity, Role,
    ToolDefinition, ToolResult,
};
use tickloop_harness::runtime::{
    Agent,
    host_tools::{HostCallScope, HostToolPermission, HostToolProvider, PreparedHostCall},
};
use tokio::sync::{Notify, mpsc::UnboundedSender};
use tokio_util::sync::CancellationToken;

use tickloop_harness::test_support::{ScriptedModel, call, config, next_event, text};

#[tokio::test]
async fn sandbox_guidance_tracks_effective_settings_without_rewriting_request_prefixes() {
    use tickloop_code_local_environment::{
        environment::LocalEnvironment, hosted::shells::OwnTickhost,
    };
    use tickloop_code_settings::UserSettings;
    use tickloop_harness::{
        runtime::{HarnessOptions, HarnessSeams},
        test_support::harness::ChosenModels,
    };

    let directory = tempfile::tempdir().unwrap();
    let folder = directory.path().join("folder");
    let settings_root = directory.path().join("settings");
    std::fs::create_dir(&folder).unwrap();
    std::fs::create_dir(&settings_root).unwrap();
    let settings_path = settings_root.join("settings.json");
    let settings = UserSettings::in_root(&settings_root);
    let environment =
        LocalEnvironment::with_shells(Arc::new(OwnTickhost::new())).with_sandbox(settings.clone());
    let model = Arc::new(ScriptedModel::new(
        (0..14).map(|_| text("Ready.")).collect(),
    ));
    let agent = Agent::with_seams(
        HarnessSeams {
            search: None,
            transcripts: None,
            environment: Some(Arc::new(environment)),
            models: ChosenModels::new(model.clone()),
        },
        HarnessOptions {
            settings: Some(Arc::new(settings)),
            ..HarnessOptions::local(config())
        },
    )
    .unwrap();
    let chat = agent.create_chat(folder.clone()).await.unwrap();
    let mut events = agent.subscribe();
    let mut sandbox = json!({"enabled":false});
    let mut previous_messages = Vec::new();
    let mut initial_system = None;
    let mut initial_tools = None;
    let mut notes_seen = 0;
    let withdrawal = "The Bash command sandbox is disabled. Earlier sandbox instructions no longer apply; ordinary permission checks still apply.";

    for turn in 0..14 {
        match turn {
            1 => {
                sandbox["enabled"] = json!(true);
                sandbox["excludedCommands"] =
                    json!(["TOKEN=PRIVATE_EXCLUSION_SENTINEL vendor-cli *"]);
            }
            3 => sandbox["allowUnsandboxedCommands"] = json!(false),
            4 => sandbox["network"] = json!({"allowedDomains":["z.example", "a.example"]}),
            5 => {
                sandbox["filesystem"] =
                    json!({"allowWrite":["build", "cache"], "denyWrite":["private"]})
            }
            6 => sandbox["filesystem"]["allowGitConfig"] = json!(true),
            7 => sandbox["excludedCommands"] = json!(["git status", "npm test"]),
            8 => sandbox["failIfUnavailable"] = json!(true),
            9 => sandbox["autoAllowBashIfSandboxed"] = json!(false),
            10 => sandbox["excludedCommands"] = json!(["git diff", "cargo test"]),
            11 => {
                sandbox["network"]["allowedDomains"] =
                    json!(["a.example", "z.example", "a.example"]);
                sandbox["filesystem"]["allowWrite"] = json!(["cache", "build", "cache"]);
                sandbox["excludedCommands"] = json!(["cargo test", "git diff", "cargo test"]);
            }
            12 => sandbox["enabled"] = json!(false),
            _ => {}
        }
        std::fs::write(
            &settings_path,
            serde_json::to_vec(&json!({
                "sandbox":{"enabled":false},
                "projects":{folder.display().to_string():{"sandbox":sandbox}}
            }))
            .unwrap(),
        )
        .unwrap();
        let receipt = agent
            .submit(chat, format!("Observe configuration {turn}"))
            .await
            .unwrap();
        loop {
            let event = next_event(&mut events).await;
            if event.run_id != receipt.run_id {
                continue;
            }
            match event.kind {
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
        {
            let requests = model.requests.lock().unwrap();
            assert_eq!(requests.len(), turn + 1);
            let request = &requests[turn];
            let messages = request
                .messages
                .iter()
                .map(|message| serde_json::to_value(message).unwrap())
                .collect::<Vec<_>>();
            let system = messages[0].clone();
            let tools = serde_json::to_value(&request.tools).unwrap();
            assert_eq!(initial_system.get_or_insert(system.clone()), &system);
            assert_eq!(initial_tools.get_or_insert(tools.clone()), &tools);
            assert_eq!(
                &messages[..previous_messages.len()],
                previous_messages.as_slice(),
                "turn {turn}: earlier request messages must remain byte-equivalent"
            );
            previous_messages = messages;
            let notes = request
                .messages
                .iter()
                .flat_map(|message| &message.content)
                .filter_map(|content| match content {
                    Content::Text { text }
                        if text.contains("## Bash command sandbox")
                            || text.contains(withdrawal) =>
                    {
                        Some(text)
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            if matches!(turn, 1 | 3..=9 | 12) {
                notes_seen += 1;
            }
            assert_eq!(
                notes.len(),
                notes_seen,
                "turn {turn}: append exactly one note only when effective guidance changes"
            );
            if (1..=11).contains(&turn) {
                let latest = notes.last().unwrap();
                assert!(
                    !latest.contains("PRIVATE_EXCLUSION_SENTINEL"),
                    "command exclusion patterns can contain credentials and must not enter model guidance"
                );
                assert!(latest.contains("configured"));
                assert!(latest.contains("/sandbox"));
                assert!(latest.contains("dangerouslyDisableSandbox"));
                assert!(latest.contains("Bash"));
                if turn >= 3 {
                    assert!(latest.contains("Strict"));
                }
                if turn >= 4 {
                    assert!(latest.contains("a.example"));
                }
                if turn >= 5 {
                    assert!(latest.contains("build"));
                    assert!(latest.contains("private"));
                }
                if turn >= 6 {
                    assert!(latest.contains(".git/config"));
                    assert!(latest.contains("hooks"));
                }
                if turn >= 7 {
                    assert!(latest.contains("\"excludedCommandCount\":2"));
                    assert!(!latest.contains("git status"));
                    assert!(!latest.contains("git diff"));
                }
                if turn >= 8 {
                    assert!(latest.contains("failIfUnavailable is true"));
                }
                if turn >= 9 {
                    assert!(latest.contains("\"autoAllowBashIfSandboxed\":false"));
                }
            }
            if turn >= 12 {
                assert!(notes.last().unwrap().contains(withdrawal));
            }
        }
    }
    agent.shutdown().await;
}

#[tokio::test]
async fn sandbox_guidance_survives_resume_branch_and_compaction_and_sanitizes_invalid_settings() {
    use tickloop_code_local_environment::{
        environment::LocalEnvironment, hosted::shells::OwnTickhost,
    };
    use tickloop_code_settings::UserSettings;
    use tickloop_harness::{
        protocol::{BranchRequest, ChatId},
        runtime::{HarnessOptions, HarnessSeams, transcript::JsonlTranscriptStore},
        test_support::{harness::ChosenModels, persisted_config},
    };

    async fn turn(agent: &Agent, chat: ChatId, prompt: &str) {
        let mut events = agent.subscribe();
        let receipt = agent.submit(chat, prompt.into()).await.unwrap();
        loop {
            let event = next_event(&mut events).await;
            if event.run_id != receipt.run_id {
                continue;
            }
            match event.kind {
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
    }

    fn notes(request: &ModelRequest) -> Vec<&str> {
        request
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(|content| match content {
                Content::Text { text }
                    if text.contains("## Bash command sandbox")
                        || text.contains("Earlier sandbox instructions no longer apply") =>
                {
                    Some(text.as_str())
                }
                _ => None,
            })
            .collect()
    }

    let directory = tempfile::tempdir().unwrap();
    let folder = directory.path().join("folder");
    let settings_root = directory.path().join("settings");
    let state_root = directory.path().join("state");
    std::fs::create_dir(&folder).unwrap();
    std::fs::create_dir(&settings_root).unwrap();
    let settings_path = settings_root.join("settings.json");
    std::fs::write(&settings_path, r#"{"sandbox":{"enabled":true}}"#).unwrap();
    let model = Arc::new(ScriptedModel::new(
        (0..17)
            .map(|_| text("The user is checking sandbox configuration; no commands have run."))
            .collect(),
    ));
    let open = || {
        let settings = UserSettings::in_root(&settings_root);
        let environment = LocalEnvironment::with_shells(Arc::new(OwnTickhost::new()))
            .with_sandbox(settings.clone());
        Agent::with_seams(
            HarnessSeams {
                search: None,
                transcripts: Some(Arc::new(
                    JsonlTranscriptStore::open(state_root.clone()).unwrap(),
                )),
                environment: Some(Arc::new(environment)),
                models: ChosenModels::new(model.clone()),
            },
            HarnessOptions {
                local_state_root: Some(state_root.clone()),
                settings: Some(Arc::new(settings)),
                ..HarnessOptions::local(persisted_config())
            },
        )
        .unwrap()
    };
    let agent = open();
    let chat = agent.create_chat(folder).await.unwrap();
    turn(&agent, chat, "Check the configured sandbox.").await;
    assert_eq!(
        notes(model.requests.lock().unwrap().last().unwrap()).len(),
        1
    );
    std::fs::write(
        &settings_path,
        r#"{"sandbox":{"enabled":"PRIVATE_PARSE_INPUT_ONE"}}"#,
    )
    .unwrap();
    turn(&agent, chat, "Check invalid configuration.").await;
    let invalid_note = {
        let requests = model.requests.lock().unwrap();
        let current = notes(requests.last().unwrap());
        assert_eq!(current.len(), 2);
        let invalid = current.last().unwrap().to_string();
        assert!(invalid.contains("Shell execution is blocked"));
        assert!(invalid.contains("/sandbox"));
        assert!(!invalid.contains("PRIVATE_PARSE_INPUT"));
        invalid
    };
    std::fs::write(
        &settings_path,
        r#"{"sandbox":{"enabled":"PRIVATE_PARSE_INPUT_TWO"}}"#,
    )
    .unwrap();
    turn(&agent, chat, "Check another invalid configuration.").await;
    {
        let requests = model.requests.lock().unwrap();
        let current = notes(requests.last().unwrap());
        assert_eq!(current.len(), 2);
        assert_eq!(*current.last().unwrap(), invalid_note);
    }
    std::fs::write(&settings_path, r#"{"sandbox":{"enabled":true}}"#).unwrap();
    turn(&agent, chat, "Check corrected configuration.").await;
    assert_eq!(
        notes(model.requests.lock().unwrap().last().unwrap()).len(),
        3
    );
    agent.shutdown().await;
    drop(agent);

    let agent = open();
    agent.resume_chat(chat).await.unwrap();
    turn(&agent, chat, "Continue with unchanged sandbox settings.").await;
    assert_eq!(
        notes(model.requests.lock().unwrap().last().unwrap()).len(),
        3,
        "resume must retain the last delivered guidance"
    );
    let branch = agent
        .branch_chat(BranchRequest {
            request_id: tickloop_ids::generate(),
            source_chat_id: chat,
            name: None,
        })
        .await
        .unwrap();
    turn(&agent, branch.child_chat_id, "Continue in the branch.").await;
    assert_eq!(
        notes(model.requests.lock().unwrap().last().unwrap()).len(),
        3,
        "branch must retain the last delivered guidance"
    );
    agent.compact(chat).await.unwrap();
    turn(&agent, chat, "Continue after compaction.").await;
    {
        let requests = model.requests.lock().unwrap();
        let current = notes(requests.last().unwrap());
        assert_eq!(
            current.len(),
            1,
            "compaction must re-emit current guidance once, not stale settings"
        );
        assert!(current[0].contains("configured"));
        assert!(!current[0].contains("invalid or unreadable"));
    }
    agent.compact(chat).await.unwrap();
    agent.compact(branch.child_chat_id).await.unwrap();
    agent.shutdown().await;
    drop(agent);

    let agent = open();
    agent.resume_chat(chat).await.unwrap();
    for prompt in [
        "Continue after persisted compaction.",
        "Continue without another change.",
    ] {
        turn(&agent, chat, prompt).await;
        let requests = model.requests.lock().unwrap();
        let current = notes(requests.last().unwrap());
        assert_eq!(
            current.len(),
            1,
            "enabled guidance must be re-emitted once after compact and restart"
        );
        assert!(current[0].contains("configured"));
    }
    agent.shutdown().await;
    drop(agent);

    std::fs::write(&settings_path, r#"{"sandbox":{"enabled":false}}"#).unwrap();
    let agent = open();
    agent.resume_chat(chat).await.unwrap();
    turn(&agent, chat, "Continue after the sandbox was disabled.").await;
    {
        let requests = model.requests.lock().unwrap();
        let current = notes(requests.last().unwrap());
        assert_eq!(current.len(), 2);
        assert!(
            current
                .last()
                .unwrap()
                .contains("Earlier sandbox instructions no longer apply")
        );
    }
    agent.resume_chat(branch.child_chat_id).await.unwrap();
    turn(
        &agent,
        branch.child_chat_id,
        "Continue after compaction and disabling.",
    )
    .await;
    {
        let requests = model.requests.lock().unwrap();
        let current = notes(requests.last().unwrap());
        assert_eq!(
            current.len(),
            1,
            "disabling immediately after compaction still withdraws the archived guidance"
        );
        assert!(current[0].contains("Earlier sandbox instructions no longer apply"));
    }
    agent.compact(chat).await.unwrap();
    agent.shutdown().await;
    drop(agent);

    let agent = open();
    agent.resume_chat(chat).await.unwrap();
    for prompt in [
        "Continue after compacting the withdrawal.",
        "Keep the sandbox disabled.",
    ] {
        turn(&agent, chat, prompt).await;
        let requests = model.requests.lock().unwrap();
        assert!(
            notes(requests.last().unwrap()).is_empty(),
            "an already-withdrawn sandbox must not emit a second withdrawal after compact and restart"
        );
    }
    agent.shutdown().await;
}

const READ_HOST_SOURCE: &str = "ReadCapturedHostSource";
const QUEUED_SOURCE_INPUT: &str = "Use the host source captured by this follow-up input.";

struct MergedInputModel {
    started: Notify,
    release: Notify,
    requests: AtomicUsize,
    scripted: ScriptedModel,
}

impl MergedInputModel {
    fn new() -> Self {
        Self {
            started: Notify::new(),
            release: Notify::new(),
            requests: AtomicUsize::new(0),
            scripted: ScriptedModel::new(vec![
                call("before-merge", READ_HOST_SOURCE, json!({"phase":"before"})),
                call("after-merge", READ_HOST_SOURCE, json!({"phase":"after"})),
                text("Finished reading the submitted host source."),
            ]),
        }
    }
}

#[async_trait]
impl ModelClient for MergedInputModel {
    async fn generate_title(
        &self,
        request: tickloop_harness::model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_harness::model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_harness::model::ModelOutput, tickloop_harness::model::ModelError> {
        Ok(tickloop_harness::test_support::title_output(request))
    }

    fn identity(&self) -> ModelIdentity {
        self.scripted.identity()
    }

    async fn complete(
        &self,
        request: ModelRequest,
        events: UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        match self.requests.fetch_add(1, Ordering::SeqCst) {
            0 => {
                self.started.notify_one();
                tokio::select! {
                    _ = cancel.cancelled() => return Err(ModelError::Cancelled),
                    _ = self.release.notified() => {}
                }
            }
            1 => assert!(
                request.messages.iter().any(|message| {
                    message.role == Role::User
                        && message.content.iter().any(|content| {
                            matches!(content, Content::Text { text } if text == QUEUED_SOURCE_INPUT)
                        })
                }),
                "the next host call must be generated after the model receives the merged input"
            ),
            _ => {}
        }
        self.scripted.complete(request, events, cancel).await
    }
}

struct CapturedHostSource;

#[async_trait]
impl HostToolProvider for CapturedHostSource {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: READ_HOST_SOURCE.into(),
            description: "Read the source captured when this host call was prepared.".into(),
            input_schema: json!({
                "type":"object",
                "properties":{"phase":{"type":"string"}},
                "required":["phase"],
                "additionalProperties":false
            }),
        }]
    }

    fn permission(
        &self,
        _: tickloop_harness::protocol::ChatId,
        _: &str,
        _: &Value,
    ) -> HostToolPermission {
        HostToolPermission::ReadOnly
    }

    async fn prepare_call_in_scope(
        &self,
        chat: tickloop_harness::protocol::ChatId,
        scope: &HostCallScope<'_>,
        name: &str,
        input: &Value,
        cancel: CancellationToken,
    ) -> Result<PreparedHostCall, String> {
        let Some(context) = scope.context else {
            return self.prepare_call(chat, name, input, cancel).await;
        };
        Ok(PreparedHostCall::new(
            name.into(),
            input.clone(),
            json!({"phase":input["phase"],"context":context}),
            HostToolPermission::ReadOnly,
            "Read captured host source".into(),
        ))
    }

    async fn execute(
        &self,
        _: tickloop_harness::protocol::ChatId,
        _: &str,
        input: Value,
        _: CancellationToken,
    ) -> ToolResult {
        ToolResult {
            content: input.to_string(),
            is_error: false,
            output_ref: None,
        }
    }
}

fn captured_view_context(workdir: &Path, view: &str) -> HostCallContext {
    HostCallContext {
        folder: workdir.into(),
        workspace_id: Some("task-workspace".into()),
        interaction: Some(HerdrInteraction {
            socket: workdir.join(format!("{view}.sock")),
            pane: HerdrPane {
                pane_id: format!("pane-{view}"),
                terminal_id: format!("terminal-{view}"),
                workspace_id: format!("view-workspace-{view}"),
                tab_id: format!("tab-{view}"),
                cwd: None,
                foreground_cwd: None,
            },
        }),
    }
}

async fn assert_merged_host_source(headless: bool) {
    let directory = tempfile::tempdir().unwrap();
    let workdir = directory.path().canonicalize().unwrap();
    let model = Arc::new(MergedInputModel::new());
    let agent = Agent::new_embedded(config(), model.clone());
    agent
        .install_host_tools(Arc::new(CapturedHostSource))
        .unwrap();
    let chat = agent.create_chat(workdir.clone()).await.unwrap();
    let work_context = HostCallContext {
        folder: workdir.clone(),
        workspace_id: Some("task-workspace".into()),
        interaction: None,
    };
    agent
        .set_host_work_context(chat, work_context.clone())
        .await
        .unwrap();
    let first_context = captured_view_context(&workdir, "first");
    let expected_context = if headless {
        work_context
    } else {
        captured_view_context(&workdir, "second")
    };
    let mut events = agent.subscribe();
    let first = agent
        .submit_with_input_contexts_in_host_context(
            chat,
            "Read the original host source.".into(),
            Vec::new(),
            Vec::new(),
            Some(first_context.clone()),
        )
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), model.started.notified())
        .await
        .unwrap();
    let follow_up = agent
        .submit_with_input_contexts_in_host_context(
            chat,
            QUEUED_SOURCE_INPUT.into(),
            Vec::new(),
            Vec::new(),
            (!headless).then(|| expected_context.clone()),
        )
        .await
        .unwrap();
    assert_ne!(first.run_id, follow_up.run_id);
    model.release.notify_one();
    let mut input_merged = false;
    let mut results = Vec::new();
    loop {
        let event = next_event(&mut events).await;
        match event.kind {
            EventKind::InputMerged { into_run_id } if event.run_id == follow_up.run_id => {
                assert_eq!(into_run_id, first.run_id);
                input_merged = true;
            }
            EventKind::ToolCompleted { result, .. } if event.run_id == first.run_id => {
                assert!(!result.is_error, "{}", result.content);
                results.push(serde_json::from_str::<Value>(&result.content).unwrap());
            }
            EventKind::Completed { .. } if event.run_id == first.run_id => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    agent.shutdown().await;
    assert!(
        input_merged,
        "the follow-up must be merged into the active run"
    );
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["phase"], "before");
    assert_eq!(results[0]["context"], json!(first_context));
    assert_eq!(results[1]["phase"], "after");
    assert_eq!(results[1]["context"], json!(expected_context));
}

#[tokio::test]
async fn sandbox_guidance_summarized_away_by_a_directed_summary_is_still_withdrawn() {
    use tickloop_code_local_environment::{
        environment::LocalEnvironment, hosted::shells::OwnTickhost,
    };
    use tickloop_code_settings::UserSettings;
    use tickloop_harness::{
        protocol::{ChatId, DirectedSummaryRequest, SummaryDirection},
        runtime::{HarnessOptions, HarnessSeams},
        test_support::{harness::ChosenModels, persisted_config},
    };

    async fn turn(agent: &Agent, chat: ChatId, prompt: &str) {
        let mut events = agent.subscribe();
        let receipt = agent.submit(chat, prompt.into()).await.unwrap();
        loop {
            let event = next_event(&mut events).await;
            if event.run_id != receipt.run_id {
                continue;
            }
            match event.kind {
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
    }

    let directory = tempfile::tempdir().unwrap();
    let folder = directory.path().join("folder");
    let settings_root = directory.path().join("settings");
    std::fs::create_dir(&folder).unwrap();
    std::fs::create_dir(&settings_root).unwrap();
    let settings_path = settings_root.join("settings.json");
    std::fs::write(&settings_path, r#"{"sandbox":{"enabled":true}}"#).unwrap();
    let model = Arc::new(ScriptedModel::new(
        (0..4)
            .map(|_| text("The configured Bash command sandbox limits writes to the Folder."))
            .collect(),
    ));
    let settings = UserSettings::in_root(&settings_root);
    let agent = Agent::with_seams(
        HarnessSeams {
            search: None,
            transcripts: None,
            environment: Some(Arc::new(
                LocalEnvironment::with_shells(Arc::new(OwnTickhost::new()))
                    .with_sandbox(settings.clone()),
            )),
            models: ChosenModels::new(model.clone()),
        },
        HarnessOptions {
            settings: Some(Arc::new(settings)),
            ..HarnessOptions::local(persisted_config())
        },
    )
    .unwrap();
    let chat = agent.create_chat(folder).await.unwrap();
    turn(&agent, chat, "Check the configured sandbox.").await;
    turn(&agent, chat, "Keep working.").await;
    let snapshot = agent.snapshot(chat).await.unwrap();
    let second_prompt = snapshot
        .messages
        .iter()
        .position(|message| {
            message.role == Role::User
                && message.content.iter().any(
                    |content| matches!(content, Content::Text { text } if text == "Keep working."),
                )
        })
        .unwrap();
    agent
        .summarize_checkpoint(
            chat,
            DirectedSummaryRequest {
                message_id: snapshot.message_ids[&second_prompt],
                direction: SummaryDirection::UpToHere,
                guidance: None,
            },
        )
        .await
        .unwrap();

    std::fs::write(&settings_path, r#"{"sandbox":{"enabled":false}}"#).unwrap();
    turn(&agent, chat, "Continue after the sandbox was disabled.").await;

    let requests = model.requests.lock().unwrap();
    let last = requests.last().unwrap();
    assert!(
        last.messages
            .iter()
            .flat_map(|message| &message.content)
            .any(|content| matches!(
                content,
                Content::Text { text } if text.contains("Earlier sandbox instructions no longer apply")
            )),
        "a summary may still carry the earlier guidance, so disabling must withdraw it"
    );
}

#[tokio::test]
async fn merged_input_uses_its_captured_view_source_for_subsequent_host_calls() {
    assert_merged_host_source(false).await;
}

#[tokio::test]
async fn merged_headless_input_clears_the_old_view_anchor_and_uses_work_context() {
    assert_merged_host_source(true).await;
}

/// The first request names the container and whether Ticks run here; no container's Panes show
/// Ticks, so none is advertised as a place to show them, and no tool lists or places Panes.
#[test]
fn first_model_request_identifies_host_without_advertising_pane_display() {
    let binary = std::env::current_exe().unwrap();
    for (case, host) in [
        ("herdr", "herdr"),
        ("herdr_without_provider", "herdr"),
        ("tmux", "tmux"),
        ("zellij", "zellij"),
        ("standalone", "standalone"),
        ("tickhost", "standalone"),
    ] {
        let mut child = Command::new(&binary);
        child
            .arg("--exact")
            .arg(tickloop_harness::test_support::test_path!(
                workspace_host_request_probe
            ))
            .arg("--nocapture")
            .env("TICKLOOP_WORKSPACE_HOST_PROBE", case)
            .env_remove("HERDR_ENV")
            .env_remove("TMUX")
            .env_remove("ZELLIJ");
        match case {
            "herdr" | "herdr_without_provider" => {
                child.env("HERDR_ENV", "1");
                child.env("TMUX", "/private/tmux.sock");
                child.env("ZELLIJ", "0");
            }
            "tmux" => {
                child.env("TMUX", "/private/tmux.sock");
                child.env("ZELLIJ", "0");
            }
            "zellij" => {
                child.env("ZELLIJ", "0");
            }
            "standalone" | "tickhost" => {}
            _ => unreachable!(),
        }
        let output = child.output().unwrap();
        assert!(
            output.status.success(),
            "{case}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        let state = stdout
            .lines()
            .find_map(|line| {
                line.split_once("WORKSPACE_HOST_STATE:")
                    .map(|(_, state)| state)
            })
            .unwrap_or_else(|| panic!("{case}: missing model request state in {stdout}"));
        let state: Value = serde_json::from_str(state).unwrap();
        let pane_display = stdout
            .lines()
            .find_map(|line| line.split_once("PANE_DISPLAY:").map(|(_, line)| line))
            .unwrap_or_else(|| panic!("{case}: missing pane display line in {stdout}"));
        assert_eq!(pane_display, "none", "{case}");
        assert_eq!(state["workspace_host"]["name"], host, "{case}");
        assert!(
            state["workspace_host"].get("integration").is_none(),
            "{case}: no Pane shows Ticks, so no integration is reported"
        );
        let runs_ticks = matches!(case, "herdr" | "tickhost");
        assert!(
            runs_ticks || state["workspace_host"]["reader_executable"].is_null(),
            "{case}: only an Agent that can run Ticks names its reader_executable"
        );
        assert!(
            state["workspace_host"].get("tickhost_executable").is_none(),
            "{case}: the bundled tickhost is no program to open"
        );
        assert_eq!(
            state["workspace_host"]["plugins"],
            json!([]),
            "{case}: no Tick package is installed"
        );
        assert_eq!(
            state["workspace_host"]
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<std::collections::BTreeSet<_>>(),
            ["name", "plugins", "tickhost"].into_iter().collect(),
            "{case}: only actual host capabilities are reported"
        );
        assert_eq!(
            state["workspace_host"]["tickhost"],
            if runs_ticks {
                "available"
            } else {
                "unavailable"
            },
            "{case}: Ticks need the Tickhost"
        );
        assert!(!state.to_string().contains("/private/tmux.sock"));
    }
}

#[tokio::test]
async fn workspace_host_request_probe() {
    let Ok(case) = std::env::var("TICKLOOP_WORKSPACE_HOST_PROBE") else {
        return;
    };
    let directory = tempfile::tempdir().unwrap();
    let state_root = directory.path().join("state");
    let model = Arc::new(ScriptedModel::new(vec![
        text("Ready."),
        text("Still ready."),
    ]));
    let agent = Agent::open(state_root.clone(), config(), model.clone()).unwrap();
    agent
        .trust_working_directory(directory.path().into())
        .await
        .unwrap();
    if matches!(case.as_str(), "herdr" | "tickhost") {
        let hosted = HostedTickTools::new(
            DaemonControl::new(WorkerControlGrant {
                endpoint: directory.path().join("unused-daemon.sock"),
                daemon_epoch: uuid::Uuid::new_v4(),
                binding: uuid::Uuid::new_v4(),
                capability: "unused".into(),
                host_endpoint: directory.path().join("unused-host.sock"),
            }),
            Vec::new(),
        );
        agent.install_host_tools(Arc::new(hosted)).unwrap();
    }
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    for input in ["Check host", "Check host again"] {
        let receipt = agent.submit(chat, input.into()).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let event = events.recv().await.unwrap();
                if event.run_id == receipt.run_id
                    && matches!(event.kind, EventKind::Completed { .. })
                {
                    break;
                }
            }
        })
        .await
        .unwrap();
    }
    let snapshot = agent.snapshot(chat).await.unwrap();
    // Tick capabilities need no general Skill activation.
    assert!(snapshot.invoked_skills.is_empty());
    {
        let requests = model.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        for request in requests.iter() {
            assert!(!request.messages.iter().any(|message| message.content.iter().any(
                |block| matches!(block, Content::Text { text } if text.contains("Reference guidance from tickloop"))
            )));
        }
        let Content::Text { text } = &requests[0].messages[0].content[0] else {
            panic!("first model message must be system text");
        };
        let pane_display = text
            .split("\n\n")
            .find(|part| part.starts_with("# Pane display\n"))
            .unwrap_or("none");
        println!("PANE_DISPLAY:{}", pane_display.replace('\n', " "));
    }
    let user_inputs = snapshot
        .messages
        .iter()
        .enumerate()
        .filter(|(index, message)| {
            message.role == Role::User && !snapshot.host_notes.contains_key(index)
        })
        .map(|(_, message)| match message.content.as_slice() {
            [Content::Text { text }] => text.as_str(),
            _ => panic!("the probe only submits plain user inputs"),
        })
        .collect::<Vec<_>>();
    assert_eq!(user_inputs, ["Check host", "Check host again"]);
    agent.shutdown().await;
    drop(agent);
    let restored = Agent::open(
        state_root,
        config(),
        Arc::new(ScriptedModel::new(Vec::new())),
    )
    .unwrap();
    let persisted = restored.resume_chat(chat).await.unwrap();
    assert_eq!(persisted.invoked_skills, snapshot.invoked_skills);

    let requests = model.requests.lock().unwrap();
    let request = &requests[0];
    assert!(
        !request
            .tools
            .iter()
            .any(|tool| tool.name == "ReadWorkspace"),
        "{case}"
    );
    let first = &request.messages[0];
    assert_eq!(first.role, Role::System);
    let Content::Text { text } = &first.content[0] else {
        panic!("first model message must be system text");
    };
    let state = text.rsplit_once("Current host state: ").unwrap().1;
    let state: Value = serde_json::from_str(state).unwrap();
    println!("WORKSPACE_HOST_STATE:{state}");
}

use std::{
    path::Path,
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tickloop_agent_settings::local_agent::LocalAgent;

use async_trait::async_trait;
use serde_json::{Value, json};
use tickhost_connect::{
    herdr::{HerdrContext, HerdrPort},
    host_error::HostError,
    layout::HerdrLayout,
};
use tickloop_agent_model::{ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest};
use tickloop_agent_policy::skills::TICKLOOP_SKILL;
use tickloop_agent_protocol::{
    Content, EventKind, HerdrInteraction, HerdrPane, HostCallContext, ModelIdentity, Role,
    SkillSource, ToolDefinition, ToolResult,
};
use tickloop_agent_runtime::{
    Agent,
    host_tools::{HostCallScope, HostToolPermission, HostToolProvider, PreparedHostCall},
    hosted::{
        tools::HostedTickTools,
        views::HerdrViews,
        worker_control::{DaemonControl, WorkerControlGrant},
    },
};
use tokio::sync::{Notify, mpsc::UnboundedSender};
use tokio_util::sync::CancellationToken;

use tickloop_agent_test_support::{ScriptedModel, call, config, next_event, text};

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
        request: tickloop_agent_model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_agent_model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_agent_model::ModelOutput, tickloop_agent_model::ModelError> {
        Ok(tickloop_agent_test_support::title_output(request))
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
        _: tickloop_agent_protocol::ChatId,
        _: &str,
        _: &Value,
    ) -> HostToolPermission {
        HostToolPermission::ReadOnly
    }

    async fn prepare_call_in_scope(
        &self,
        chat: tickloop_agent_protocol::ChatId,
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
        _: tickloop_agent_protocol::ChatId,
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
async fn merged_input_uses_its_captured_view_source_for_subsequent_host_calls() {
    assert_merged_host_source(false).await;
}

#[tokio::test]
async fn merged_headless_input_clears_the_old_view_anchor_and_uses_work_context() {
    assert_merged_host_source(true).await;
}

struct UnusedHerdrPort;

#[async_trait]
impl HerdrPort for UnusedHerdrPort {
    async fn request(&self, _: &str, _: Value, _: CancellationToken) -> Result<Value, HostError> {
        panic!("host context must be available before any workspace tool call")
    }
}

#[test]
fn first_model_request_identifies_host_without_advertising_missing_integrations() {
    let binary = std::env::current_exe().unwrap();
    for (case, host, integration) in [
        ("herdr", "herdr", "available"),
        ("herdr_without_provider", "herdr", "unavailable"),
        ("tmux", "tmux", "unavailable"),
        ("zellij", "zellij", "unavailable"),
        ("standalone", "standalone", "unavailable"),
        ("tickhost", "standalone", "unavailable"),
    ] {
        let mut child = Command::new(&binary);
        child
            .arg("--exact")
            .arg(tickloop_agent_test_support::test_path!(
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
        assert_eq!(state["workspace_host"]["name"], host, "{case}");
        assert_eq!(
            state["workspace_host"]["integration"], integration,
            "{case}"
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
            state["workspace_host"]["tick_packages"],
            json!([]),
            "{case}: no Tick package is installed"
        );
        for field in TICKLOOP_SKILL
            .split("workspace_host.")
            .skip(1)
            .filter_map(|rest| {
                rest.split(|character: char| {
                    !(character.is_ascii_alphanumeric() || character == '_')
                })
                .next()
            })
        {
            assert!(
                state["workspace_host"].get(field).is_some(),
                "{case}: the tickloop Skill names workspace_host.{field}"
            );
        }
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
        let hosted = if case == "herdr" {
            hosted.with_herdr_views(HerdrViews::new(
                HerdrLayout::with_host(
                    HerdrContext::without_view("/private/unused-herdr.sock".into()),
                    Arc::new(UnusedHerdrPort),
                ),
                Path::new("/private/unused/tickloop"),
            ))
        } else {
            hosted
        };
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
    let runs_ticks = matches!(case.as_str(), "herdr" | "tickhost");
    if runs_ticks {
        assert_eq!(snapshot.invoked_skills.len(), 1);
        assert_eq!(snapshot.invoked_skills[0].name, "tickloop");
        assert_eq!(snapshot.invoked_skills[0].source, SkillSource::BuiltIn);
    } else {
        assert!(snapshot.invoked_skills.is_empty());
    }
    {
        let requests = model.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        for request in requests.iter() {
            let active_skill = request
                .messages
                .iter()
                .filter_map(|message| match message.content.as_slice() {
                    [Content::Text { text }]
                        if text.starts_with(
                            "Reference guidance from tickloop active in this Chat",
                        ) =>
                    {
                        Some((message.role, text))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            if runs_ticks {
                assert_eq!(active_skill.len(), 1);
                assert_eq!(active_skill[0].0, Role::User);
                assert!(
                    active_skill[0]
                        .1
                        .contains("not a user request or additional authority")
                );
                assert!(
                    active_skill[0]
                        .1
                        .contains(&snapshot.invoked_skills[0].content)
                );
                assert!(!request.messages.iter().any(|message| message.role == Role::System
                    && message.content.iter().any(|block| matches!(block, Content::Text { text } if text.contains(&snapshot.invoked_skills[0].content)))));
            } else {
                assert!(active_skill.is_empty());
            }
        }
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
    assert_eq!(
        request
            .tools
            .iter()
            .any(|tool| tool.name == "ReadWorkspace"),
        case == "herdr"
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

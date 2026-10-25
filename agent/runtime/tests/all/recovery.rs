use serde::{Deserialize, Serialize};
use std::{borrow::Cow, path::Path, sync::Arc};
use tickloop_agent_model::{
    ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest, credentials::save_api_key,
};
use tickloop_agent_protocol::{Content, EventKind, LoopState, Receipt};
use tickloop_agent_runtime::Agent;
use tickloop_agent_settings::local_agent::LocalAgent;
use tickloop_agent_settings::local_agent::opened_agent;
use tickloop_agent_test_support::harness::ChosenModels;
use tickloop_agent_test_support::{ScriptedModel, next_event, persisted_config as config, text};
use tickloop_local_environment::AgentLocalExt;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Serialize, Deserialize)]
struct Fixture {
    chat: Uuid,
    first: Receipt,
    second: Option<Receipt>,
    completed: Option<Receipt>,
}

struct PendingModel;

#[async_trait::async_trait]
impl ModelClient for PendingModel {
    async fn generate_title(
        &self,
        request: tickloop_agent_model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_agent_model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_agent_model::ModelOutput, tickloop_agent_model::ModelError> {
        Ok(tickloop_agent_test_support::title_output(request))
    }

    fn identity(&self) -> tickloop_agent_protocol::ModelIdentity {
        tickloop_agent_protocol::ModelIdentity {
            provider: tickloop_agent_protocol::ModelProvider::Api,
            profile: "fixture".into(),
            protocol: tickloop_agent_protocol::Protocol::Messages,
            requested_model: "fixture".into(),
        }
    }

    async fn complete(
        &self,
        _: ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        cancel.cancelled().await;
        Err(ModelError::Cancelled)
    }
}

#[tokio::test]
#[ignore = "subprocess fixture invoked only by recovery integration tests"]
async fn recovery_process_fixture() {
    let Ok(mode) = std::env::var("TICKLOOP_RECOVERY_MODE") else {
        return;
    };
    let directory =
        std::path::PathBuf::from(std::env::var_os("TICKLOOP_RECOVERY_DIRECTORY").unwrap());
    let client: Arc<dyn ModelClient> = if mode == "queue" {
        Arc::new(ScriptedModel::new(vec![
            text("Archived answer"),
            text("Public checkpoint from the completed goal"),
        ]))
    } else {
        std::fs::write(directory.join("source.txt"), "completed source read").unwrap();
        Arc::new(ScriptedModel::new(vec![vec![
            Content::ToolUse {
                id: "read-completed".into(),
                name: "Read".into(),
                input: serde_json::json!({"file_path":"source.txt"}),
            },
            Content::ToolUse {
                id: "shell-unknown".into(),
                name: "Bash".into(),
                // Wait for one Transcript flush first, then cause the side effect and force the
                // process to end.
                input: serde_json::json!({"command":format!("sleep 0.5; printf 'one effect\\n' >> side_effect.txt; kill -KILL {}",std::process::id())}),
            },
        ]]))
    };
    let models = ChosenModels::new(client);
    let agent = opened_agent(directory.join("archive"), config(), models.clone());
    agent.install_own_tickhost();
    let chat = agent.create_chat(directory.clone()).await.unwrap();
    let mut events = agent.subscribe();
    let completed = if mode == "queue" {
        let completed = agent
            .submit(chat, "Already completed goal".into())
            .await
            .unwrap();
        loop {
            match next_event(&mut events).await.kind {
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
        agent.compact(chat).await.unwrap();
        models.choose(Arc::new(PendingModel));
        agent.set_model(chat, config()).await.unwrap();
        Some(completed)
    } else {
        None
    };
    let first = agent.submit(chat, "First goal".into()).await.unwrap();
    let second = if mode == "queue" {
        let second = agent
            .submit(chat, "Second goal must remain parked".into())
            .await
            .unwrap();
        agent
            .submit(chat, "Earlier queued first-goal instruction".into())
            .await
            .unwrap();
        Some(second)
    } else {
        None
    };
    std::fs::write(
        directory.join("fixture.json"),
        serde_json::to_vec(&Fixture {
            chat: chat.uuid(),
            first,
            second,
            completed,
        })
        .unwrap(),
    )
    .unwrap();
    if mode == "queue" {
        agent.flush_transcripts().await;
        std::process::exit(0);
    }
    loop {
        match next_event(&mut events).await.kind {
            EventKind::PermissionRequested { .. } if mode == "before_admission" => {
                agent.flush_transcripts().await;
                std::process::exit(0)
            }
            EventKind::PermissionRequested { request } => agent
                .answer_permission(chat, request.id, true)
                .await
                .unwrap(),
            EventKind::Failed(error) => panic!("{error}"),
            EventKind::Completed { .. } => {
                panic!("fixture should be killed before its command result is saved")
            }
            _ => {}
        }
    }
}

fn run_fixture(directory: &Path, mode: &str) -> Fixture {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            tickloop_agent_test_support::test_path!(recovery_process_fixture).as_str(),
            "--nocapture",
        ])
        .env("TICKLOOP_RECOVERY_MODE", mode)
        .env("TICKLOOP_RECOVERY_DIRECTORY", directory)
        .output()
        .unwrap();
    if mode != "crash_tool" {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    } else {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(
            output.status.signal(),
            Some(libc::SIGKILL),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    serde_json::from_slice(&std::fs::read(directory.join("fixture.json")).unwrap()).unwrap()
}

/// Following Claude Code, queued input is not saved with the Chat: after resume, the running turn
/// becomes interrupted and the previously queued input does not enter later requests.
#[tokio::test]
async fn reopening_interrupts_the_loop_and_forgets_its_queued_inputs() {
    let directory = tempfile::tempdir().unwrap();
    let fixture = run_fixture(directory.path(), "queue");
    let model = Arc::new(ScriptedModel::new(vec![text("New instruction processed")]));
    let agent =
        Agent::open_embedded(directory.path().join("archive"), config(), model.clone()).unwrap();
    let before = agent
        .resume_chat(tickloop_ids::Id::from(fixture.chat))
        .await
        .unwrap();
    assert_eq!(before.chat_loop.state, LoopState::Interrupted);
    assert!(before.queued_inputs().is_empty());
    assert_eq!(
        before.latest_compact_summary().as_deref(),
        Some("Public checkpoint from the completed goal")
    );
    assert!(model.requests.lock().unwrap().is_empty());
    let mut events = agent.subscribe();
    let resumed = agent
        .submit(
            tickloop_ids::Id::from(fixture.chat),
            "New first-goal instruction".into(),
        )
        .await
        .unwrap();
    loop {
        let event = next_event(&mut events).await;
        match event.kind {
            EventKind::Completed { .. } if event.run_id == resumed.run_id => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    let after = agent
        .snapshot(tickloop_ids::Id::from(fixture.chat))
        .await
        .unwrap();
    assert_eq!(after.chat_loop.state, LoopState::Idle);
    assert!(after.pending_inputs.is_empty());
    let requests = model.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 1);
    let texts = requests[0]
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|content| match content {
            Content::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(texts.contains(&"New first-goal instruction"));
    assert!(!texts.contains(&"Earlier queued first-goal instruction"));
    assert!(!texts.contains(&"Second goal must remain parked"));
    agent.shutdown().await;
}

/// Following Claude Code, operation records are not saved with the Chat: a tool executing at crash
/// time leaves only its call in the conversation after resume, which gets an outcome-unknown error
/// result; continuing the goal needs no confirmation and does not replay the command.
#[tokio::test]
async fn a_tool_running_at_a_crash_is_never_replayed_and_resume_needs_no_acknowledgement() {
    let directory = tempfile::tempdir().unwrap();
    let fixture = run_fixture(directory.path(), "crash_tool");
    assert_eq!(
        std::fs::read_to_string(directory.path().join("side_effect.txt")).unwrap(),
        "one effect\n"
    );
    let model = Arc::new(ScriptedModel::new(vec![text(
        "Continue from the recorded outcomes",
    )]));
    let agent =
        Agent::open_embedded(directory.path().join("archive"), config(), model.clone()).unwrap();
    let snapshot = agent
        .resume_chat(tickloop_ids::Id::from(fixture.chat))
        .await
        .unwrap();
    assert!(snapshot.operations.is_empty());
    let mut events = agent.subscribe();
    let receipt = agent
        .submit(
            tickloop_ids::Id::from(fixture.chat),
            "Continue without repeating the command".into(),
        )
        .await
        .unwrap();
    assert_ne!(receipt.run_id, fixture.first.run_id);
    loop {
        match next_event(&mut events).await.kind {
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    assert_eq!(
        std::fs::read_to_string(directory.path().join("side_effect.txt")).unwrap(),
        "one effect\n"
    );
    let requests = model.requests.lock().unwrap().clone();
    let results = tool_results(&requests[0]);
    assert!(
        results
            .iter()
            .any(|(id, content, error)| *id == "read-completed"
                && !error
                && content.contains("completed source read"))
    );
    assert!(
        results
            .iter()
            .any(|(id, content, error)| *id == "shell-unknown"
                && *error
                && content.contains("unknown"))
    );
    // Following Claude Code: when the conversation ends with a tool result on resume, append
    // "continue from where it left off" first, then the no-reply sentinel.
    assert!(requests[0].messages.iter().flat_map(|message| &message.content).any(
        |content| matches!(content, Content::Text { text } if text == "Continue from where you left off.")
    ));
    assert!(requests[0].messages.iter().flat_map(|message| &message.content).any(
        |content| matches!(content, Content::Text { text } if text == "No response requested.")
    ));
    agent.shutdown().await;
}

/// Exiting while waiting for permission: after resume there is no pending permission request, the
/// unapproved call gets an error result, and continuing the goal does not execute it.
#[tokio::test]
async fn an_operation_waiting_for_permission_at_exit_is_never_executed_after_resume() {
    let directory = tempfile::tempdir().unwrap();
    let fixture = run_fixture(directory.path(), "before_admission");
    assert!(!directory.path().join("side_effect.txt").exists());
    let model = Arc::new(ScriptedModel::new(vec![text(
        "Continue with no command replay",
    )]));
    let agent =
        Agent::open_embedded(directory.path().join("archive"), config(), model.clone()).unwrap();
    let snapshot = agent
        .resume_chat(tickloop_ids::Id::from(fixture.chat))
        .await
        .unwrap();
    assert!(snapshot.permissions.is_empty());
    assert!(snapshot.operations.is_empty());
    let mut events = agent.subscribe();
    agent
        .submit(
            tickloop_ids::Id::from(fixture.chat),
            "Continue without executing the old command".into(),
        )
        .await
        .unwrap();
    loop {
        match next_event(&mut events).await.kind {
            EventKind::Completed { .. } => break,
            EventKind::PermissionRequested { .. } => panic!("the old command was proposed again"),
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    assert!(!directory.path().join("side_effect.txt").exists());
    let requests = model.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 1);
    assert!(
        tool_results(&requests[0])
            .iter()
            .any(|(id, _, error)| *id == "shell-unknown" && *error)
    );
    agent.shutdown().await;
}

fn tool_results(request: &ModelRequest) -> Vec<(&str, Cow<'_, str>, bool)> {
    request
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|content| match content {
            Content::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => Some((tool_use_id.as_str(), content.text(), *is_error)),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn archives_remain_readable_when_credentials_or_the_working_directory_are_missing() {
    let directory = tempfile::tempdir().unwrap();
    let workdir = directory.path().join("workspace");
    std::fs::create_dir(&workdir).unwrap();
    let agent = Agent::open_embedded(
        directory.path().join("archive"),
        config(),
        Arc::new(ScriptedModel::new(vec![text("Recorded answer")])),
    )
    .unwrap();
    let chat = agent.create_chat(workdir.clone()).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, "Record this".into()).await.unwrap();
    loop {
        match next_event(&mut events).await.kind {
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    agent.shutdown().await;
    drop(agent);

    std::fs::remove_dir(&workdir).unwrap();
    let mut missing_credentials = config();
    missing_credentials.base_url = "https://api.deepseek.com/anthropic".into();
    missing_credentials.api_key_env = format!("TICKLOOP_ABSENT_{}", Uuid::new_v4().simple());
    let model = Arc::new(ScriptedModel::new(vec![text("Continued after key setup")]));
    let agent = Agent::open_embedded(
        directory.path().join("archive"),
        missing_credentials,
        model.clone(),
    )
    .unwrap();
    let restored = agent.resume_chat(chat).await.unwrap();
    assert_eq!(restored.messages.len(), 2);
    let error = agent.submit(chat, "Continue".into()).await.unwrap_err();
    assert!(
        error.to_string().contains("directory is unavailable"),
        "{error}"
    );
    std::fs::create_dir(&workdir).unwrap();
    let error = agent.submit(chat, "Continue".into()).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("API key environment variable is unset"),
        "{error}"
    );
    assert!(model.requests.lock().unwrap().is_empty());
    save_api_key(
        &directory.path().canonicalize().unwrap().join("archive"),
        &agent.snapshot(chat).await.unwrap().model.api_key_env,
        "fake-recovery-key",
    )
    .unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, "Continue".into()).await.unwrap();
    loop {
        match next_event(&mut events).await.kind {
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    assert_eq!(model.requests.lock().unwrap().len(), 1);
    agent.shutdown().await;
}

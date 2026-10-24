use crate::Agent;
use async_trait::async_trait;
use std::sync::{Arc, Mutex};
use tickloop_agent_model::{ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest};
use tokio::sync::{Notify, mpsc::UnboundedSender};
use tokio_util::sync::CancellationToken;

use tickloop_agent_protocol::{
    ChatId, ChatSnapshot, CommandState, Content, EventKind, InputModality, ModelConfig,
    PauseReason, PendingInput, Protocol, Receipt, Receiving, TriggerSource, Usage, UserShellState,
};
use tickloop_agent_tools::UserCommandSnapshot;

#[derive(Default)]
struct CapturingModel {
    requests: Mutex<Vec<ModelRequest>>,
}

#[derive(Default)]
struct BlockingModel {
    started: Notify,
    release: Notify,
    requests: Mutex<Vec<ModelRequest>>,
}

#[async_trait]
impl ModelClient for BlockingModel {
    async fn generate_title(
        &self,
        request: tickloop_agent_model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_agent_model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_agent_model::ModelOutput, tickloop_agent_model::ModelError> {
        Ok(tickloop_agent_model::fixture_title(request))
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
        request: ModelRequest,
        _: UnboundedSender<ModelDelta>,
        _: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        self.requests.lock().unwrap().push(request);
        self.started.notify_one();
        self.release.notified().await;
        Ok(ModelOutput {
            private_source: None,
            content: vec![Content::Text {
                text: "done".into(),
            }],
            usage: Usage::default(),
            stop_reason: "end_turn".into(),
        })
    }
}

#[async_trait]
impl ModelClient for CapturingModel {
    async fn generate_title(
        &self,
        request: tickloop_agent_model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_agent_model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_agent_model::ModelOutput, tickloop_agent_model::ModelError> {
        Ok(tickloop_agent_model::fixture_title(request))
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
        request: ModelRequest,
        _: UnboundedSender<ModelDelta>,
        _: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        self.requests.lock().unwrap().push(request);
        Ok(ModelOutput {
            private_source: None,
            content: vec![Content::Text {
                text: "done".into(),
            }],
            usage: Usage::default(),
            stop_reason: "end_turn".into(),
        })
    }
}

fn config() -> ModelConfig {
    ModelConfig {
        provider: tickloop_agent_protocol::ModelProvider::Api,
        auth_source: tickloop_agent_protocol::ModelAuthSource::ApiKeyEnv,
        name: "test".into(),
        protocol: Protocol::Messages,
        base_url: "http://127.0.0.1".into(),
        model: "test".into(),
        api_key_env: String::new(),
        max_output_tokens: 1024,
        provider_id: String::new(),
        provider_name: String::new(),
        model_id: String::new(),
        connection: None,
        card: None,
        input_modalities: vec![InputModality::Text],
    }
}

#[tokio::test(start_paused = true)]
async fn restart_discards_a_stale_trigger_input_and_a_new_input_resumes_receiving() {
    let directory = tempfile::tempdir().unwrap();
    let archive = directory.path().join("archive");
    let source = Agent::open_embedded(
        archive.clone(),
        config(),
        Arc::new(CapturingModel::default()),
    )
    .unwrap();
    let chat_id = source.create_chat(directory.path().into()).await.unwrap();
    let loop_id;
    {
        let mut chats = source.inner.chats.lock().await;
        let chat = chats.get_mut(&chat_id).unwrap();
        loop_id = chat.snapshot.chat_loop.id;
        // Transcript 在第一条对话消息写入时才创建，没有消息的 Chat 不会被保存。
        for (role, text) in [
            (tickloop_agent_protocol::Role::User, "watch for events"),
            (tickloop_agent_protocol::Role::Assistant, "watching"),
        ] {
            chat.snapshot
                .messages
                .push(tickloop_agent_protocol::Message {
                    role,
                    content: vec![Content::Text { text: text.into() }],
                });
        }
        chat.snapshot.last_queue_sequence = 1;
        chat.snapshot.pending_inputs.push(PendingInput {
            receipt: Receipt {
                chat_id,
                loop_id,
                run_id: uuid::Uuid::new_v4().into(),
            },
            input: "check after restart".into(),
            contexts: Vec::new(),
            agent_message: None,
            directed_human: None,
            denial_reference: None,
            trigger_source: Some(TriggerSource::ScheduledTask {
                task_id: uuid::Uuid::from_u128(0x5e7),
                scheduled_for_ms: 1,
                wrappers: Vec::new(),
            }),
            armed: true,
            queued_sequence: 1,
            task_continuation: false,
            is_skill_invocation: false,
            attached_file_reads: Vec::new(),
        });
        source.save(&chat.snapshot);
    }
    source.shutdown().await;
    drop(source);

    let model = Arc::new(CapturingModel::default());
    let reopened = Agent::open_embedded(archive, config(), model.clone()).unwrap();
    let recovered = reopened.resume_chat(chat_id).await.unwrap();
    assert!(recovered.pending_inputs.is_empty());
    assert_eq!(
        recovered.chat_loop.receiving,
        Receiving::Paused {
            reason: PauseReason::HistoryLoaded
        }
    );
    assert!(
        reopened
            .deliver_trigger_for_tests(
                chat_id,
                tickloop_agent_protocol::TriggerSource::ScheduledTask {
                    task_id: uuid::Uuid::from_u128(0x5e7),
                    scheduled_for_ms: 2,
                    wrappers: Vec::new(),
                },
                "check after restart"
            )
            .await
            .is_err(),
        "a paused Loop takes no Trigger"
    );

    let mut events = reopened.subscribe();
    let manual = reopened
        .submit(chat_id, "resume this trigger".into())
        .await
        .unwrap();
    loop {
        let event = events.recv().await.unwrap();
        if event.run_id == manual.run_id && matches!(event.kind, EventKind::Completed { .. }) {
            break;
        }
    }
    assert_eq!(model.requests.lock().unwrap().len(), 1);
    assert!(recovered_receiving(&reopened, chat_id).await);
    reopened
        .deliver_trigger_for_tests(
            chat_id,
            tickloop_agent_protocol::TriggerSource::ScheduledTask {
                task_id: uuid::Uuid::from_u128(0x5e7),
                scheduled_for_ms: 3,
                wrappers: Vec::new(),
            },
            "check after resuming",
        )
        .await
        .unwrap();
    loop {
        let event = events.recv().await.unwrap();
        if event.loop_id == loop_id && matches!(event.kind, EventKind::Completed { .. }) {
            break;
        }
    }
    assert_eq!(model.requests.lock().unwrap().len(), 2);
    reopened.shutdown().await;
}

async fn recovered_receiving(agent: &Agent, chat_id: ChatId) -> bool {
    agent
        .snapshot(chat_id)
        .await
        .unwrap()
        .chat_loop
        .receiving
        .is_receiving()
}

#[tokio::test]
async fn direct_user_shell_joins_the_conversation_without_agent_permissions_or_operations() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(CapturingModel::default());
    let agent = Agent::new_embedded(config(), model.clone());
    agent.install_own_tickhost();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut shell_events = agent.subscribe_user_shells();

    let receipt = agent
        .submit_user_shell(
            chat,
            "printf 'direct output'; printf 'created' > direct.txt".into(),
            false,
        )
        .await
        .unwrap();
    let completed = loop {
        let record = tokio::time::timeout(std::time::Duration::from_secs(3), shell_events.recv())
            .await
            .unwrap()
            .unwrap();
        if record.id == receipt.command_id
            && matches!(
                record.state,
                UserShellState::Completed | UserShellState::Failed
            )
        {
            break record;
        }
    };

    assert_eq!(completed.state, UserShellState::Completed);
    assert_eq!(
        std::fs::read_to_string(directory.path().join("direct.txt")).unwrap(),
        "created"
    );
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert!(snapshot.permissions.is_empty());
    assert!(snapshot.operations.is_empty());
    assert!(snapshot.policy.rules.is_empty());
    assert_eq!(snapshot.user_shells.len(), 1);
    assert!(
        conversation_texts(&snapshot)
            .iter()
            .all(|text| !text.contains("<task-notification>"))
    );

    let output = agent
        .read_user_shell_output(chat, receipt.command_id, 0, 1024)
        .await
        .unwrap();
    let output: serde_json::Value = serde_json::from_str(&output.content).unwrap();
    assert_eq!(output["origin"], "user_shell");
    assert_eq!(output["output"], "direct output");

    let mut events = agent.subscribe();
    agent
        .submit(chat, "Explain the command result".into())
        .await
        .unwrap();
    loop {
        if matches!(
            events.recv().await.unwrap().kind,
            EventKind::Completed { .. }
        ) {
            break;
        }
    }
    {
        let requests = model.requests.lock().unwrap();
        let rendered = requests
            .last()
            .unwrap()
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(|content| match content {
                Content::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains(
            "<bash-input>printf 'direct output'; printf 'created' > direct.txt</bash-input>"
        ));
        assert!(rendered.contains("<bash-stdout>direct output</bash-stdout>"));
        assert!(rendered.contains("Explain the command result"));
    }
    agent.shutdown().await;
}

#[tokio::test]
async fn stale_background_update_cannot_replace_a_user_terminal_fact() {
    let directory = tempfile::tempdir().unwrap();
    let agent = Agent::new_embedded(config(), Arc::new(CapturingModel::default()));
    agent.install_own_tickhost();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut shell_events = agent.subscribe_user_shells();
    let receipt = agent
        .submit_user_shell(chat, "printf final".into(), false)
        .await
        .unwrap();
    let terminal = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let record = shell_events.recv().await.unwrap();
            if record.id == receipt.command_id
                && matches!(
                    record.state,
                    UserShellState::Completed | UserShellState::Failed | UserShellState::Unknown
                )
            {
                break record;
            }
        }
    })
    .await;
    let terminal = match terminal {
        Ok(record) => record,
        Err(_) => panic!(
            "user shell did not reach a terminal state; snapshot: {:?}",
            agent.snapshot(chat).await.unwrap().user_shells
        ),
    };
    assert_eq!(terminal.state, UserShellState::Completed, "{terminal:?}");
    let terminal_output_ref = terminal.output_ref.clone().unwrap();
    let authoritative = agent
        .record_user_shell(
            chat,
            UserCommandSnapshot {
                updated_at_ms: None,
                operation_id: receipt.command_id,
                chat_id: chat.into(),
                command: terminal.command.clone(),
                cwd: terminal.cwd.clone(),
                pid: terminal.pid,
                state: CommandState::Background,
                was_background: true,
                backgrounded_on_timeout: false,
                exit_code: None,
                signal: None,
                reason: None,
                output_ref: directory.path().join("stale.log"),
                output_bytes: 0,
            },
        )
        .await
        .unwrap();
    assert_eq!(authoritative.state, UserShellState::Completed);
    assert_eq!(authoritative.exit_code, Some(0));
    assert_eq!(authoritative.output_ref, Some(terminal_output_ref));
    assert_eq!(authoritative.output_bytes, terminal.output_bytes);
    assert!(terminal.updated_at_ms.is_some());
    assert_eq!(
        authoritative.updated_at_ms, terminal.updated_at_ms,
        "a stale snapshot must preserve the completed Shell update time"
    );
    assert!(authoritative.starts_in_background);
    assert_eq!(
        conversation_texts(&agent.snapshot(chat).await.unwrap()),
        [
            "<bash-input>printf final</bash-input>",
            "<bash-stdout>final</bash-stdout><bash-stderr></bash-stderr>",
        ]
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn a_user_shell_can_be_backgrounded_and_stopped_without_a_loop_identity() {
    let directory = tempfile::tempdir().unwrap();
    let agent = Agent::new_embedded(config(), Arc::new(CapturingModel::default()));
    agent.install_own_tickhost();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut shell_events = agent.subscribe_user_shells();
    let receipt = agent
        .submit_user_shell(chat, "while :; do printf x; sleep 0.02; done".into(), false)
        .await
        .unwrap();

    loop {
        let record = shell_events.recv().await.unwrap();
        if record.id == receipt.command_id
            && record.state == UserShellState::Running
            && record.output_bytes > 0
        {
            break;
        }
    }
    let background = agent
        .background_user_shell(chat, receipt.command_id)
        .await
        .unwrap();
    assert_eq!(background.state, UserShellState::Background);
    let background_update = loop {
        let record = shell_events.recv().await.unwrap();
        if record.id == receipt.command_id && record.output_bytes > background.output_bytes {
            break record;
        }
    };
    assert_eq!(background_update.state, UserShellState::Background);
    assert!(agent.stop_command(chat, receipt.command_id).await.is_err());
    assert!(matches!(
        agent
            .snapshot(chat)
            .await
            .unwrap()
            .user_shells
            .iter()
            .find(|record| record.id == receipt.command_id)
            .unwrap()
            .state,
        UserShellState::Background | UserShellState::Running
    ));
    let stopped = agent
        .stop_user_shell(chat, receipt.command_id)
        .await
        .unwrap();
    assert_eq!(stopped.state, UserShellState::Failed);
    assert_eq!(stopped.reason.as_deref(), Some("cancelled"));
    let snapshot = agent.snapshot(chat).await.unwrap();
    let texts = conversation_texts(&snapshot);
    let output_ref = stopped.output_ref.unwrap();
    assert_eq!(
        texts,
        [
            "<bash-input>while :; do printf x; sleep 0.02; done</bash-input>".to_string(),
            format!(
                "<bash-stdout>Command was manually backgrounded by user with ID: {}. Output is being written to: {}</bash-stdout><bash-stderr></bash-stderr>",
                tickloop_ids::IdContext::new().model_text(
                    tickloop_agent_protocol::ShellTaskId::from(receipt.command_id)
                ),
                output_ref.display()
            ),
            format!(
                "<task-notification>\n<task-id>{}</task-id>\n<output-file>{}</output-file>\n<status>killed</status>\n<summary>Background command \"while :; do printf x; sleep 0.02; done\" was stopped</summary>\n</task-notification>",
                tickloop_ids::IdContext::new().model_text(
                    tickloop_agent_protocol::ShellTaskId::from(receipt.command_id)
                ),
                output_ref.display()
            ),
        ]
    );
    assert!(snapshot.chat_loop.run_id.is_none());
    agent.shutdown().await;
}

#[tokio::test]
async fn a_user_shell_moved_by_its_timeout_is_announced_as_running_in_the_background() {
    let directory = tempfile::tempdir().unwrap();
    let agent = Agent::new_embedded(config(), Arc::new(CapturingModel::default()));
    agent.install_own_tickhost();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut shell_events = agent.subscribe_user_shells();
    let command = "while [ ! -e never ]; do sleep 0.05; done";
    let receipt = agent
        .submit_user_shell(chat, command.into(), false)
        .await
        .unwrap();
    let (running, output_ref) = loop {
        let record = shell_events.recv().await.unwrap();
        if record.id == receipt.command_id
            && record.state == UserShellState::Running
            && let Some(output_ref) = record.output_ref.clone()
        {
            break (record, output_ref);
        }
    };
    agent
        .record_user_shell(
            chat,
            UserCommandSnapshot {
                updated_at_ms: None,
                operation_id: receipt.command_id,
                chat_id: chat.into(),
                command: command.into(),
                cwd: running.cwd.clone(),
                pid: running.pid,
                state: CommandState::Background,
                was_background: true,
                backgrounded_on_timeout: true,
                exit_code: None,
                signal: None,
                reason: None,
                output_ref: output_ref.clone(),
                output_bytes: 0,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        conversation_texts(&agent.snapshot(chat).await.unwrap()),
        [
            format!("<bash-input>{command}</bash-input>"),
            format!(
                "<bash-stdout>Command running in background with ID: {}. Output is being written to: {}</bash-stdout><bash-stderr></bash-stderr>",
                tickloop_ids::IdContext::new().model_text(
                    tickloop_agent_protocol::ShellTaskId::from(receipt.command_id)
                ),
                output_ref.display()
            ),
        ]
    );
    agent
        .stop_user_shell(chat, receipt.command_id)
        .await
        .unwrap();
    agent.shutdown().await;
}

#[tokio::test]
async fn a_background_notification_follows_messages_that_finished_before_it() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(CapturingModel::default());
    let agent = Agent::new_embedded(config(), model.clone());
    agent.install_own_tickhost();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut shell_events = agent.subscribe_user_shells();
    agent
        .submit_user_shell(chat, "sleep 0.1; printf 'late shell output'".into(), true)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    let first = agent.submit(chat, "first prompt".into()).await.unwrap();
    loop {
        let event = events.recv().await.unwrap();
        if event.run_id == first.run_id && matches!(event.kind, EventKind::Completed { .. }) {
            break;
        }
    }
    loop {
        let record = shell_events.recv().await.unwrap();
        if record.state == UserShellState::Completed {
            break;
        }
    }
    let second = agent.submit(chat, "second prompt".into()).await.unwrap();
    loop {
        let event = events.recv().await.unwrap();
        if event.run_id == second.run_id && matches!(event.kind, EventKind::Completed { .. }) {
            break;
        }
    }

    {
        let requests = model.requests.lock().unwrap();
        let messages = &requests.last().unwrap().messages;
        let position = |predicate: fn(&str) -> bool| {
            messages
                .iter()
                .position(|message| {
                    message
                        .content
                        .iter()
                        .any(|content| matches!(content, Content::Text { text } if predicate(text)))
                })
                .unwrap()
        };
        let shell_input = position(|text| text.starts_with("<bash-input>"));
        let first_prompt = position(|text| text == "first prompt");
        let shell_result = position(|text| text.starts_with("<task-notification>"));
        let second_prompt = position(|text| text == "second prompt");
        assert!(shell_input < first_prompt);
        assert!(first_prompt < shell_result);
        assert!(shell_result < second_prompt);
    }
    agent.shutdown().await;
}

#[tokio::test]
async fn shells_submitted_while_agent_is_busy_are_persisted_and_run_in_order() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(BlockingModel::default());
    let agent = Agent::new_embedded(config(), model.clone());
    agent.install_own_tickhost();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut shell_events = agent.subscribe_user_shells();

    agent
        .submit(chat, "hold the execution slot".into())
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), model.started.notified())
        .await
        .unwrap();

    let first = agent
        .submit_user_shell(chat, "printf 1 >> order.txt".into(), false)
        .await
        .unwrap();
    let second = agent
        .submit_user_shell(chat, "printf 2 >> order.txt".into(), false)
        .await
        .unwrap();
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(snapshot.user_shells.len(), 2);
    assert!(
        snapshot
            .user_shells
            .iter()
            .all(|record| record.state == UserShellState::Queued)
    );
    assert!(!directory.path().join("order.txt").exists());

    model.release.notify_one();
    let mut completed = std::collections::HashSet::new();
    while completed.len() < 2 {
        let record = tokio::time::timeout(std::time::Duration::from_secs(3), shell_events.recv())
            .await
            .unwrap()
            .unwrap();
        if matches!(
            record.state,
            UserShellState::Completed | UserShellState::Failed
        ) {
            completed.insert(record.id);
        }
    }
    assert!(completed.contains(&first.command_id));
    assert!(completed.contains(&second.command_id));
    assert_eq!(
        std::fs::read_to_string(directory.path().join("order.txt")).unwrap(),
        "12"
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn shell_and_model_inputs_keep_their_typed_submission_order() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(BlockingModel::default());
    let agent = Agent::new_embedded(config(), model.clone());
    agent.install_own_tickhost();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();

    agent.submit(chat, "prompt-before".into()).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), model.started.notified())
        .await
        .unwrap();
    let shell_1 = agent
        .submit_user_shell(chat, "printf 1 >> typed-order.txt".into(), false)
        .await
        .unwrap();
    let prompt_between = agent.submit(chat, "prompt-between".into()).await.unwrap();
    let shell_2 = agent
        .submit_user_shell(chat, "printf 2 >> typed-order.txt".into(), false)
        .await
        .unwrap();
    let prompt_after = agent.submit(chat, "prompt-after".into()).await.unwrap();

    let snapshot = agent.snapshot(chat).await.unwrap();
    let first = snapshot
        .user_shells
        .iter()
        .find(|record| record.id == shell_1.command_id)
        .unwrap();
    let second = snapshot
        .user_shells
        .iter()
        .find(|record| record.id == shell_2.command_id)
        .unwrap();
    assert!(first.preceding_model_runs.is_empty());
    assert_eq!(second.preceding_model_runs, vec![prompt_between.run_id]);
    assert!(!second.preceding_model_runs.contains(&prompt_after.run_id));

    model.release.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(3), model.started.notified())
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(directory.path().join("typed-order.txt")).unwrap(),
        "1"
    );

    model.release.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(3), model.started.notified())
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(directory.path().join("typed-order.txt")).unwrap(),
        "12"
    );

    model.release.notify_one();
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(3), events.recv())
            .await
            .unwrap()
            .unwrap();
        if event.run_id == prompt_after.run_id && matches!(event.kind, EventKind::Completed { .. })
        {
            break;
        }
    }
    {
        let requests = model.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        for (request, expected) in
            requests
                .iter()
                .zip(["prompt-before", "prompt-between", "prompt-after"])
        {
            assert!(request.messages.iter().any(|message| {
                message
                    .content
                    .iter()
                    .any(|content| matches!(content, Content::Text { text } if text == expected))
            }));
        }
    }
    agent.shutdown().await;
}

#[tokio::test]
async fn restart_forgets_queued_shell_state_and_does_not_leave_chat_busy() {
    let directory = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let model = Arc::new(BlockingModel::default());
    let source = Agent::open_embedded(store.path().into(), config(), model.clone()).unwrap();
    source.install_own_tickhost();
    let chat = source.create_chat(directory.path().into()).await.unwrap();
    source
        .submit(chat, "hold the execution slot".into())
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), model.started.notified())
        .await
        .unwrap();
    source
        .submit_user_shell(chat, "printf stale > stale.txt".into(), false)
        .await
        .unwrap();
    assert_eq!(
        source.snapshot(chat).await.unwrap().user_shells[0].state,
        UserShellState::Queued
    );
    source.shutdown().await;
    drop(source);

    let recovered = Agent::open_embedded(
        store.path().into(),
        config(),
        Arc::new(CapturingModel::default()),
    )
    .unwrap();
    recovered.install_own_tickhost();
    let snapshot = recovered.resume_chat(chat).await.unwrap();
    assert!(snapshot.user_shells.is_empty());
    assert!(
        conversation_texts(&snapshot)
            .iter()
            .all(|text| !text.contains("stale"))
    );
    let mut shell_events = recovered.subscribe_user_shells();
    let fresh = recovered
        .submit_user_shell(chat, "printf fresh > fresh.txt".into(), false)
        .await
        .unwrap();
    loop {
        let record = tokio::time::timeout(std::time::Duration::from_secs(3), shell_events.recv())
            .await
            .unwrap()
            .unwrap();
        if record.id == fresh.command_id && record.state == UserShellState::Completed {
            break;
        }
    }
    assert!(directory.path().join("fresh.txt").exists());
    assert!(!directory.path().join("stale.txt").exists());
    recovered.shutdown().await;
}

fn conversation_texts(snapshot: &ChatSnapshot) -> Vec<String> {
    snapshot
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|content| match content {
            Content::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

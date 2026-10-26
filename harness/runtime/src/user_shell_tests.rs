use crate::Agent;
use async_trait::async_trait;
use std::sync::{Arc, Mutex};
use tickloop_harness_model::{ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use tickloop_harness_protocol::{
    ChatId, ChatSnapshot, CommandState, Content, EventKind, InputModality, ModelConfig,
    PauseReason, PendingInput, Protocol, Receipt, Receiving, TriggerSource, Usage, UserShellState,
};
use tickloop_harness_tools::UserCommandSnapshot;

#[derive(Default)]
struct CapturingModel {
    requests: Mutex<Vec<ModelRequest>>,
}

#[async_trait]
impl ModelClient for CapturingModel {
    async fn generate_title(
        &self,
        request: tickloop_harness_model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_harness_model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_harness_model::ModelOutput, tickloop_harness_model::ModelError> {
        Ok(tickloop_harness_model::fixture_title(request))
    }

    fn identity(&self) -> tickloop_harness_protocol::ModelIdentity {
        tickloop_harness_protocol::ModelIdentity {
            provider: tickloop_harness_protocol::ModelProvider::Api,
            profile: "fixture".into(),
            protocol: tickloop_harness_protocol::Protocol::Messages,
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
        provider: tickloop_harness_protocol::ModelProvider::Api,
        auth_source: tickloop_harness_protocol::ModelAuthSource::ApiKeyEnv,
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

async fn recovered_receiving(agent: &Agent, chat_id: ChatId) -> bool {
    agent
        .snapshot(chat_id)
        .await
        .unwrap()
        .chat_loop
        .receiving
        .is_receiving()
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
        // The Transcript is created only when the first conversation message is written; a Chat
        // with no messages is not saved.
        for (role, text) in [
            (tickloop_harness_protocol::Role::User, "watch for events"),
            (tickloop_harness_protocol::Role::Assistant, "watching"),
        ] {
            chat.snapshot
                .messages
                .push(tickloop_harness_protocol::Message {
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
                tickloop_harness_protocol::TriggerSource::ScheduledTask {
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
            tickloop_harness_protocol::TriggerSource::ScheduledTask {
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

fn shell_agent() -> Agent {
    let environment = Arc::new(crate::environment::memory::MemoryEnvironment::new());
    environment.define_shell(
        "printf final",
        std::time::Duration::ZERO,
        b"final".to_vec(),
        0,
    );
    environment.define_shell(
        "while [ ! -e never ]; do sleep 0.05; done",
        std::time::Duration::from_secs(60),
        Vec::new(),
        0,
    );
    Agent::with_seams(
        crate::HarnessSeams {
            search: None,
            transcripts: None,
            environment: Some(environment),
            models: tickloop_harness_test_support::harness::ChosenModels::new(Arc::new(
                CapturingModel::default(),
            )),
        },
        crate::HarnessOptions::local(config()),
    )
    .unwrap()
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
#[tokio::test]
async fn stale_background_update_cannot_replace_a_user_terminal_fact() {
    let directory = tempfile::tempdir().unwrap();
    let agent = shell_agent();
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
async fn a_user_shell_moved_by_its_timeout_is_announced_as_running_in_the_background() {
    let directory = tempfile::tempdir().unwrap();
    let agent = shell_agent();
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
                    tickloop_harness_protocol::ShellTaskId::from(receipt.command_id)
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

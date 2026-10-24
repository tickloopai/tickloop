use super::{Agent, AgentError};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tickloop_agent_model::{ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest};
use tickloop_agent_protocol::{
    Content, ExecutionMode, InputModality, LoopSnapshot, LoopState, Message, ModelConfig,
    ModelIdentity, ModelRequestKind, OperationRecord, OperationState, Protocol, Receipt,
    RequestOutcome, Role, Usage,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// 报告用量后永不完成的独立权限检查模型。
struct Checker {
    calls: AtomicUsize,
    dropped: AtomicBool,
}

impl Checker {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            dropped: AtomicBool::new(false),
        })
    }
}

struct CompletionGuard<'a>(&'a AtomicBool);

impl Drop for CompletionGuard<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl ModelClient for Checker {
    async fn generate_title(
        &self,
        request: tickloop_agent_model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_agent_model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_agent_model::ModelOutput, tickloop_agent_model::ModelError> {
        Ok(tickloop_agent_model::fixture_title(request))
    }

    fn identity(&self) -> ModelIdentity {
        ModelIdentity {
            provider: tickloop_agent_protocol::ModelProvider::Api,
            profile: "independent permission checker".into(),
            protocol: Protocol::Responses,
            requested_model: "checker-model".into(),
        }
    }

    async fn complete(
        &self,
        _: ModelRequest,
        events: mpsc::UnboundedSender<ModelDelta>,
        _: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        let _guard = CompletionGuard(&self.dropped);
        self.calls.fetch_add(1, Ordering::SeqCst);
        events.send(ModelDelta::Usage(reported_usage())).unwrap();
        std::future::pending().await
    }
}

struct UnusedConversationModel;

#[async_trait::async_trait]
impl ModelClient for UnusedConversationModel {
    async fn generate_title(
        &self,
        request: tickloop_agent_model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_agent_model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_agent_model::ModelOutput, tickloop_agent_model::ModelError> {
        Ok(tickloop_agent_model::fixture_title(request))
    }

    fn identity(&self) -> ModelIdentity {
        ModelIdentity {
            provider: tickloop_agent_protocol::ModelProvider::Api,
            profile: "conversation profile".into(),
            protocol: Protocol::Messages,
            requested_model: "conversation-model".into(),
        }
    }

    async fn complete(
        &self,
        _: ModelRequest,
        _: mpsc::UnboundedSender<ModelDelta>,
        _: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        panic!("this test must not call the conversation model");
    }
}

fn reported_usage() -> Usage {
    Usage {
        input_tokens: Some(19),
        output_tokens: Some(4),
        cache_read_input_tokens: Some(0),
        cache_write_input_tokens: None,
    }
}

async fn fixture(checker: Arc<Checker>) -> (tempfile::TempDir, Agent, Receipt, CancellationToken) {
    let directory = tempfile::tempdir().unwrap();
    let config = ModelConfig {
        provider: tickloop_agent_protocol::ModelProvider::Api,
        auth_source: tickloop_agent_protocol::ModelAuthSource::ApiKeyEnv,
        name: "conversation profile".into(),
        protocol: Protocol::Messages,
        base_url: "http://127.0.0.1".into(),
        model: "conversation-model".into(),
        api_key_env: String::new(),
        max_output_tokens: 256,
        provider_id: String::new(),
        provider_name: String::new(),
        model_id: String::new(),
        connection: None,
        card: None,
        input_modalities: vec![InputModality::Text],
    };
    let agent = Agent::open_embedded(
        directory.path().join("state"),
        config,
        Arc::new(UnusedConversationModel),
    )
    .unwrap();
    let chat_id = agent.create_chat(directory.path().into()).await.unwrap();
    let receipt = Receipt {
        chat_id,
        loop_id: Uuid::new_v4().into(),
        run_id: Uuid::new_v4().into(),
    };
    let cancel = CancellationToken::new();
    {
        let mut chats = agent.inner.chats.lock().await;
        let chat = chats.get_mut(&chat_id).unwrap();
        chat.snapshot.chat_loop = LoopSnapshot {
            state: LoopState::Running,
            run_id: Some(receipt.run_id),
            ..LoopSnapshot::new(receipt.loop_id)
        };
        chat.snapshot.message_runs.insert(0, receipt.run_id);
        chat.bookkeeping
            .run_loops
            .insert(receipt.run_id, receipt.loop_id);
        chat.snapshot.message_ids.insert(0, Uuid::new_v4());
        chat.snapshot
            .prompt_execution_modes
            .insert(0, ExecutionMode::Auto);
        chat.snapshot.messages.push(Message {
            role: Role::User,
            content: vec![Content::Text {
                text: "Inspect the directory".into(),
            }],
        });
        chat.active = Some(cancel.clone());
        chat.active_run = Some(receipt.run_id);
        agent.save(&chat.snapshot);
    }
    agent.set_auto_reviewer(checker).await;
    (directory, agent, receipt, cancel)
}

fn operation(receipt: &Receipt) -> OperationRecord {
    OperationRecord {
        task_binding: None,
        id: Uuid::new_v4(),
        loop_id: receipt.loop_id,
        run_id: receipt.run_id,
        call_id: "fixture-call".into(),
        name: "Bash".into(),
        input: json!({"command":"pwd"}),
        file_change: None,
        state: OperationState::Proposed,
        result: None,
    }
}

#[tokio::test]
async fn a_cancelled_auto_classifier_does_not_deny_and_keeps_reported_usage() {
    let checker = Checker::new();
    let (_directory, agent, receipt, cancel) = fixture(checker.clone()).await;
    let mut events = agent.subscribe();
    let review = {
        let agent = agent.clone();
        let receipt = receipt.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            agent
                .review_auto_operation(&receipt, &operation(&receipt), None, cancel)
                .await
        })
    };
    while checker.calls.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
    cancel.cancel();
    let review = review.await.unwrap();
    assert!(matches!(
        review,
        Err(AgentError::Model(ModelError::Cancelled))
    ));
    assert!(checker.dropped.load(Ordering::SeqCst));
    assert_eq!(checker.calls.load(Ordering::SeqCst), 1);
    assert!(events.try_recv().is_err());
    let snapshot = agent.snapshot(receipt.chat_id).await.unwrap();
    assert_eq!(snapshot.request_usage.entries().len(), 1);
    let entry = &snapshot.request_usage.entries()[0];
    assert_eq!(entry.kind, ModelRequestKind::PermissionCheck);
    assert_eq!(entry.loop_id, Some(receipt.loop_id));
    assert_eq!(entry.run_id, Some(receipt.run_id));
    assert_eq!(entry.model, checker.identity());
    assert_eq!(entry.outcome, RequestOutcome::Cancelled);
    assert_eq!(entry.usage, reported_usage());
    assert_eq!(
        snapshot
            .request_usage
            .run_conversation_usage(receipt.run_id)
            .unwrap(),
        Usage::default()
    );
    agent.shutdown().await;
}

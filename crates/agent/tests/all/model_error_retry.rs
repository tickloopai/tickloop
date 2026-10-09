use crate::support;

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use tickloop_agent::runtime::{Agent, EventKind, RequestFailure, RequestOutcome};
use tickloop_agent_model::{ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest};
use tickloop_agent_protocol::{Content, ModelIdentity, Role, Usage};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

enum Reply {
    Error(ModelError),
    PartialError(&'static str, ModelError),
    Success(&'static str),
}

struct FailingModel {
    replies: Mutex<VecDeque<Reply>>,
    requests: Mutex<Vec<ModelRequest>>,
}

impl FailingModel {
    fn new(replies: Vec<Reply>) -> Self {
        Self {
            replies: Mutex::new(replies.into()),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

#[async_trait]
impl ModelClient for FailingModel {
    async fn generate_title(
        &self,
        request: tickloop_agent_model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_agent_model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_agent_model::ModelOutput, tickloop_agent_model::ModelError> {
        Ok(support::title_output(request))
    }

    fn identity(&self) -> ModelIdentity {
        ModelIdentity {
            provider: tickloop_agent_protocol::ModelProvider::Api,
            profile: "fixture".into(),
            protocol: tickloop_agent_protocol::Protocol::Messages,
            requested_model: "fixture".into(),
        }
    }

    async fn complete(
        &self,
        request: ModelRequest,
        events: UnboundedSender<ModelDelta>,
        _: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        self.requests.lock().unwrap().push(request);
        let reply = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected model request");
        match reply {
            Reply::Error(error) => Err(error),
            Reply::PartialError(text, error) => {
                events.send(ModelDelta::Text(text.into())).unwrap();
                Err(error)
            }
            Reply::Success(text) => {
                events.send(ModelDelta::Text(text.into())).unwrap();
                Ok(ModelOutput {
                    private_source: None,
                    content: vec![Content::Text { text: text.into() }],
                    usage: Usage::default(),
                    stop_reason: "end_turn".into(),
                })
            }
        }
    }
}

#[tokio::test]
async fn interrupted_stream_is_not_replayed_after_text_was_shown() {
    let model = Arc::new(FailingModel::new(vec![Reply::PartialError(
        "Visible prefix",
        ModelError::StreamInterrupted,
    )]));
    let agent = Agent::new_embedded(support::config(), model.clone());
    let (completed, error) = run_until_finished(&agent, &model).await;
    assert!(!completed);
    assert!(error.contains("stream ended"));
    assert_eq!(model.request_count(), 1);
    agent.shutdown().await;
}

#[tokio::test]
async fn user_interrupt_cancels_the_retry_wait() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(FailingModel::new(vec![Reply::Error(ModelError::Http {
        status: 503,
    })]));
    let agent = Agent::new_embedded(support::config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, None, "Answer".into()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = events.recv().await.unwrap();
            if event.chat_id == chat
                && let EventKind::ModelRetry {
                    error,
                    attempt,
                    max_retries,
                    ..
                } = event.kind
            {
                assert!(error.contains("HTTP 503"));
                assert_eq!(attempt, 1);
                assert_eq!(max_retries, 10);
                break;
            }
        }
    })
    .await
    .expect("retry was not scheduled");
    agent.interrupt(chat).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let event = events.recv().await.unwrap();
            if event.chat_id == chat && matches!(event.kind, EventKind::Interrupted) {
                break;
            }
        }
    })
    .await
    .expect("retry wait did not stop");
    assert_eq!(model.request_count(), 1);
    agent.shutdown().await;
}

async fn run_until_finished(agent: &Agent, model: &Arc<FailingModel>) -> (bool, String) {
    let directory = tempfile::tempdir().unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, None, "Answer".into()).await.unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = events.recv().await.unwrap();
            if event.chat_id == chat {
                match event.kind {
                    EventKind::Completed { .. } => break (true, String::new()),
                    EventKind::Failed(error) => break (false, error),
                    _ => {}
                }
            }
        }
    })
    .await
    .expect("run did not finish");
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(
        support::model_script_requests(&snapshot).len(),
        model.request_count()
    );
    if outcome.0 {
        assert_eq!(
            snapshot
                .messages
                .iter()
                .filter(|message| message.role == Role::Assistant)
                .count(),
            1
        );
    }
    outcome
}

#[tokio::test]
async fn transient_http_error_retries_and_records_each_attempt() {
    let model = Arc::new(FailingModel::new(vec![
        Reply::Error(ModelError::Http { status: 503 }),
        Reply::Success("Recovered"),
    ]));
    let agent = Agent::new_embedded(support::config(), model.clone());
    let (completed, error) = run_until_finished(&agent, &model).await;
    assert!(completed, "{error}");
    assert_eq!(model.request_count(), 2);
    let snapshot = &agent.list_chats().await[0];
    assert_eq!(
        support::model_script_requests(snapshot)[0].outcome,
        RequestOutcome::Failed(RequestFailure::Http { status: 503 })
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn authentication_error_fails_without_repeating_the_same_key() {
    let model = Arc::new(FailingModel::new(vec![Reply::Error(
        ModelError::Authentication,
    )]));
    let agent = Agent::new_embedded(support::config(), model.clone());
    let (completed, error) = run_until_finished(&agent, &model).await;
    assert!(!completed);
    assert!(error.contains("authentication"));
    assert_eq!(model.request_count(), 1);
    agent.shutdown().await;
}

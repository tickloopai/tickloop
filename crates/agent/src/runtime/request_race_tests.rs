use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use super::{
    Agent, AgentError, ChatId, ProtocolFailure, RequestFailure, RequestOutcome, UsageCounter,
    model_request::RequestContext,
};
use crate::model::{
    Content, InputModality, ModelClient, ModelConfig, ModelDelta, ModelError, ModelIdentity,
    ModelOutput, ModelRequest, Protocol, Usage,
};

#[derive(Default)]
struct RaceModel {
    observations: Vec<Usage>,
    on_drop: Option<Usage>,
    response: Option<ModelOutput>,
    cancel_on_poll: Option<CancellationToken>,
    calls: AtomicUsize,
    dropped: Arc<AtomicBool>,
    drop_usage_delivered: Arc<AtomicBool>,
}

struct DropObservation {
    usage: Option<Usage>,
    events: UnboundedSender<ModelDelta>,
    dropped: Arc<AtomicBool>,
    delivered: Arc<AtomicBool>,
}

impl Drop for DropObservation {
    fn drop(&mut self) {
        if let Some(usage) = self.usage.take() {
            self.delivered.store(
                self.events.send(ModelDelta::Usage(usage)).is_ok(),
                Ordering::SeqCst,
            );
        }
        self.dropped.store(true, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl ModelClient for RaceModel {
    async fn generate_title(
        &self,
        request: crate::model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<crate::model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<crate::model::ModelOutput, crate::model::ModelError> {
        Ok(crate::model::fixture_title(request))
    }

    fn identity(&self) -> ModelIdentity {
        ModelIdentity {
            provider: crate::model::ModelProvider::Api,
            profile: "usage-race".into(),
            protocol: Protocol::Messages,
            requested_model: "usage-race-model".into(),
        }
    }

    async fn complete(
        &self,
        _: ModelRequest,
        events: UnboundedSender<ModelDelta>,
        _: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let _drop = DropObservation {
            usage: self.on_drop.clone(),
            events: events.clone(),
            dropped: self.dropped.clone(),
            delivered: self.drop_usage_delivered.clone(),
        };
        for observation in &self.observations {
            events.send(ModelDelta::Usage(observation.clone())).unwrap();
        }
        if let Some(cancel) = &self.cancel_on_poll {
            cancel.cancel();
        }
        match &self.response {
            Some(response) => Ok(response.clone()),
            None => std::future::pending().await,
        }
    }
}

async fn fixture(model: Arc<RaceModel>) -> (tempfile::TempDir, Agent, ChatId) {
    let directory = tempfile::tempdir().unwrap();
    let identity = model.identity();
    let config = ModelConfig {
        provider: crate::model::ModelProvider::Api,
        auth_source: crate::model::ModelAuthSource::ApiKeyEnv,
        name: identity.profile,
        protocol: identity.protocol,
        model: identity.requested_model,
        base_url: "http://127.0.0.1".into(),
        api_key_env: String::new(),
        max_output_tokens: 256,
        provider_id: String::new(),
        provider_name: String::new(),
        model_id: String::new(),
        connection: None,
        card: None,
        input_modalities: vec![InputModality::Text],
    };
    let agent = Agent::new_embedded(config, model);
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    (directory, agent, chat)
}

fn empty_request() -> ModelRequest {
    ModelRequest {
        history_sources: Vec::new(),
        messages: Vec::new(),
        tools: Vec::new(),
        structured_output: None,
        temperature: None,
        reasoning_effort: None,
    }
}

#[tokio::test(start_paused = true)]
async fn immediate_complete_response_wins_over_a_zero_length_deadline() {
    let usage = Usage {
        input_tokens: Some(17),
        output_tokens: Some(3),
        cache_read_input_tokens: Some(0),
        cache_write_input_tokens: None,
    };
    let model = Arc::new(RaceModel {
        observations: vec![usage.clone()],
        response: Some(ModelOutput {
            private_source: None,
            content: vec![Content::Text {
                text: "public summary".into(),
            }],
            usage: usage.clone(),
            stop_reason: "end_turn".into(),
        }),
        ..RaceModel::default()
    });
    let (_directory, agent, chat) = fixture(model.clone()).await;
    let pending = agent
        .tracked_model_request(
            RequestContext::manual_compaction(chat),
            model.clone(),
            empty_request(),
            CancellationToken::new(),
            Some(Duration::ZERO),
        )
        .await
        .unwrap();
    assert_eq!(pending.output.usage, usage);
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    assert!(model.dropped.load(Ordering::SeqCst));
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(snapshot.request_usage.entries().len(), 1);
    assert_eq!(
        snapshot.request_usage.entries()[0].outcome,
        RequestOutcome::InFlight
    );
    assert_eq!(snapshot.request_usage.entries()[0].usage, usage);
    agent
        .finish_model_request(chat, pending.sequence, RequestOutcome::Accepted)
        .await
        .unwrap();
    assert_eq!(
        agent.snapshot(chat).await.unwrap().request_usage.entries()[0].outcome,
        RequestOutcome::Accepted
    );
    agent.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn cancellation_and_timeout_destroy_the_future_before_closing_and_draining_usage() {
    for timeout in [false, true] {
        let cancel = CancellationToken::new();
        let usage = Usage {
            input_tokens: Some(71),
            output_tokens: None,
            cache_read_input_tokens: Some(0),
            cache_write_input_tokens: None,
        };
        let model = Arc::new(RaceModel {
            on_drop: Some(usage.clone()),
            cancel_on_poll: (!timeout).then(|| cancel.clone()),
            ..RaceModel::default()
        });
        let (_directory, agent, chat) = fixture(model.clone()).await;
        let error = agent
            .tracked_model_request(
                RequestContext::manual_compaction(chat),
                model.clone(),
                empty_request(),
                cancel,
                timeout.then_some(Duration::ZERO),
            )
            .await
            .err()
            .expect("pending response must end through cancellation or timeout");
        match error {
            AgentError::Model(ModelError::Cancelled) if !timeout => {}
            AgentError::Model(ModelError::TimedOut) if timeout => {}
            other => panic!("unexpected termination: {other:?}"),
        }
        let expected_attempts = if timeout {
            super::model_request::MAX_MODEL_ERROR_RETRIES + 1
        } else {
            1
        };
        assert_eq!(model.calls.load(Ordering::SeqCst), expected_attempts);
        assert!(model.dropped.load(Ordering::SeqCst));
        assert!(model.drop_usage_delivered.load(Ordering::SeqCst));
        let snapshot = agent.snapshot(chat).await.unwrap();
        assert_eq!(snapshot.request_usage.entries().len(), expected_attempts);
        for entry in snapshot.request_usage.entries() {
            assert_eq!(entry.usage, usage);
            assert_eq!(
                entry.outcome,
                if timeout {
                    RequestOutcome::TimedOut
                } else {
                    RequestOutcome::Cancelled
                }
            );
        }
        agent.shutdown().await;
    }
}

#[tokio::test(start_paused = true)]
async fn queued_usage_regression_takes_precedence_over_local_cancel_or_timeout() {
    for timeout in [false, true] {
        let cancel = CancellationToken::new();
        let model = Arc::new(RaceModel {
            observations: vec![
                Usage {
                    input_tokens: Some(100),
                    cache_read_input_tokens: Some(9),
                    ..Usage::default()
                },
                Usage {
                    input_tokens: Some(90),
                    output_tokens: Some(20),
                    cache_write_input_tokens: Some(0),
                    ..Usage::default()
                },
            ],
            cancel_on_poll: (!timeout).then(|| cancel.clone()),
            ..RaceModel::default()
        });
        let (_directory, agent, chat) = fixture(model.clone()).await;
        let error = agent
            .tracked_model_request(
                RequestContext::manual_compaction(chat),
                model.clone(),
                empty_request(),
                cancel,
                timeout.then_some(Duration::ZERO),
            )
            .await
            .err()
            .expect("a usage regression must fail the request");
        assert!(matches!(
            error,
            AgentError::Model(ModelError::InvalidResponse(_))
        ));
        assert_eq!(model.calls.load(Ordering::SeqCst), 1);
        assert!(model.dropped.load(Ordering::SeqCst));
        let snapshot = agent.snapshot(chat).await.unwrap();
        assert_eq!(snapshot.request_usage.entries().len(), 1);
        let entry = &snapshot.request_usage.entries()[0];
        assert_eq!(
            entry.outcome,
            RequestOutcome::Failed(RequestFailure::Protocol(ProtocolFailure::UsageRegression(
                UsageCounter::Input,
            )))
        );
        assert_eq!(
            entry.usage,
            Usage {
                input_tokens: Some(100),
                output_tokens: Some(20),
                cache_read_input_tokens: Some(9),
                cache_write_input_tokens: Some(0),
            }
        );
        agent.shutdown().await;
    }
}

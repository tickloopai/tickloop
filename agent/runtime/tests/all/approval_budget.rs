use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use serde_json::json;
use tickloop_agent_model::{ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest};
use tickloop_agent_protocol::ReasoningEffort;
use tickloop_agent_protocol::{
    ApprovalFailureKind, ApprovalOutcome, EventKind, ExecutionMode, ModelIdentity,
    ModelRequestKind, RequestOutcome, Usage,
};
use tickloop_agent_runtime::Agent;
use tickloop_agent_test_support::{ScriptedModel, auto_classification, call, config, text};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy)]
enum Reply {
    Block,
    Allow,
    Invalid,
    ServiceFailure,
}

struct TimedReviewer {
    replies: Vec<(u64, Reply)>,
    started: Mutex<Vec<tokio::time::Instant>>,
}

struct ThinkingAwareReviewer {
    supports_off: bool,
    model: ScriptedModel,
}

#[async_trait::async_trait]
impl ModelClient for ThinkingAwareReviewer {
    fn identity(&self) -> ModelIdentity {
        self.model.identity()
    }

    fn supports_reasoning_effort(&self, effort: ReasoningEffort) -> bool {
        self.supports_off && effort == ReasoningEffort::Off
    }

    async fn complete(
        &self,
        request: ModelRequest,
        events: UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        self.model.complete(request, events, cancel).await
    }
}

#[tokio::test]
async fn classifier_disables_api_thinking_only_when_supported_in_both_stages() {
    for supports_off in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let reviewer = Arc::new(ThinkingAwareReviewer {
            supports_off,
            model: ScriptedModel::new(vec![
                text(&auto_classification(true).to_string()),
                text(&auto_classification(false).to_string()),
            ]),
        });
        let agent = Agent::new_embedded(
            config(),
            Arc::new(ScriptedModel::new(vec![
                call(
                    "candidate",
                    "Bash",
                    json!({"command":"ln -s missing reviewed"}),
                ),
                text("done"),
            ])),
        );
        agent.set_auto_reviewer(reviewer.clone()).await;
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        agent
            .set_execution_mode(chat, ExecutionMode::Auto)
            .await
            .unwrap();
        let mut events = agent.subscribe();
        agent
            .submit(chat, "Create a local symlink.".into())
            .await
            .unwrap();
        loop {
            match tickloop_agent_test_support::next_event(&mut events)
                .await
                .kind
            {
                EventKind::Completed { .. } => break,
                EventKind::PermissionRequested { .. } | EventKind::Failed(_) => {
                    panic!("unexpected failure")
                }
                _ => {}
            }
        }
        {
            let requests = reviewer.model.requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            for request in requests.iter() {
                assert_eq!(
                    request.reasoning_effort,
                    supports_off.then_some(ReasoningEffort::Off)
                );
            }
        }
        agent.shutdown().await;
    }
}

#[async_trait::async_trait]
impl ModelClient for TimedReviewer {
    fn identity(&self) -> ModelIdentity {
        config().identity()
    }

    async fn complete(
        &self,
        _: ModelRequest,
        _: UnboundedSender<ModelDelta>,
        _: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        let index = {
            let mut started = self.started.lock().unwrap();
            let index = started.len();
            started.push(tokio::time::Instant::now());
            index
        };
        let (seconds, reply) = self.replies[index];
        tokio::time::sleep(Duration::from_secs(seconds)).await;
        if matches!(reply, Reply::ServiceFailure) {
            return Err(ModelError::Http { status: 503 });
        }
        Ok(ModelOutput {
            content: if matches!(reply, Reply::Invalid) {
                text("invalid classifier response")
            } else {
                text(&auto_classification(matches!(reply, Reply::Block)).to_string())
            },
            usage: Usage::default(),
            stop_reason: "end_turn".into(),
            private_source: None,
        })
    }
}

#[tokio::test(start_paused = true)]
async fn llm_stages_format_retries_and_transport_retries_share_one_deadline() {
    for replies in [
        vec![(24, Reply::Block), (24, Reply::Allow)],
        vec![(20, Reply::Invalid), (20, Reply::Block), (20, Reply::Allow)],
        vec![
            (20, Reply::ServiceFailure),
            (20, Reply::ServiceFailure),
            (20, Reply::Allow),
        ],
    ] {
        let directory = tempfile::tempdir().unwrap();
        let reviewer = Arc::new(TimedReviewer {
            replies: replies.clone(),
            started: Mutex::new(Vec::new()),
        });
        let agent = Agent::new_embedded(
            config(),
            Arc::new(ScriptedModel::new(vec![
                call(
                    "candidate",
                    "Bash",
                    json!({"command":"ln -s missing reviewed"}),
                ),
                text("I will wait for the user."),
            ])),
        );
        agent.set_auto_reviewer(reviewer.clone()).await;
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        agent
            .set_execution_mode(chat, ExecutionMode::Auto)
            .await
            .unwrap();
        let mut events = agent.subscribe();
        let receipt = agent
            .submit(chat, "Create a local symlink.".into())
            .await
            .unwrap();
        let mut assessment = None;
        let mut assessed_at = None;
        let mut prompted = false;
        loop {
            let event = tokio::time::timeout(Duration::from_secs(90), events.recv())
                .await
                .expect("approval scenario stalled")
                .unwrap();
            if event.run_id != receipt.run_id {
                continue;
            }
            match event.kind {
                EventKind::ApprovalAssessed { assessment: value } => {
                    assessment = Some(value);
                    assessed_at = Some(tokio::time::Instant::now());
                }
                EventKind::PermissionRequested { request } => {
                    prompted = true;
                    agent
                        .answer_permission(chat, request.id, false)
                        .await
                        .unwrap();
                }
                EventKind::Completed { .. } | EventKind::Interrupted => break,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
        let assessment = assessment.unwrap();
        assert_eq!(assessment.outcome, ApprovalOutcome::Unavailable);
        assert!(prompted);
        assert_eq!(
            assessment.attempts.last().unwrap().failure,
            Some(ApprovalFailureKind::Timeout)
        );
        {
            let started = reviewer.started.lock().unwrap();
            assert_eq!(started.len(), replies.len());
            let elapsed = assessed_at.unwrap().duration_since(started[0]);
            assert!(
                (Duration::from_secs(44)..=Duration::from_secs(45)).contains(&elapsed),
                "{elapsed:?}"
            );
        }
        let snapshot = agent.snapshot(chat).await.unwrap();
        let requests = snapshot
            .request_usage
            .entries()
            .iter()
            .filter(|request| request.kind == ModelRequestKind::PermissionCheck)
            .collect::<Vec<_>>();
        assert_eq!(requests.len(), replies.len());
        assert!(
            requests
                .iter()
                .all(|request| request.outcome != RequestOutcome::InFlight)
        );
        assert_eq!(requests.last().unwrap().outcome, RequestOutcome::TimedOut);
        assert!(
            directory
                .path()
                .join("reviewed")
                .symlink_metadata()
                .is_err()
        );
        agent.shutdown().await;
    }
}

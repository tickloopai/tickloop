use crate::support;

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use tickloop_agent_model::{
    ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest, ModelRequestStage,
};
use tickloop_agent_protocol::{
    Content, EventKind, LoopState, ModelIdentity, RequestFailure, RequestOutcome, Role, Usage,
};
use tickloop_agent_runtime::Agent;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

enum Reply {
    Error(ModelError),
    Panic,
    PanicAfter(CancellationToken),
    Output(Vec<Content>),
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
        Ok(tickloop_agent_test_support::title_output(request))
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
            Reply::Output(content) => Ok(ModelOutput {
                private_source: None,
                content,
                usage: Usage::default(),
                stop_reason: "tool_use".into(),
            }),
            Reply::Panic => {
                events
                    .send(ModelDelta::Usage(Usage {
                        input_tokens: Some(17),
                        output_tokens: Some(2),
                        ..Usage::default()
                    }))
                    .unwrap();
                panic!("fixture-private-panic-payload");
            }
            Reply::PanicAfter(release) => {
                release.cancelled().await;
                panic!("fixture-private-panic-payload");
            }
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
    let agent = Agent::new_embedded(tickloop_agent_test_support::config(), model.clone());
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
    let agent = Agent::new_embedded(tickloop_agent_test_support::config(), model.clone());
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
    let agent = Agent::new_embedded(tickloop_agent_test_support::config(), model.clone());
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

#[tokio::test(start_paused = true)]
async fn transport_timeouts_retry_and_fail_without_leaving_the_run_running() {
    for stage in [
        ModelRequestStage::Connect,
        ModelRequestStage::ResponseHeaders,
        ModelRequestStage::ResponseStream,
    ] {
        let model = Arc::new(FailingModel::new(
            (0..11)
                .map(|_| Reply::Error(ModelError::TimedOutAt { stage }))
                .chain(std::iter::once(Reply::Success("Recovered")))
                .collect(),
        ));
        let agent = Agent::new_embedded(tickloop_agent_test_support::config(), model.clone());
        let directory = tempfile::tempdir().unwrap();
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        let mut events = agent.subscribe();
        agent.submit(chat, None, "Answer".into()).await.unwrap();

        let error = tokio::time::timeout(Duration::from_secs(300), async {
            loop {
                let event = events.recv().await.unwrap();
                if event.chat_id == chat
                    && let EventKind::Failed(error) = event.kind
                {
                    break error;
                }
            }
        })
        .await
        .expect("transport timeout left the run Running");
        assert!(error.contains("timed out"), "{error}");
        assert_eq!(model.request_count(), 11);
        let snapshot = agent.snapshot(chat).await.unwrap();
        assert_eq!(snapshot.loops[0].state, LoopState::Failed);
        let requests = support::model_script_requests(&snapshot);
        assert_eq!(requests.len(), 11);
        assert!(
            requests
                .iter()
                .all(|request| request.outcome == RequestOutcome::TimedOut)
        );

        let recovery = agent.submit(chat, None, "Try again".into()).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let event = events.recv().await.unwrap();
                if event.chat_id == chat && matches!(event.kind, EventKind::Completed { .. }) {
                    break;
                }
            }
        })
        .await
        .expect("failed request kept the execution slot occupied");
        let snapshot = agent.snapshot(chat).await.unwrap();
        assert_eq!(
            snapshot
                .loops
                .iter()
                .find(|goal| goal.id == recovery.loop_id)
                .unwrap()
                .state,
            LoopState::Completed
        );
        assert_eq!(model.request_count(), 12);
        assert_eq!(
            support::model_script_requests(&snapshot)
                .last()
                .unwrap()
                .outcome,
            RequestOutcome::Accepted
        );

        agent.shutdown().await;
    }
}

#[tokio::test]
async fn internal_model_panic_fails_without_retry_and_releases_the_execution_slot() {
    let model = Arc::new(FailingModel::new(vec![
        Reply::Panic,
        Reply::Success("Other chat"),
        Reply::Success("Recovered"),
    ]));
    let archive = tempfile::tempdir().unwrap();
    let agent = Agent::open_embedded(
        archive.path().into(),
        tickloop_agent_test_support::config(),
        model.clone(),
    )
    .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let failed = agent.submit(chat, None, "Answer".into()).await.unwrap();
    let error = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = events.recv().await.unwrap();
            if event.chat_id == chat {
                match event.kind {
                    EventKind::Failed(error) => break error,
                    EventKind::ModelRetry { .. } => {
                        panic!("an internal panic must not retry the model")
                    }
                    _ => {}
                }
            }
        }
    })
    .await
    .expect("model panic left the run Running");
    assert!(error.contains("internal"), "{error}");
    assert!(!error.contains("fixture-private-panic-payload"));
    assert_eq!(model.request_count(), 1);
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(snapshot.loops[0].state, LoopState::Failed);
    assert!(snapshot.pending_thinking.is_empty());
    assert!(
        support::model_script_requests(&snapshot)
            .iter()
            .all(|request| request.outcome == RequestOutcome::Failed(RequestFailure::Internal))
    );
    assert!(
        snapshot
            .failure_events
            .iter()
            .any(|event| event.run_id == failed.run_id)
    );
    assert!(
        !serde_json::to_string(&snapshot)
            .unwrap()
            .contains("fixture-private-panic-payload")
    );

    for target in [
        agent.create_chat(directory.path().into()).await.unwrap(),
        chat,
    ] {
        let recovered = agent
            .submit(target, None, "Try again".into())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let event = events.recv().await.unwrap();
                if event.run_id == recovered.run_id
                    && matches!(event.kind, EventKind::Completed { .. })
                {
                    break;
                }
            }
        })
        .await
        .expect("panic prevented a later run from completing");
    }
    assert_eq!(model.request_count(), 3);
    agent.shutdown().await;
    let restored = Agent::open_embedded(
        archive.path().into(),
        tickloop_agent_test_support::config(),
        model.clone(),
    )
    .unwrap();
    restored.resume_chat(chat).await.unwrap();
    let snapshot = restored.snapshot(chat).await.unwrap();
    assert!(snapshot.failure_events.iter().any(|event| event.run_id == failed.run_id && matches!(&event.kind, EventKind::Failed(message) if message.contains("internal"))));
    assert!(
        !serde_json::to_string(&snapshot)
            .unwrap()
            .contains("fixture-private-panic-payload")
    );
    assert_eq!(model.request_count(), 3);
    restored.shutdown().await;
}

#[tokio::test]
async fn a_subagent_model_panic_releases_the_parent_waiter() {
    let model = Arc::new(FailingModel::new(vec![
        Reply::Output(tickloop_agent_test_support::call(
            "delegate",
            "Subagent",
            serde_json::json!({"name":"research", "task":"Inspect the fixture"}),
        )),
        Reply::Panic,
        Reply::Success("The child failed."),
    ]));
    let agent = Agent::new_embedded(tickloop_agent_test_support::config(), model.clone());
    let directory = tempfile::tempdir().unwrap();
    let root = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let receipt = agent.submit(root, None, "Delegate".into()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = events.recv().await.unwrap();
            if event.run_id == receipt.run_id && matches!(event.kind, EventKind::Completed { .. }) {
                break;
            }
        }
    })
    .await
    .expect("child panic left the parent waiting");
    let chats = agent.list_chats().await;
    let child = chats.iter().find(|chat| chat.delegation.is_some()).unwrap();
    assert_eq!(child.loops[0].state, LoopState::Failed);
    assert_eq!(
        support::model_script_requests(child)[0].outcome,
        RequestOutcome::Failed(RequestFailure::Internal)
    );
    assert_eq!(model.request_count(), 3);
    agent.shutdown().await;
}

#[tokio::test]
async fn an_internal_panic_does_not_automatically_run_a_queued_input() {
    let release = CancellationToken::new();
    let model = Arc::new(FailingModel::new(vec![Reply::PanicAfter(release.clone())]));
    let agent = Agent::new_embedded(tickloop_agent_test_support::config(), model.clone());
    let directory = tempfile::tempdir().unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent.submit(chat, None, "Answer".into()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while model.request_count() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    agent.submit(chat, None, "Status?".into()).await.unwrap();
    release.cancel();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if matches!(events.recv().await.unwrap().kind, EventKind::Failed(_)) {
                break;
            }
        }
    })
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(model.request_count(), 1);
    assert_eq!(snapshot.pending_inputs.len(), 1);
    assert_eq!(snapshot.loops[0].state, LoopState::Failed);
    agent.shutdown().await;
}

#[tokio::test]
async fn authentication_error_fails_without_repeating_the_same_key() {
    let model = Arc::new(FailingModel::new(vec![Reply::Error(
        ModelError::Authentication,
    )]));
    let agent = Agent::new_embedded(tickloop_agent_test_support::config(), model.clone());
    let (completed, error) = run_until_finished(&agent, &model).await;
    assert!(!completed);
    assert!(error.contains("authentication"));
    assert_eq!(model.request_count(), 1);
    agent.shutdown().await;
}

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tickloop_agent_model::{ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest};
use tickloop_agent_protocol::{
    Content, EventKind, INTERRUPT_MESSAGE, InputModality, LoopState, ModelConfig, Protocol, Usage,
};
use tickloop_agent_runtime::Agent;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct ConversationModel {
    requests: Mutex<Vec<ModelRequest>>,
}

#[async_trait]
impl ModelClient for ConversationModel {
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
        request: ModelRequest,
        events: UnboundedSender<ModelDelta>,
        _: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        self.requests.lock().unwrap().push(request);
        events.send(ModelDelta::Text("Hello ".into())).unwrap();
        events.send(ModelDelta::Text("Sam".into())).unwrap();
        Ok(ModelOutput {
            private_source: None,
            content: vec![Content::Text {
                text: "Hello Sam".into(),
            }],
            usage: Usage {
                input_tokens: Some(12),
                output_tokens: Some(2),
                ..Default::default()
            },
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

#[tokio::test]
async fn a_followup_keeps_chat_goal_and_shared_history_while_creating_a_new_run() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ConversationModel::default());
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let first = agent
        .submit(chat, None, "My name is Sam".into())
        .await
        .unwrap();
    let mut streamed = String::new();
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), events.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.loop_id, first.loop_id);
        match event.kind {
            EventKind::Text(text) => streamed.push_str(&text),
            EventKind::Completed { usage } => {
                assert_eq!(usage.output_tokens, Some(2));
                break;
            }
            _ => {}
        }
    }
    assert_eq!(streamed, "Hello Sam");
    let second = agent
        .submit(chat, Some(first.loop_id), "What is my name?".into())
        .await
        .unwrap();
    assert_eq!(first.loop_id, second.loop_id);
    assert_ne!(first.run_id, second.run_id);
    while !matches!(
        events.recv().await.unwrap().kind,
        EventKind::Completed { .. }
    ) {}
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(snapshot.messages.len(), 4);
    assert_eq!(snapshot.loops.len(), 1);
    assert_eq!(snapshot.loops[0].state, LoopState::Completed);
    let requests = model.requests.lock().unwrap();
    assert_eq!(requests[1].messages.len(), 4);
    assert_eq!(
        requests[1].messages[1].content,
        vec![Content::Text {
            text: "My name is Sam".into()
        }]
    );
}

#[tokio::test]
async fn opening_an_archive_is_read_only_until_the_user_continues_a_selected_goal() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("archive");
    let model = Arc::new(ConversationModel::default());
    let agent = Agent::open_embedded(path.clone(), config(), model.clone()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let original = agent
        .submit(chat, None, "Remember Sam".into())
        .await
        .unwrap();
    while !matches!(
        events.recv().await.unwrap().kind,
        EventKind::Completed { .. }
    ) {}
    agent.shutdown().await;
    drop(agent);
    let resumed = Agent::open_embedded(path, config(), model.clone()).unwrap();
    assert!(resumed.list_chats().await.is_empty());
    assert_eq!(resumed.resume_chat(chat).await.unwrap().messages.len(), 2);
    assert_eq!(resumed.list_chats().await.len(), 1);
    assert_eq!(model.requests.lock().unwrap().len(), 1);
    let mut events = resumed.subscribe();
    let continued = resumed
        .submit(chat, Some(original.loop_id), "Continue".into())
        .await
        .unwrap();
    assert_ne!(continued.run_id, original.run_id);
    while !matches!(
        events.recv().await.unwrap().kind,
        EventKind::Completed { .. }
    ) {}
    assert_eq!(model.requests.lock().unwrap()[1].messages.len(), 4);
    resumed.shutdown().await;
}

#[tokio::test]
async fn inputs_accepted_while_busy_run_in_order_and_keep_goals_independent() {
    struct SlowModel(Arc<ConversationModel>);
    #[async_trait]
    impl ModelClient for SlowModel {
        async fn generate_title(
            &self,
            request: tickloop_agent_model::ModelRequest,
            _: tokio::sync::mpsc::UnboundedSender<tickloop_agent_model::ModelDelta>,
            _: tokio_util::sync::CancellationToken,
        ) -> Result<tickloop_agent_model::ModelOutput, tickloop_agent_model::ModelError> {
            Ok(tickloop_agent_test_support::title_output(request))
        }

        fn identity(&self) -> tickloop_agent_protocol::ModelIdentity {
            self.0.identity()
        }

        async fn complete(
            &self,
            request: ModelRequest,
            events: UnboundedSender<ModelDelta>,
            cancel: CancellationToken,
        ) -> Result<ModelOutput, ModelError> {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            self.0.complete(request, events, cancel).await
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let responses = Arc::new(ConversationModel::default());
    let agent = Agent::new_embedded(config(), Arc::new(SlowModel(responses.clone())));
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let first = agent.submit(chat, None, "First goal".into()).await.unwrap();
    let second = agent
        .submit(chat, None, "Independent goal".into())
        .await
        .unwrap();
    agent
        .submit(chat, Some(first.loop_id), "First goal addition".into())
        .await
        .unwrap();
    let mut completed = 0;
    while completed < 3 {
        if matches!(
            events.recv().await.unwrap().kind,
            EventKind::Completed { .. }
        ) {
            completed += 1;
        }
    }
    assert_ne!(first.loop_id, second.loop_id);
    assert_eq!(agent.snapshot(chat).await.unwrap().loops.len(), 2);
    let requests = responses.requests.lock().unwrap();
    assert_eq!(
        requests
            .iter()
            .map(|request| request.messages.len())
            .collect::<Vec<_>>(),
        vec![2, 4, 6]
    );
    assert!(
        matches!(&requests[2].messages[5].content[0],Content::Text{text} if text=="First goal addition")
    );
}

#[tokio::test]
async fn stopping_a_goal_cancels_its_run_and_queue_but_other_goals_continue() {
    struct CancellableModel;
    #[async_trait]
    impl ModelClient for CancellableModel {
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
            request: ModelRequest,
            _: UnboundedSender<ModelDelta>,
            cancel: CancellationToken,
        ) -> Result<ModelOutput, ModelError> {
            if request.messages.len() == 2 {
                cancel.cancelled().await;
                return Err(ModelError::Cancelled);
            }
            Ok(ModelOutput {
                private_source: None,
                content: vec![Content::Text {
                    text: "other goal done".into(),
                }],
                usage: Usage::default(),
                stop_reason: "end_turn".into(),
            })
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let agent = Agent::new_embedded(config(), Arc::new(CancellableModel));
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let first = agent.submit(chat, None, "cancel me".into()).await.unwrap();
    agent
        .submit(chat, Some(first.loop_id), "queued addition".into())
        .await
        .unwrap();
    let second = agent.submit(chat, None, "keep me".into()).await.unwrap();
    agent.stop(chat, first.loop_id).await.unwrap();
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), events.recv())
            .await
            .unwrap()
            .unwrap();
        if event.loop_id == second.loop_id && matches!(event.kind, EventKind::Completed { .. }) {
            break;
        }
    }
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(
        snapshot
            .loops
            .iter()
            .find(|goal| goal.id == first.loop_id)
            .unwrap()
            .state,
        LoopState::Stopped
    );
    assert_eq!(snapshot.messages.len(), 4);
    assert!(matches!(
        &snapshot.messages[1].content[..],
        [Content::Text { text }] if text == INTERRUPT_MESSAGE
    ));
    assert!(
        agent
            .submit(chat, Some(first.loop_id), "accidental wake".into())
            .await
            .is_err()
    );
    agent.shutdown().await;
}

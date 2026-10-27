use std::sync::{Arc, Mutex};
use tickloop_code_settings::local_agent::LocalAgent;

use async_trait::async_trait;
use tickloop_harness::model::{ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest};
use tickloop_harness::protocol::{
    Content, EventKind, INTERRUPT_MESSAGE, InputModality, LoopState, ModelConfig, Protocol, Usage,
};
use tickloop_harness::runtime::Agent;
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
        request: tickloop_harness::model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_harness::model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_harness::model::ModelOutput, tickloop_harness::model::ModelError> {
        Ok(tickloop_harness::test_support::title_output(request))
    }

    fn identity(&self) -> tickloop_harness::protocol::ModelIdentity {
        tickloop_harness::protocol::ModelIdentity {
            provider: tickloop_harness::protocol::ModelProvider::Api,
            profile: "fixture".into(),
            protocol: tickloop_harness::protocol::Protocol::Messages,
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
        provider: tickloop_harness::protocol::ModelProvider::Api,
        auth_source: tickloop_harness::protocol::ModelAuthSource::ApiKeyEnv,
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
    let first = agent.submit(chat, "My name is Sam".into()).await.unwrap();
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
    let second = agent.submit(chat, "What is my name?".into()).await.unwrap();
    assert_eq!(first.loop_id, second.loop_id);
    assert_ne!(first.run_id, second.run_id);
    while !matches!(
        events.recv().await.unwrap().kind,
        EventKind::Completed { .. }
    ) {}
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(snapshot.messages.len(), 4);
    assert_eq!(snapshot.chat_loop.id, first.loop_id);
    assert_eq!(snapshot.chat_loop.state, LoopState::Idle);
    let requests = model.requests.lock().unwrap();
    assert_eq!(requests[1].messages.len(), 5);
    assert_eq!(
        requests[1].messages[2].content,
        vec![Content::Text {
            text: "My name is Sam".into()
        }]
    );
}

#[tokio::test]
async fn opening_an_archive_is_read_only_until_the_user_continues() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("archive");
    let model = Arc::new(ConversationModel::default());
    let agent = Agent::open_embedded(path.clone(), config(), model.clone()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let original = agent.submit(chat, "Remember Sam".into()).await.unwrap();
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
    let continued = resumed.submit(chat, "Continue".into()).await.unwrap();
    assert_ne!(continued.run_id, original.run_id);
    while !matches!(
        events.recv().await.unwrap().kind,
        EventKind::Completed { .. }
    ) {}
    assert_eq!(model.requests.lock().unwrap()[1].messages.len(), 5);
    resumed.shutdown().await;
}

#[tokio::test]
async fn inputs_accepted_while_busy_run_in_order_in_the_one_loop() {
    struct SlowModel(Arc<ConversationModel>);
    #[async_trait]
    impl ModelClient for SlowModel {
        async fn generate_title(
            &self,
            request: tickloop_harness::model::ModelRequest,
            _: tokio::sync::mpsc::UnboundedSender<tickloop_harness::model::ModelDelta>,
            _: tokio_util::sync::CancellationToken,
        ) -> Result<tickloop_harness::model::ModelOutput, tickloop_harness::model::ModelError>
        {
            Ok(tickloop_harness::test_support::title_output(request))
        }

        fn identity(&self) -> tickloop_harness::protocol::ModelIdentity {
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
    let first = agent.submit(chat, "First request".into()).await.unwrap();
    let second = agent.submit(chat, "Another request".into()).await.unwrap();
    let third = agent
        .submit(chat, "First request addition".into())
        .await
        .unwrap();
    assert_eq!(first.loop_id, second.loop_id);
    assert_eq!(second.loop_id, third.loop_id);
    loop {
        let event = events.recv().await.unwrap();
        if event.run_id == second.run_id && matches!(event.kind, EventKind::Completed { .. }) {
            break;
        }
    }
    let requests = responses.requests.lock().unwrap();
    assert_eq!(requests.len(), 2, "the queued inputs join one turn");
    let later = serde_json::to_string(&requests[1].messages).unwrap();
    let another = later.find("Another request").unwrap();
    let addition = later.find("First request addition").unwrap();
    assert!(another < addition, "{later}");
}

#[tokio::test]
async fn stop_interrupts_the_turn_and_a_new_input_continues_the_loop() {
    struct CancellableModel;
    #[async_trait]
    impl ModelClient for CancellableModel {
        async fn generate_title(
            &self,
            request: tickloop_harness::model::ModelRequest,
            _: tokio::sync::mpsc::UnboundedSender<tickloop_harness::model::ModelDelta>,
            _: tokio_util::sync::CancellationToken,
        ) -> Result<tickloop_harness::model::ModelOutput, tickloop_harness::model::ModelError>
        {
            Ok(tickloop_harness::test_support::title_output(request))
        }

        fn identity(&self) -> tickloop_harness::protocol::ModelIdentity {
            tickloop_harness::protocol::ModelIdentity {
                provider: tickloop_harness::protocol::ModelProvider::Api,
                profile: "fixture".into(),
                protocol: tickloop_harness::protocol::Protocol::Messages,
                requested_model: "fixture".into(),
            }
        }

        async fn complete(
            &self,
            request: ModelRequest,
            _: UnboundedSender<ModelDelta>,
            cancel: CancellationToken,
        ) -> Result<ModelOutput, ModelError> {
            if request.messages.len() == 3 {
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
    let first = agent.submit(chat, "cancel me".into()).await.unwrap();
    loop {
        let event = events.recv().await.unwrap();
        if event.run_id == first.run_id && matches!(event.kind, EventKind::Started) {
            break;
        }
    }
    agent.stop(chat).await.unwrap();
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), events.recv())
            .await
            .unwrap()
            .unwrap();
        if event.run_id == first.run_id && matches!(event.kind, EventKind::Interrupted) {
            break;
        }
    }
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(snapshot.chat_loop.state, LoopState::Interrupted);
    assert!(matches!(
        &snapshot.messages[1].content[..],
        [Content::Text { text }] if text == INTERRUPT_MESSAGE
    ));
    let next = agent.submit(chat, "keep going".into()).await.unwrap();
    assert_eq!(next.loop_id, first.loop_id);
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), events.recv())
            .await
            .unwrap()
            .unwrap();
        if event.run_id == next.run_id && matches!(event.kind, EventKind::Completed { .. }) {
            break;
        }
    }
    assert_eq!(
        agent.snapshot(chat).await.unwrap().chat_loop.state,
        LoopState::Idle
    );
    agent.shutdown().await;
}

use std::{sync::Arc, time::Duration};

use serde_json::json;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use super::{Agent, EventKind};
use crate::model::{
    Content, ModelClient, ModelConfig, ModelDelta, ModelError, ModelIdentity, ModelOutput,
    ModelRequest, Protocol, Usage,
};

struct ReadOnce;

#[async_trait::async_trait]
impl ModelClient for ReadOnce {
    fn identity(&self) -> ModelIdentity {
        ModelIdentity {
            provider: crate::model::ModelProvider::Api,
            profile: "read-once".into(),
            protocol: Protocol::Messages,
            requested_model: "read-once".into(),
        }
    }

    async fn complete(
        &self,
        request: ModelRequest,
        _: UnboundedSender<ModelDelta>,
        _: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        let has_result = request
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .any(|content| matches!(content, Content::ToolResult { .. }));
        let content = if has_result {
            vec![Content::Text {
                text: "Read it.".into(),
            }]
        } else {
            vec![Content::ToolUse {
                id: "read-notes".into(),
                name: "Read".into(),
                input: json!({"file_path":"notes.txt"}),
            }]
        };
        Ok(ModelOutput {
            private_source: None,
            content,
            usage: Usage::default(),
            stop_reason: "end_turn".into(),
        })
    }
}

fn config() -> ModelConfig {
    ModelConfig {
        provider: crate::model::ModelProvider::Api,
        auth_source: crate::model::ModelAuthSource::ApiKeyEnv,
        name: "read-once".into(),
        protocol: Protocol::Messages,
        base_url: "http://127.0.0.1".into(),
        model: "read-once".into(),
        api_key_env: String::new(),
        max_output_tokens: 256,
        provider_id: String::new(),
        provider_name: String::new(),
        model_id: String::new(),
        connection: None,
        card: None,
        input_modalities: vec![crate::model::InputModality::Text],
    }
}

#[tokio::test]
async fn a_read_is_recorded_only_for_the_chat_that_made_it() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("notes.txt"), "Delivery Friday\n").unwrap();
    let agent = Agent::new_embedded(config(), Arc::new(ReadOnce));
    let reader = agent.create_chat(directory.path().into()).await.unwrap();
    let bystander = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(reader, None, "Read the notes".into())
        .await
        .unwrap();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(3), events.recv())
            .await
            .unwrap()
            .unwrap();
        match event.kind {
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }

    let path = directory.path().canonicalize().unwrap().join("notes.txt");
    let chats = agent.inner.chats.lock().await;
    let record = chats[&reader]
        .tools
        .file_reads()
        .get(&path)
        .expect("the reading Chat keeps what it saw");
    assert_eq!(record.content, "Delivery Friday\n");
    assert!(record.is_full_read);
    assert!(chats[&bystander].tools.file_reads().get(&path).is_none());
    drop(chats);
    agent.shutdown().await;
}

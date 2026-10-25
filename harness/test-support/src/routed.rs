//! A model that answers each request from its content, so concurrent family Chats sharing one
//! model each get the reply their own state calls for.

use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::Value;
use tickloop_harness_model::{ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest};
use tickloop_harness_protocol::{Content, Message, Role};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

type ResponseScript = dyn Fn(&ModelRequest) -> Vec<Content> + Send + Sync;

pub struct RoutedModel {
    pub requests: Mutex<Vec<ModelRequest>>,
    respond: Box<ResponseScript>,
}

impl RoutedModel {
    pub fn new(respond: impl Fn(&ModelRequest) -> Vec<Content> + Send + Sync + 'static) -> Self {
        Self {
            requests: Mutex::new(vec![]),
            respond: Box::new(respond),
        }
    }
}

#[async_trait]
impl ModelClient for RoutedModel {
    async fn generate_title(
        &self,
        request: tickloop_harness_model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_harness_model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_harness_model::ModelOutput, tickloop_harness_model::ModelError> {
        Ok(super::title_output(request))
    }

    fn identity(&self) -> tickloop_harness_protocol::ModelIdentity {
        super::ScriptedModel::new(vec![]).identity()
    }
    async fn complete(
        &self,
        request: ModelRequest,
        events: UnboundedSender<ModelDelta>,
        _: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        self.requests.lock().unwrap().push(request.clone());
        let content = (self.respond)(&request);
        for block in &content {
            if let Content::Text { text } = block {
                let _ = events.send(ModelDelta::Text(text.clone()));
            }
        }
        Ok(ModelOutput {
            private_source: None,
            content,
            usage: Default::default(),
            stop_reason: "end_turn".into(),
        })
    }
}

pub fn has_tool_result(request: &ModelRequest, id: &str) -> bool {
    request
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .any(|content| matches!(content,Content::ToolResult {tool_use_id,..} if tool_use_id==id))
}

pub fn state(request: &ModelRequest) -> Value {
    request
        .messages
        .iter()
        .find_map(|message| {
            if message.role != Role::System {
                return None;
            }
            message.content.iter().find_map(|content| match content {
                Content::Text { text } => text
                    .rsplit_once("Current host state: ")
                    .and_then(|(_, value)| serde_json::from_str(value).ok()),
                _ => None,
            })
        })
        .unwrap()
}

pub fn history_text(request: &ModelRequest) -> String {
    request
        .messages
        .iter()
        .filter(|message| message.role != Role::System)
        .flat_map(|message| &message.content)
        .filter_map(|content| {
            if let Content::Text { text } = content {
                Some(text.as_str())
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn tool_result(messages: &[Message], call_id: &str) -> Option<Value> {
    messages
        .iter()
        .flat_map(|message| &message.content)
        .find_map(|content| {
            if let Content::ToolResult {
                tool_use_id,
                content,
                ..
            } = content
                && tool_use_id == call_id
            {
                serde_json::from_str(&content.text()).ok()
            } else {
                None
            }
        })
}

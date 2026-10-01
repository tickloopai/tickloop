//! The secondary model of `WebFetch`, following Claude Code `applyPromptToMarkdown`: the Chat's
//! current model answers the prompt about a fetched page in one request of its own, recorded in
//! the request ledger of the Run but never added to the conversation.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use super::{
    Agent, AgentError, Receipt, RequestKind, RequestOutcome,
    model_request::{PendingModelResponse, RequestContext},
};
use crate::model::{Content, Message, ModelClient, ModelError, ModelRequest, Role};
use crate::tools::{SecondaryModel, ToolError};

/// Claude Code's answer when the secondary model returns no text.
const NO_RESPONSE: &str = "No response from model";

pub(super) struct ChatSecondaryModel<'a> {
    agent: &'a Agent,
    receipt: &'a Receipt,
    client: Arc<dyn ModelClient>,
}

impl<'a> ChatSecondaryModel<'a> {
    pub(super) fn new(
        agent: &'a Agent,
        receipt: &'a Receipt,
        client: Arc<dyn ModelClient>,
    ) -> Self {
        Self {
            agent,
            receipt,
            client,
        }
    }

    async fn finish(
        &self,
        response: &PendingModelResponse,
        outcome: RequestOutcome,
    ) -> Result<(), ToolError> {
        self.agent
            .finish_model_request(self.receipt.chat_id, response.sequence, outcome)
            .await
            .map_err(|error| ToolError::SecondaryModel(error.to_string()))
    }
}

#[async_trait::async_trait]
impl SecondaryModel for ChatSecondaryModel<'_> {
    async fn respond(
        &self,
        prompt: String,
        cancel: CancellationToken,
    ) -> Result<String, ToolError> {
        let request = ModelRequest {
            history_sources: Vec::new(),
            messages: vec![Message {
                role: Role::User,
                content: vec![Content::Text { text: prompt }],
            }],
            tools: Vec::new(),
            structured_output: None,
            temperature: None,
            reasoning_effort: None,
        };
        let response = match self
            .agent
            .tracked_model_request(
                RequestContext::for_run(self.receipt, RequestKind::WebFetch),
                self.client.clone(),
                request,
                cancel.clone(),
                None,
            )
            .await
        {
            Ok(response) => response,
            Err(AgentError::Model(ModelError::Cancelled)) => return Err(ToolError::Cancelled),
            Err(error) => return Err(ToolError::SecondaryModel(error.to_string())),
        };
        if cancel.is_cancelled() {
            self.finish(&response, RequestOutcome::Cancelled).await?;
            return Err(ToolError::Cancelled);
        }
        self.finish(&response, RequestOutcome::Accepted).await?;
        let answer = response
            .output
            .content
            .iter()
            .filter_map(|content| match content {
                Content::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        Ok(if answer.is_empty() {
            NO_RESPONSE.to_owned()
        } else {
            answer
        })
    }
}

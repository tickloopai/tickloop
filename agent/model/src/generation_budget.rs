use std::time::Duration;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{
    ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest, ModelRequestContext,
    ModelResponseRejection,
};
use tickloop_agent_protocol::{Adapter, Content, ModelConfig, Protocol};

const MAX_ATTEMPTS: usize = 1;

#[derive(Clone, Copy)]
pub struct GenerationBudget {
    output_tokens: u32,
    deadline: Duration,
}

impl GenerationBudget {
    pub fn timeout(self) -> Duration {
        self.deadline
    }

    pub fn output_tokens(self) -> u32 {
        self.output_tokens
    }

    pub fn new(output_tokens: u32, deadline: Duration) -> Self {
        Self {
            output_tokens,
            deadline,
        }
    }

    pub fn apply_model_limit(self, mut model: ModelConfig) -> ModelConfig {
        // Codex's fixed subscription transport does not support a server output-limit field.
        if model.adapter() != Adapter::CodexResponses {
            model.max_output_tokens = model.max_output_tokens.min(self.output_tokens);
            if let Some(card) = &mut model.card {
                card.max_output_tokens = model.max_output_tokens;
            }
        }
        model
    }

    pub async fn complete(
        self,
        client: &dyn ModelClient,
        chat: Uuid,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        let (events, mut deltas) = mpsc::unbounded_channel();
        let completion = client.complete_for_chat_with_context(
            chat,
            request,
            events,
            cancel.clone(),
            ModelRequestContext::new(MAX_ATTEMPTS),
        );
        tokio::pin!(completion);
        let deadline = tokio::time::sleep(self.deadline);
        tokio::pin!(deadline);
        let mut streamed_bytes = 0usize;
        let mut stream_open = true;
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(ModelError::Cancelled),
                _ = &mut deadline => {
                    cancel.cancel();
                    return Err(ModelError::TimedOut);
                }
                delta = deltas.recv(), if stream_open => match delta {
                    Some(delta) => {
                        if self.reached_bound(&delta, &mut streamed_bytes) {
                            cancel.cancel();
                            return Err(ModelError::ResponseRejected(ModelResponseRejection::OutputLimit));
                        }
                    }
                    None => stream_open = false,
                },
                result = &mut completion => {
                    while let Ok(delta) = deltas.try_recv() {
                        if self.reached_bound(&delta, &mut streamed_bytes) {
                            cancel.cancel();
                            return Err(ModelError::ResponseRejected(ModelResponseRejection::OutputLimit));
                        }
                    }
                    let output = result?;
                    let output_bytes = output.content.iter().map(|content| match content {
                        Content::Text { text } => text.len(),
                        Content::Thinking { thinking, .. } => thinking.len(),
                        Content::ProtocolData { protocol: Protocol::Responses, value } if value["type"] == "reasoning" => {
                            [(&value["summary"], "summary_text"), (&value["content"], "reasoning_text")]
                                .into_iter().flat_map(|(parts, kind)| parts.as_array().into_iter().flatten().filter(move |part| part["type"] == kind))
                                .filter_map(|part| part["text"].as_str()).map(str::len).sum()
                        }
                        _ => 0,
                    }).sum::<usize>();
                    if output_bytes >= self.output_tokens as usize
                        || output.usage.output_tokens.is_some_and(|tokens| tokens >= u64::from(self.output_tokens)) {
                        cancel.cancel();
                        return Err(ModelError::ResponseRejected(ModelResponseRejection::OutputLimit));
                    }
                    return Ok(output);
                }
            }
        }
    }

    fn reached_bound(self, delta: &ModelDelta, streamed_bytes: &mut usize) -> bool {
        match delta {
            ModelDelta::Text(text) | ModelDelta::Thinking(text) => {
                // Without a provider tokenizer, one UTF-8 byte per budget unit cuts conservatively.
                *streamed_bytes = streamed_bytes.saturating_add(text.len());
                *streamed_bytes >= self.output_tokens as usize
            }
            ModelDelta::Usage(usage) => usage
                .output_tokens
                .is_some_and(|tokens| tokens >= u64::from(self.output_tokens)),
            ModelDelta::OutputStarted => false,
        }
    }
}

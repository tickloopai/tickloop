use std::sync::Arc;

use async_trait::async_trait;
use tickloop_agent_model::{
    ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest, ModelRequestContext,
};
use tickloop_agent_protocol::ModelIdentity;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

pub fn without_title_requests(client: Arc<dyn ModelClient>) -> Arc<dyn ModelClient> {
    Arc::new(ProviderTransport(client))
}

struct ProviderTransport(Arc<dyn ModelClient>);

#[async_trait]
impl ModelClient for ProviderTransport {
    fn connection_id(&self) -> Option<String> {
        self.0.connection_id()
    }

    fn identity(&self) -> ModelIdentity {
        self.0.identity()
    }

    async fn generate_title(
        &self,
        _: ModelRequest,
        _: UnboundedSender<ModelDelta>,
        _: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        // These transport fixtures reserve each response for a conversation request.
        Err(ModelError::Cancelled)
    }

    async fn close_chat(&self, chat_id: tickloop_agent_protocol::ChatId) {
        self.0.close_chat(chat_id).await;
    }

    async fn complete_for_chat_with_context(
        &self,
        chat_id: tickloop_agent_protocol::ChatId,
        request: ModelRequest,
        events: UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
        context: ModelRequestContext,
    ) -> Result<ModelOutput, ModelError> {
        self.0
            .complete_for_chat_with_context(chat_id, request, events, cancel, context)
            .await
    }

    async fn complete_for_chat(
        &self,
        chat_id: tickloop_agent_protocol::ChatId,
        request: ModelRequest,
        events: UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        self.0
            .complete_for_chat(chat_id, request, events, cancel)
            .await
    }

    async fn complete(
        &self,
        request: ModelRequest,
        events: UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        self.0.complete(request, events, cancel).await
    }
}

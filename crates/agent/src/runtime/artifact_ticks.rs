use super::{Agent, AgentError, ChatId};
use crate::artifacts::ArtifactTick;

impl Agent {
    pub async fn add_artifact_tick(
        &self,
        chat_id: ChatId,
        tick: ArtifactTick,
    ) -> Result<Vec<ArtifactTick>, AgentError> {
        let origin = reqwest::Url::parse(&tick.origin)
            .map_err(|_| AgentError::Invalid("invalid artifact origin".into()))?;
        let _url = reqwest::Url::parse(&tick.url)
            .map_err(|_| AgentError::Invalid("invalid artifact URL".into()))?;
        if !["markdown", "html"].contains(&tick.format.as_str())
            || !["owner", "editor", "viewer"].contains(&tick.access_role.as_str())
            || tick.artifact_id.is_empty()
            || tick.artifact_id.contains(['/', '?', '#'])
            || origin.path() != "/"
            || origin.host_str().is_none()
            || origin.query().is_some()
            || origin.fragment().is_some()
            || !origin.username().is_empty()
            || origin.password().is_some()
            || (origin.scheme() != "https"
                && !(origin.scheme() == "http" && crate::network::is_loopback(&origin)))
        {
            return Err(AgentError::Invalid("invalid artifact Tick".into()));
        }
        let mut chats = self.inner.chats.lock().await;
        let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
        if let Some(existing) = chat.snapshot.artifact_ticks.iter_mut().find(|existing| {
            existing.artifact_id == tick.artifact_id && existing.origin == tick.origin
        }) {
            let added_at_ms = existing.added_at_ms;
            *existing = tick;
            existing.added_at_ms = added_at_ms;
        } else {
            chat.snapshot.artifact_ticks.push(tick);
        }
        let ticks = chat.snapshot.artifact_ticks.clone();
        self.record_artifact_ticks(chat_id, ticks.clone());
        Ok(ticks)
    }
    pub async fn remove_artifact_tick(
        &self,
        chat_id: ChatId,
        instance_id: &str,
    ) -> Result<Vec<ArtifactTick>, AgentError> {
        let mut chats = self.inner.chats.lock().await;
        let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
        chat.snapshot
            .artifact_ticks
            .retain(|tick| tick.instance_id() != instance_id);
        let ticks = chat.snapshot.artifact_ticks.clone();
        self.record_artifact_ticks(chat_id, ticks.clone());
        Ok(ticks)
    }
    pub async fn artifact_ticks(&self, chat_id: ChatId) -> Result<Vec<ArtifactTick>, AgentError> {
        Ok(self
            .inner
            .chats
            .lock()
            .await
            .get(&chat_id)
            .ok_or(AgentError::NotFound)?
            .snapshot
            .artifact_ticks
            .clone())
    }
}

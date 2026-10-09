use super::{Agent, AgentError};
use tickloop_agent_protocol::{ArtifactTick, ChatId};

impl Agent {
    pub(super) fn auto_attach_artifact_tick(&self, chat: &mut super::Chat, tick: ArtifactTick) {
        let Ok(tick) = normalize_artifact_tick(tick) else {
            return;
        };
        if chat
            .snapshot
            .removed_artifact_ticks
            .contains(&tick.instance_id())
        {
            return;
        }
        upsert_artifact_tick(chat, tick);
        self.record_artifact_ticks(
            chat.snapshot.id,
            &chat.snapshot.workdir,
            chat.snapshot.artifact_ticks.clone(),
            chat.snapshot.removed_artifact_ticks.clone(),
        );
    }
    pub async fn add_artifact_tick(
        &self,
        chat_id: ChatId,
        mut tick: ArtifactTick,
    ) -> Result<Vec<ArtifactTick>, AgentError> {
        tick = normalize_artifact_tick(tick)?;
        let mut chats = self.inner.chats.lock().await;
        let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
        chat.snapshot
            .removed_artifact_ticks
            .remove(&tick.instance_id());
        upsert_artifact_tick(chat, tick);
        let ticks = chat.snapshot.artifact_ticks.clone();
        self.record_artifact_ticks(
            chat_id,
            &chat.snapshot.workdir,
            ticks.clone(),
            chat.snapshot.removed_artifact_ticks.clone(),
        );
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
            .removed_artifact_ticks
            .insert(instance_id.to_owned());
        chat.snapshot
            .artifact_ticks
            .retain(|tick| tick.instance_id() != instance_id);
        let ticks = chat.snapshot.artifact_ticks.clone();
        self.record_artifact_ticks(
            chat_id,
            &chat.snapshot.workdir,
            ticks.clone(),
            chat.snapshot.removed_artifact_ticks.clone(),
        );
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

fn normalize_artifact_tick(mut tick: ArtifactTick) -> Result<ArtifactTick, AgentError> {
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
            && !(origin.scheme() == "http" && tickloop_agent_base::network::is_loopback(&origin)))
    {
        return Err(AgentError::Invalid("invalid artifact Tick".into()));
    }
    tick.origin = tick.origin.trim_end_matches('/').to_owned();
    tick_protocol::InstanceRef::artifact(&tick.origin, &tick.artifact_id)
        .map_err(|error| AgentError::Invalid(error.into()))?;
    Ok(tick)
}

fn upsert_artifact_tick(chat: &mut super::Chat, tick: ArtifactTick) {
    if let Some(existing) = chat
        .snapshot
        .artifact_ticks
        .iter_mut()
        .find(|existing| existing.instance_id() == tick.instance_id())
    {
        let added_at_ms = existing.added_at_ms;
        *existing = tick;
        existing.added_at_ms = added_at_ms;
    } else {
        chat.snapshot.artifact_ticks.push(tick);
    }
}

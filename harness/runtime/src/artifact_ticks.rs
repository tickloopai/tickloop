use super::{Agent, AgentError};
use tickloop_harness_protocol::{ArtifactTick, ChatId};

impl Agent {
    pub(super) fn auto_attach_artifact_tick(
        &self,
        chat: &mut super::Chat,
        tick: ArtifactTick,
    ) -> Option<tick_protocol::InstanceRef> {
        let tick = normalize_artifact_tick(tick).ok()?;
        if chat
            .snapshot
            .removed_artifact_ticks
            .contains(&tick.instance_id())
        {
            return None;
        }
        let instance =
            tick_protocol::InstanceRef::artifact(&tick.origin, &tick.artifact_id).ok()?;
        upsert_artifact_tick(chat, tick);
        self.record_artifact_ticks(
            chat.snapshot.id,
            chat.snapshot.folder.as_deref(),
            chat.snapshot.artifact_ticks.clone(),
            chat.snapshot.removed_artifact_ticks.clone(),
        );
        Some(instance)
    }
    /// A confirmed rename: the Chat's artifact row and every subscription on the artifact show
    /// the new title before its metadata is read again.
    pub(super) fn retitle_artifact_tick(
        &self,
        chat: &mut super::Chat,
        instance: &tick_protocol::InstanceRef,
        title: &str,
    ) {
        self.tick_deliveries().retitle(instance, title);
        let Some(tick) = chat
            .snapshot
            .artifact_ticks
            .iter_mut()
            .find(|tick| tick.instance_id() == instance.as_str())
            .filter(|tick| tick.title != title)
        else {
            return;
        };
        tick.title = title.to_owned();
        self.record_artifact_ticks(
            chat.snapshot.id,
            chat.snapshot.folder.as_deref(),
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
        let instance = tick_protocol::InstanceRef::artifact(&tick.origin, &tick.artifact_id).ok();
        upsert_artifact_tick(chat, tick);
        self.mark_joined(chat, instance);
        let ticks = chat.snapshot.artifact_ticks.clone();
        self.record_artifact_ticks(
            chat_id,
            chat.snapshot.folder.as_deref(),
            ticks.clone(),
            chat.snapshot.removed_artifact_ticks.clone(),
        );
        Ok(ticks)
    }
    /// Unlinks the artifact from the Chat: it leaves Monitor for good and its association with
    /// the Chat's Loops ends, so its @tickloop requests no longer reach them. Tick Subscriptions
    /// on it are cancelled on their own.
    pub async fn remove_artifact_tick(
        &self,
        chat_id: ChatId,
        instance_id: &str,
    ) -> Result<Vec<ArtifactTick>, AgentError> {
        if let Ok(instance) = instance_id.parse::<tick_protocol::InstanceRef>()
            && let Some((origin, artifact_id)) = instance.artifact_parts()
        {
            self.unlink_feedback_receivers(chat_id, Some((origin, artifact_id)))
                .await?;
        }
        let ticks = {
            let mut chats = self.inner.chats.lock().await;
            let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
            chat.snapshot
                .removed_artifact_ticks
                .insert(instance_id.to_owned());
            chat.snapshot
                .artifact_ticks
                .retain(|tick| tick.instance_id() != instance_id);
            if let Ok(instance) = instance_id.parse::<tick_protocol::InstanceRef>()
                && let Some((origin, artifact_id)) = instance.artifact_parts()
            {
                self.unlink_artifact(chat, origin, artifact_id);
            }
            let ticks = chat.snapshot.artifact_ticks.clone();
            self.record_artifact_ticks(
                chat_id,
                chat.snapshot.folder.as_deref(),
                ticks.clone(),
                chat.snapshot.removed_artifact_ticks.clone(),
            );
            ticks
        };
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
    if tickloop_harness_artifacts::artifact_type(&tick.format).is_none()
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
            && !(origin.scheme() == "http" && tickloop_harness_base::network::is_loopback(&origin)))
    {
        return Err(AgentError::Invalid("invalid artifact Tick".into()));
    }
    tick.origin = tick.origin.trim_end_matches('/').to_owned();
    tick.artifact_id = tickloop_harness_artifacts::canonical_artifact_id(&tick.artifact_id);
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

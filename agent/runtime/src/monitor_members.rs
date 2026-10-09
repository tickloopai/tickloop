//! A Chat's Monitor membership: the Tick Instances it opened or explicitly added, and the ones the
//! user removed from view. Membership never subscribes, reads or calls the model; removal only
//! hides an instance and leaves its process, subscriptions and feedback associations untouched.

use tick_protocol::InstanceRef;

use super::{Agent, AgentError};
use tickloop_agent_protocol::{ChatId, MonitorMembership};

impl Agent {
    pub(super) fn record_opened_tick(
        &self,
        chat: &mut super::Chat,
        instance: InstanceRef,
        title: Option<String>,
    ) {
        chat.snapshot.monitor.record_opened(instance, title);
        self.record_monitor_membership(
            chat.snapshot.id,
            &chat.snapshot.workdir,
            chat.snapshot.monitor.clone(),
        );
    }

    /// Adds an existing instance to the Chat's Monitor and clears its removal mark. Adding
    /// neither transfers ownership nor grants read, modify or subscribe permission.
    pub async fn add_monitor_member(
        &self,
        chat_id: ChatId,
        instance: InstanceRef,
        title: Option<String>,
    ) -> Result<MonitorMembership, AgentError> {
        self.update_monitor(chat_id, |membership| membership.add(instance, title))
            .await
    }

    /// Joins the instance reopened in place of `lost` to the Chat's Monitor as one it opened,
    /// under the lost one's title; the lost row stays until it is removed.
    pub async fn record_reopened_tick(
        &self,
        chat_id: ChatId,
        lost: &InstanceRef,
        reopened: InstanceRef,
    ) -> Result<MonitorMembership, AgentError> {
        self.update_monitor(chat_id, |membership| {
            let title = membership
                .members
                .iter()
                .find(|member| &member.instance == lost)
                .and_then(|member| member.title.clone());
            membership.record_opened(reopened, title);
        })
        .await
    }

    /// Removes an instance from the Chat's Monitor view only.
    pub async fn remove_monitor_member(
        &self,
        chat_id: ChatId,
        instance: InstanceRef,
    ) -> Result<MonitorMembership, AgentError> {
        self.update_monitor(chat_id, |membership| membership.remove(instance))
            .await
    }

    async fn update_monitor(
        &self,
        chat_id: ChatId,
        change: impl FnOnce(&mut MonitorMembership),
    ) -> Result<MonitorMembership, AgentError> {
        let mut chats = self.inner.chats.lock().await;
        let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
        change(&mut chat.snapshot.monitor);
        let membership = chat.snapshot.monitor.clone();
        self.record_monitor_membership(chat_id, &chat.snapshot.workdir, membership.clone());
        Ok(membership)
    }
}

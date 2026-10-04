//! A Chat's Monitor membership: the Tick Instances it opened or explicitly added, and the ones the
//! user removed from view. Membership never subscribes, reads or calls the model; removal only
//! hides an instance and leaves its process, subscriptions and feedback associations untouched.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use tick_connect::instance::InstanceRef;

use super::{Agent, AgentError, ChatId};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MonitorMemberSource {
    /// This Chat opened the instance; the Chat owns its lifecycle.
    Opened,
    /// The user added an existing instance; ownership and permissions stay where they were.
    Added,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct MonitorMember {
    pub instance: InstanceRef,
    pub source: MonitorMemberSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct MonitorMembership {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<MonitorMember>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub removed: BTreeSet<InstanceRef>,
}

impl MonitorMembership {
    pub fn is_empty(&self) -> bool {
        self.members.is_empty() && self.removed.is_empty()
    }

    pub fn is_removed(&self, instance: &InstanceRef) -> bool {
        self.removed.contains(instance)
    }

    /// What a Branch or copied Chat starts with: no opened or added Ticks, because a new Chat
    /// owns nothing, but the same removals, so copied Loop subscriptions do not bring back rows
    /// the user removed.
    pub fn for_new_chat(&self) -> Self {
        Self {
            members: Vec::new(),
            removed: self.removed.clone(),
        }
    }

    /// Records a verified open. A removal mark stays: only an explicit add brings a row back.
    fn record_opened(&mut self, instance: InstanceRef, title: Option<String>) {
        self.upsert(instance, title).source = MonitorMemberSource::Opened;
    }

    /// An explicit add clears the removal mark and keeps an earlier Opened source.
    fn add(&mut self, instance: InstanceRef, title: Option<String>) {
        self.removed.remove(&instance);
        self.upsert(instance, title);
    }

    fn remove(&mut self, instance: InstanceRef) {
        self.removed.insert(instance);
    }

    fn upsert(&mut self, instance: InstanceRef, title: Option<String>) -> &mut MonitorMember {
        let index = match self
            .members
            .iter()
            .position(|member| member.instance == instance)
        {
            Some(index) => index,
            None => {
                self.members.push(MonitorMember {
                    instance,
                    source: MonitorMemberSource::Added,
                    title: None,
                });
                self.members.len() - 1
            }
        };
        let member = &mut self.members[index];
        if title.is_some() {
            member.title = title;
        }
        member
    }
}

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

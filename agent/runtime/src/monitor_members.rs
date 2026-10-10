//! A Chat's Monitor membership: the Tick Instances it opened, the ones the user removed from
//! view, and when the Chat last handled each instance's updates. Membership never subscribes,
//! reads or calls the model; removal only hides an instance and leaves its process,
//! subscriptions and feedback associations untouched.
//!
//! An update is handled once it enters one of this Chat's Agent turns (a subscription delivery,
//! a read tool on the instance, a completion notification or a claimed feedback request) or the
//! user opens the instance's row; a later update is unread. A turn taking updates in is recorded
//! apart from the other handling, so a View can show the update going to the Agent.

use std::path::{Path, PathBuf};

use serde_json::Value;
use tick_protocol::InstanceRef;

use super::{Agent, AgentError, Chat};
use tickloop_agent_protocol::{ChatId, MonitorMembership, PendingInput, TICK_SUBSCRIPTION_SOURCE};

/// What a completed tool call read, as far as Monitor's unread updates care.
pub(super) enum ReadTarget {
    Tick(InstanceRef),
    /// A file the Read tool read; a Shell's output file stands for the Shell.
    File(PathBuf),
    Artifact(String),
}

impl ReadTarget {
    pub(super) fn of(name: &str, input: &Value) -> Option<Self> {
        match name {
            crate::tick_tools::READ_TICK => input["tick_instance_id"]
                .as_str()?
                .parse()
                .ok()
                .map(Self::Tick),
            tickloop_agent_tools::READ_TOOL_NAME => input["file_path"]
                .as_str()
                .map(|path| Self::File(path.into())),
            crate::mcp_tools::ARTIFACT_READ_TOOL
                if matches!(input["action"].as_str(), Some("read" | "threads")) =>
            {
                input["artifact_id"]
                    .as_str()
                    .map(|id| Self::Artifact(id.into()))
            }
            crate::mcp_tools::ARTIFACT_COMMENT_TOOL => input["artifact_id"]
                .as_str()
                .map(|id| Self::Artifact(id.into())),
            _ => None,
        }
    }

    /// The Monitor instance this read covers in `chat`, if any.
    fn instance(self, chat: &Chat) -> Option<InstanceRef> {
        match self {
            Self::Tick(instance) => Some(instance),
            Self::File(path) => shell_with_output(chat, &path),
            Self::Artifact(id) => chat
                .snapshot
                .artifact_ticks
                .iter()
                .find(|tick| tick.artifact_id == id)
                .and_then(|tick| InstanceRef::artifact(&tick.origin, &tick.artifact_id).ok()),
        }
    }
}

/// The Shell whose output file is `path`: an Agent command or a user Shell of the Chat.
fn shell_with_output(chat: &Chat, path: &Path) -> Option<InstanceRef> {
    chat.snapshot
        .commands
        .iter()
        .find(|command| command.output_ref == path)
        .map(|command| command.operation_id)
        .or_else(|| {
            chat.snapshot
                .user_shells
                .iter()
                .find(|shell| shell.output_ref.as_deref() == Some(path))
                .map(|shell| shell.id)
        })
        .map(InstanceRef::shell)
}

impl Agent {
    pub(super) fn record_opened_tick(
        &self,
        chat: &mut Chat,
        instance: InstanceRef,
        title: Option<String>,
    ) {
        chat.snapshot.monitor.record_opened(instance.clone(), title);
        // What the instance did before it joined is no news for this Chat.
        chat.snapshot.monitor.mark_handled(instance, now_ms());
        self.record_monitor_membership(
            chat.snapshot.id,
            &chat.snapshot.workdir,
            chat.snapshot.monitor.clone(),
        );
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
            membership.record_opened(reopened.clone(), title);
            membership.mark_handled(reopened, now_ms());
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

    /// The user opened the instance's Monitor row: its updates so far count as handled.
    pub async fn mark_monitor_handled(
        &self,
        chat_id: ChatId,
        instance: InstanceRef,
    ) -> Result<MonitorMembership, AgentError> {
        self.update_monitor(chat_id, |membership| {
            membership.mark_handled(instance, now_ms());
        })
        .await
    }

    /// Records that one of this Chat's turns took in the instances' updates.
    pub(super) fn mark_taken_in(
        &self,
        chat: &mut Chat,
        instances: impl IntoIterator<Item = InstanceRef>,
    ) {
        self.record_handled(chat, instances, MonitorMembership::mark_taken_in);
    }

    /// Records that what the instances went through before they joined the Chat's Monitor is no
    /// news for it; no turn took anything in.
    pub(super) fn mark_joined(
        &self,
        chat: &mut Chat,
        instances: impl IntoIterator<Item = InstanceRef>,
    ) {
        self.record_handled(chat, instances, MonitorMembership::mark_handled);
    }

    fn record_handled(
        &self,
        chat: &mut Chat,
        instances: impl IntoIterator<Item = InstanceRef>,
        mark: fn(&mut MonitorMembership, InstanceRef, u64) -> bool,
    ) {
        let now = now_ms();
        let mut moved = false;
        for instance in instances {
            moved |= mark(&mut chat.snapshot.monitor, instance, now);
        }
        if moved {
            self.record_monitor_membership(
                chat.snapshot.id,
                &chat.snapshot.workdir,
                chat.snapshot.monitor.clone(),
            );
        }
    }

    /// A completed tool call whose result the turn reads next handled what it read.
    pub(super) fn mark_read_handled(&self, chat: &mut Chat, target: ReadTarget) {
        if let Some(instance) = target.instance(chat) {
            self.mark_taken_in(chat, [instance]);
        }
    }

    /// The run starting now with `pending` takes in what it delivers: the Tick Updates of a
    /// subscription delivery, the artifact of a claimed feedback request, and the completion
    /// notifications already written into the conversation.
    pub(super) fn mark_run_inputs_handled(&self, chat: &mut Chat, pending: &PendingInput) {
        let mut instances = std::mem::take(&mut chat.bookkeeping.unread_notifications);
        if let Some(source) = pending
            .trigger_source
            .as_ref()
            .filter(|source| source.source == TICK_SUBSCRIPTION_SOURCE)
        {
            instances.extend(
                source.payload["items"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|item| serde_json::from_value(item["instance"].clone()).ok()),
            );
        }
        if let Some(claim) = chat.active_event_claims.get(&pending.receipt.run_id) {
            instances.extend(
                InstanceRef::artifact(&claim.source.origin, &claim.source.artifact_id).ok(),
            );
        }
        self.mark_taken_in(chat, instances);
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

pub(super) fn now_ms() -> u64 {
    tickloop_agent_base::private_fs::unix_time_milliseconds()
}

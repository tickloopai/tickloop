//! A Chat's Monitor membership: the Tick Instances it opened, the ones the user removed from
//! view, and when the Chat last handled each instance's updates. Membership never subscribes,
//! reads or calls the model; removal only hides an instance and leaves its process,
//! subscriptions and feedback associations untouched.
//!
//! An update is handled once it enters one of this Chat's Agent turns (a subscription delivery,
//! a read tool on the instance, a completion notification or a claimed feedback request) or the
//! user opens the instance's row; a later update is unread. A turn taking updates in (all but a
//! read) is recorded apart from the other handling, so a View can show the update going to the
//! Agent, and so is the Agent reading or operating the instance with a tool, so a View can show
//! the Agent using it.

use std::path::{Path, PathBuf};

use serde_json::Value;
use tick_protocol::InstanceRef;

use super::{Agent, AgentError, Chat};
use tickloop_agent_protocol::{
    ChatId, LoopState, MonitorMembership, PendingInput, TICK_MANUAL_SOURCE,
    TICK_SUBSCRIPTION_SOURCE,
};
use uuid::Uuid;

/// What a completed tool call read or operated, as far as Monitor cares.
pub(super) struct ToolUse {
    target: UseTarget,
    /// Whether the call read the target, which handles its updates; an operation does not.
    reads: bool,
}

enum UseTarget {
    Tick(InstanceRef),
    /// A file the Read tool read; a Shell's output file stands for the Shell.
    File(PathBuf),
    Artifact(String),
}

impl ToolUse {
    pub(super) fn of(name: &str, input: &Value) -> Option<Self> {
        let tick = || {
            input["tick_instance_id"]
                .as_str()?
                .parse()
                .ok()
                .map(UseTarget::Tick)
        };
        let artifact = || {
            input["artifact_id"]
                .as_str()
                .map(|id| UseTarget::Artifact(id.into()))
        };
        let (target, reads) = match name {
            crate::tick_tools::TICK_DESCRIBE => (tick()?, false),
            crate::tick_tools::TICK_READ => (tick()?, true),
            crate::tick_tools::TICK_CALL => (tick()?, false),
            crate::tick_tools::TICK_MANAGE if input["action"].as_str() == Some("send_input") => {
                (tick()?, false)
            }
            tickloop_agent_tools::READ_TOOL_NAME => {
                (UseTarget::File(input["file_path"].as_str()?.into()), true)
            }
            crate::mcp_tools::ARTIFACT_READ_TOOL
                if matches!(input["action"].as_str(), Some("read" | "threads")) =>
            {
                (artifact()?, true)
            }
            crate::mcp_tools::ARTIFACT_COMMENT_TOOL => (artifact()?, true),
            crate::mcp_tools::ARTIFACT_EDIT_TOOL => (artifact()?, false),
            _ => return None,
        };
        Some(Self { target, reads })
    }
}

impl UseTarget {
    /// The Monitor instance this target stands for in `chat`, if any.
    fn instance(self, chat: &Chat) -> Option<InstanceRef> {
        match self {
            Self::Tick(instance) => Some(instance),
            Self::File(path) => shell_with_output(chat, &path),
            Self::Artifact(id) => chat
                .snapshot
                .artifact_ticks
                .iter()
                .find(|tick| tickloop_agent_artifacts::artifact_ids_match(&tick.artifact_id, &id))
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
        is_terminal: bool,
    ) {
        chat.snapshot
            .monitor
            .record_opened(instance.clone(), title)
            .is_terminal = is_terminal;
        // What the instance did before it joined is no news for this Chat.
        chat.snapshot.monitor.mark_handled(instance, now_ms());
        self.record_monitor_membership(
            chat.snapshot.id,
            chat.snapshot.folder.as_deref(),
            chat.snapshot.monitor.clone(),
        );
    }

    /// Joins the instance reopened in place of `lost` to the Chat's Monitor as one it opened,
    /// under the lost one's title, and takes the lost row out: one row stays.
    pub async fn record_reopened_tick(
        &self,
        chat_id: ChatId,
        lost: &InstanceRef,
        reopened: InstanceRef,
    ) -> Result<MonitorMembership, AgentError> {
        let membership = self
            .update_monitor(chat_id, |membership| {
                let title = membership
                    .members
                    .iter()
                    .find(|member| &member.instance == lost)
                    .and_then(|member| member.title.clone());
                let is_terminal = membership
                    .members
                    .iter()
                    .any(|member| &member.instance == lost && member.is_terminal);
                membership
                    .record_opened(reopened.clone(), title)
                    .is_terminal = is_terminal;
                membership.mark_handled(reopened, now_ms());
                membership.remove(lost.clone());
            })
            .await?;
        self.sync_tick_subscriptions().await;
        Ok(membership)
    }

    /// Removes an instance from the Chat's Monitor view only.
    pub async fn remove_monitor_member(
        &self,
        chat_id: ChatId,
        instance: InstanceRef,
    ) -> Result<MonitorMembership, AgentError> {
        let membership = self
            .update_monitor(chat_id, |membership| membership.remove(instance))
            .await?;
        self.sync_tick_subscriptions().await;
        Ok(membership)
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
                chat.snapshot.folder.as_deref(),
                chat.snapshot.monitor.clone(),
            );
        }
    }

    /// A tool call that completed used what it read or operated, and a read, whose result the
    /// turn reads next, handled the updates so far without taking them in as a delivery does.
    pub(super) fn mark_tool_use(&self, chat: &mut Chat, tool_use: ToolUse) {
        let Some(instance) = tool_use.target.instance(chat) else {
            return;
        };
        let now = now_ms();
        let monitor = &mut chat.snapshot.monitor;
        let is_used = monitor.mark_used(instance.clone(), now);
        let is_handled = tool_use.reads && monitor.mark_handled(instance, now);
        if is_used || is_handled {
            self.record_monitor_membership(
                chat.snapshot.id,
                chat.snapshot.folder.as_deref(),
                chat.snapshot.monitor.clone(),
            );
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
        if let Some(claim) = chat.feedback_inputs.get(&pending.receipt.run_id) {
            instances.extend(InstanceRef::artifact(&claim.origin, &claim.artifact_id).ok());
        }
        if let Some(source) = pending
            .trigger_source
            .as_ref()
            .filter(|source| source.source == TICK_MANUAL_SOURCE)
        {
            for item in source.payload["items"].as_array().into_iter().flatten() {
                if let (Ok(instance), Some(through_ms)) = (
                    serde_json::from_value(item["instance"].clone()),
                    item["through_ms"].as_u64(),
                ) {
                    chat.snapshot
                        .monitor
                        .mark_taken_in_through(instance, through_ms, now_ms());
                }
            }
            self.record_monitor_membership(
                chat.snapshot.id,
                chat.snapshot.folder.as_deref(),
                chat.snapshot.monitor.clone(),
            );
        }
        self.mark_taken_in(chat, instances);
    }

    /// Whether the Chat's Monitor may still list its Shell task `task`: as Monitor lists the task
    /// by itself unless the user removed it, or while a Loop of the Chat subscribes to it. Only a
    /// row a View keeps for its open detail is left out, which no Agent knows of.
    pub(crate) async fn monitor_may_list_shell(&self, chat_id: ChatId, task: Uuid) -> bool {
        let Ok(commands) = self.commands(chat_id).await else {
            return false;
        };
        let chats = self.inner.chats.lock().await;
        let Some(chat) = chats.get(&chat_id) else {
            return false;
        };
        let instance = InstanceRef::shell(task);
        let is_subscribed = chat
            .snapshot
            .loops
            .iter()
            .filter(|goal| !matches!(goal.state, LoopState::Stopped | LoopState::Expired))
            .flat_map(|goal| {
                goal.trigger
                    .iter()
                    .flat_map(|trigger| &trigger.subscriptions)
            })
            .any(|subscription| subscription.instance == instance);
        is_subscribed
            || (!chat.snapshot.monitor.is_removed(&instance)
                && crate::shell_task_center::project_tasks(&chat.snapshot, commands)
                    .iter()
                    .find(|listed| listed.id == task)
                    .is_some_and(|listed| {
                        listed.is_listed_in_monitor(
                            chat.snapshot.monitor.handled_at(&instance),
                            now_ms(),
                        )
                    }))
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
        self.record_monitor_membership(
            chat_id,
            chat.snapshot.folder.as_deref(),
            membership.clone(),
        );
        Ok(membership)
    }
}

pub(super) fn now_ms() -> u64 {
    tickloop_agent_base::private_fs::unix_time_milliseconds()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::mcp_tools::{ARTIFACT_COMMENT_TOOL, ARTIFACT_EDIT_TOOL, ARTIFACT_READ_TOOL};

    /// Whether a call of `name` with `input` uses the artifact `doc`, and then whether it reads it.
    fn artifact_use(name: &str, input: Value) -> Option<bool> {
        let tool_use = ToolUse::of(name, &input)?;
        matches!(&tool_use.target, UseTarget::Artifact(id) if id == "doc").then_some(tool_use.reads)
    }

    #[test]
    fn artifact_tools_read_or_operate_the_artifact_they_name() {
        for action in ["read", "threads"] {
            assert_eq!(
                artifact_use(
                    ARTIFACT_READ_TOOL,
                    json!({"action": action, "artifact_id": "doc"})
                ),
                Some(true),
                "{action}"
            );
        }
        assert_eq!(
            artifact_use(ARTIFACT_READ_TOOL, json!({"action": "list"})),
            None
        );
        assert_eq!(
            artifact_use(
                ARTIFACT_COMMENT_TOOL,
                json!({"action": "done", "artifact_id": "doc"})
            ),
            Some(true)
        );
        assert_eq!(
            artifact_use(
                ARTIFACT_EDIT_TOOL,
                json!({"action": "patch", "artifact_id": "doc", "patches": []})
            ),
            Some(false),
            "an edit operates the artifact without reading it"
        );
        assert_eq!(
            artifact_use(
                ARTIFACT_EDIT_TOOL,
                json!({"action": "create", "title": "Doc"})
            ),
            None,
            "a new artifact has no row to flash yet"
        );
    }
}

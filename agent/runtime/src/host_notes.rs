//! Host notes: user-role messages the host writes for the model alone, each announced once in the
//! conversation and checkpointed so a resumed Chat does not announce it again. The host tool set
//! can change between requests (an Artifact sign-in, a resumed Chat in a newer host) while history
//! still claims the old set; and after the user leaves Plan mode without approving a plan, history
//! may still hold the model to Plan's read-only rules.

use std::collections::BTreeSet;

use tickloop_agent_protocol::{
    ChatSnapshot, Content, ExecutionMode, HostNote, Message, Receipt, Role,
};
use uuid::Uuid;

use crate::{Agent, AgentError, Chat};

impl Agent {
    /// Announces a changed host tool set into the active run's history, checkpointed so a resumed
    /// Chat does not announce it again. `current` is `None` while the tool set is unconfirmed,
    /// which keeps the last announcement.
    pub(super) async fn announce_host_tool_changes(
        &self,
        receipt: &Receipt,
        current: Option<BTreeSet<String>>,
    ) -> Result<(), AgentError> {
        let Some(current) = current else {
            return Ok(());
        };
        let (snapshot, bookkeeping) = {
            let mut chats = self.inner.chats.lock().await;
            let chat = chats
                .get_mut(&receipt.chat_id)
                .ok_or(AgentError::NotFound)?;
            if chat.active_run != Some(receipt.run_id)
                || !announce_tool_changes(chat, receipt, current)
            {
                return Ok(());
            }
            (chat.snapshot.clone(), chat.bookkeeping.clone())
        };
        self.checkpoint_transcript(&snapshot, &bookkeeping)
            .await
            .map_err(AgentError::Storage)
    }

    /// Claude Code's `plan_mode_exit` attachment: the first request after the user left Plan mode
    /// without approving a plan tells the model once that plan mode ended. A Chat back in Plan by
    /// then drops the notice.
    pub(super) async fn announce_plan_mode_exit(
        &self,
        receipt: &Receipt,
    ) -> Result<(), AgentError> {
        let (snapshot, bookkeeping) = {
            let mut chats = self.inner.chats.lock().await;
            let chat = chats
                .get_mut(&receipt.chat_id)
                .ok_or(AgentError::NotFound)?;
            if chat.active_run != Some(receipt.run_id)
                || !std::mem::take(&mut chat.needs_plan_mode_exit_notice)
                || chat.snapshot.policy.mode == ExecutionMode::Plan
            {
                return Ok(());
            }
            let plan_file = chat
                .snapshot
                .folder
                .as_deref()
                .and_then(|folder| {
                    crate::plan::current_plan_file(
                        &chat.bookkeeping,
                        self.plans_directory(folder).path(),
                        receipt.loop_id,
                    )
                })
                .filter(|path| path.exists());
            append_note(
                &mut chat.snapshot,
                receipt,
                crate::plan::plan_mode_exit_notice(plan_file.as_deref()),
                HostNote::PlanModeExit,
            );
            (chat.snapshot.clone(), chat.bookkeeping.clone())
        };
        self.checkpoint_transcript(&snapshot, &bookkeeping)
            .await
            .map_err(AgentError::Storage)
    }
}

fn append_note(snapshot: &mut ChatSnapshot, receipt: &Receipt, text: String, note: HostNote) {
    let index = snapshot.messages.len();
    snapshot.message_runs.insert(index, receipt.run_id);
    snapshot.message_ids.insert(index, Uuid::new_v4());
    snapshot.host_notes.insert(index, note);
    snapshot.messages.push(Message {
        role: Role::User,
        content: vec![Content::Text { text }],
    });
}

/// Appends a note when `current` differs from the host tools this history was last told about,
/// and reports whether it did. A history that has not reached the model yet only records the set:
/// the first request already lists every tool.
fn announce_tool_changes(chat: &mut Chat, receipt: &Receipt, current: BTreeSet<String>) -> bool {
    let snapshot = &mut chat.snapshot;
    if snapshot.announced_host_tools.as_ref() == Some(&current) {
        return false;
    }
    let has_reached_model = !snapshot.message_compactions.is_empty()
        || snapshot
            .messages
            .iter()
            .any(|message| message.role == Role::Assistant);
    let text = match snapshot.announced_host_tools.take() {
        _ if !has_reached_model => None,
        None if current.is_empty() => None,
        None => Some(format!(
            "Host notice: host tools available in this conversation now: {}. Earlier statements in this conversation about tool availability may be outdated.",
            current.iter().cloned().collect::<Vec<_>>().join(", ")
        )),
        Some(announced) => changed_tools_text(&announced, &current),
    };
    let Some(mut text) = text else {
        snapshot.announced_host_tools = Some(current);
        return false;
    };
    text.push_str(
        " This notice comes from the host, not the user, and does not authorize any new work.",
    );
    append_note(
        snapshot,
        receipt,
        text,
        HostNote::HostTools(current.clone()),
    );
    snapshot.announced_host_tools = Some(current);
    true
}

fn changed_tools_text(announced: &BTreeSet<String>, current: &BTreeSet<String>) -> Option<String> {
    let added = current.difference(announced).cloned().collect::<Vec<_>>();
    let removed = announced.difference(current).cloned().collect::<Vec<_>>();
    if added.is_empty() && removed.is_empty() {
        return None;
    }
    let mut text = String::from("Host notice: the host tool set changed.");
    if !added.is_empty() {
        text.push_str(&format!(
            " Now available: {}. Earlier statements in this conversation that these tools are unavailable are outdated.",
            added.join(", ")
        ));
    }
    if !removed.is_empty() {
        text.push_str(&format!(
            " No longer available: {}. Do not call them; earlier statements that they are available are outdated.",
            removed.join(", ")
        ));
    }
    Some(text)
}

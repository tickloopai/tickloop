//! Narrow reads of one Chat that copy only what they answer, so a reader never holds the Chat
//! lock for the length of the conversation the way a full snapshot copy does.

use std::path::PathBuf;

use super::*;
use tickloop_harness_protocol::{
    ChatStatus, chat_display_title, chat_has_pending_work, chat_has_work, chat_preview,
};

/// What an Agent row shows about a Chat.
#[derive(Clone, Debug)]
pub struct ChatSummary {
    pub folder: Option<PathBuf>,
    pub title: String,
    pub preview: Option<String>,
    pub has_work: bool,
    /// Whether work is still coming while the Loop is idle; see `chat_has_pending_work`.
    pub pending_work: bool,
    pub chat_loop: LoopSnapshot,
    pub user_shells: Vec<UserShellRecord>,
    pub waiting_permissions: usize,
    pub waiting_questions: usize,
    /// The Chat that delegated this one; `None` for a root Chat.
    pub parent_chat: Option<ChatId>,
}

impl Chat {
    /// Read views expose effective queue eligibility: inputs already claimed by a running
    /// prompt preparation cannot independently be sent now. Only the exported copy changes.
    pub(super) fn project_pending_input_eligibility(&self, pending: &mut [PendingInput]) {
        for input in pending {
            input.armed &= !self
                .hook_state
                .session
                .preparing_inputs
                .values()
                .any(|reserved| reserved.contains(&input.receipt.run_id));
        }
    }
}

impl Agent {
    /// What the Chat is doing now, without its history.
    pub async fn status(&self, chat: ChatId) -> Result<ChatStatus, AgentError> {
        let chats = self.inner.chats.lock().await;
        let read = chats.get(&chat).ok_or(AgentError::NotFound)?;
        let mut status = ChatStatus::of(&read.snapshot);
        read.project_pending_input_eligibility(&mut status.pending_inputs);
        status.mode_before_plan = read.snapshot.policy.mode_before_plan;
        status.task_board = shared_tasks::view(&chats, chat);
        status.provider_failures = self.inner.provider_failures.lock().unwrap().clone();
        Ok(status)
    }

    pub async fn summary(&self, chat: ChatId) -> Result<ChatSummary, AgentError> {
        let chats = self.inner.chats.lock().await;
        let snapshot = &chats.get(&chat).ok_or(AgentError::NotFound)?.snapshot;
        let title = chat_display_title(snapshot, &snapshot.user_shells);
        Ok(ChatSummary {
            folder: snapshot.folder.clone(),
            preview: chat_preview(snapshot, &title),
            title,
            has_work: chat_has_work(
                snapshot,
                &snapshot.user_shells,
                snapshot.pending_inputs.len() + snapshot.pending_commands.len(),
            ),
            pending_work: chat_has_pending_work(
                snapshot,
                &snapshot.commands,
                &snapshot.user_shells,
                snapshot.pending_inputs.len() + snapshot.pending_commands.len(),
            ),
            chat_loop: snapshot.chat_loop.clone(),
            user_shells: snapshot.user_shells.clone(),
            waiting_permissions: snapshot.permissions.len(),
            waiting_questions: snapshot.questions.len(),
            parent_chat: snapshot
                .delegation
                .as_ref()
                .map(|delegation| delegation.parent_chat_id),
        })
    }

    /// The operation the run recorded last for the tool call.
    pub async fn operation(
        &self,
        chat: ChatId,
        run_id: RunId,
        call_id: &str,
    ) -> Result<Option<OperationRecord>, AgentError> {
        Ok(self
            .inner
            .chats
            .lock()
            .await
            .get(&chat)
            .ok_or(AgentError::NotFound)?
            .snapshot
            .operations
            .iter()
            .rev()
            .find(|operation| operation.run_id == run_id && operation.call_id == call_id)
            .cloned())
    }

    /// The events of the Chat's replay window after sequence `after`, oldest first.
    pub async fn events_after(&self, chat: ChatId, after: u64) -> Result<Vec<Event>, AgentError> {
        Ok(self
            .inner
            .chats
            .lock()
            .await
            .get(&chat)
            .ok_or(AgentError::NotFound)?
            .snapshot
            .events
            .iter()
            .filter(|event| event.sequence > after)
            .cloned()
            .collect())
    }

    pub async fn last_sequence(&self, chat: ChatId) -> Result<u64, AgentError> {
        self.inner
            .chats
            .lock()
            .await
            .get(&chat)
            .map(|chat| chat.snapshot.last_sequence)
            .ok_or(AgentError::NotFound)
    }
}

//! The user context message, which leads each conversation request as Claude Code's does: the
//! instruction files and the date in the user's time zone.

use tickloop_harness_policy::instructions::{InstructionFile, user_context_message};
use tickloop_harness_protocol::{ChatId, Content, Message, Role};

use crate::{Agent, AgentError};

const CURRENT_DATE_FORMAT: &str = "%Y-%m-%d";

impl Agent {
    /// The Chat's user context message, read when the Chat has none cached; the instruction
    /// files are read on a blocking thread, outside the Chats lock.
    pub(super) async fn user_context(&self, chat_id: ChatId) -> Result<Message, AgentError> {
        {
            let chats = self.inner.chats.lock().await;
            let chat = chats.get(&chat_id).ok_or(AgentError::NotFound)?;
            if let Some(cached) = chat.user_context.get() {
                return Ok(context_message(cached.clone()));
            }
        }
        let files = self.instruction_files(chat_id).await?;
        let current_date = self.current_date();
        let text = user_context_message(&files, &current_date);
        let mut chats = self.inner.chats.lock().await;
        let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
        if chat.user_context.get().is_none() {
            chat.announced_date = Some(current_date);
        }
        Ok(context_message(
            chat.user_context.get_or_init(|| text).clone(),
        ))
    }

    /// The Chat's instruction files as they read now, read on a blocking thread outside the
    /// Chats lock.
    pub(super) async fn instruction_files(
        &self,
        chat_id: ChatId,
    ) -> Result<Vec<InstructionFile>, AgentError> {
        let folder = {
            let chats = self.inner.chats.lock().await;
            let chat = chats.get(&chat_id).ok_or(AgentError::NotFound)?;
            chat.snapshot.folder.clone()
        };
        let settings = self.inner.settings.clone();
        tokio::task::spawn_blocking(move || {
            settings
                .map(|settings| settings.instruction_files(folder.as_deref()))
                .unwrap_or_default()
        })
        .await
        .map_err(|error| AgentError::Invalid(format!("reading instruction files failed: {error}")))
    }

    /// Today's date in the user's time zone, as `YYYY-MM-DD`.
    pub(super) fn current_date(&self) -> String {
        self.scheduled_now()
            .with_timezone(&self.inner.schedule_clock.zone())
            .format(CURRENT_DATE_FORMAT)
            .to_string()
    }
}

pub(super) fn context_message(text: String) -> Message {
    Message {
        role: Role::User,
        content: vec![Content::Text { text }],
    }
}

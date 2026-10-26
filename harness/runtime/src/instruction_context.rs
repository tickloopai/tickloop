//! The user context message, which leads each conversation request as Claude Code's does: the
//! instruction files and the date in the user's time zone.

use tickloop_harness_policy::instructions::user_context_message;
use tickloop_harness_protocol::{ChatId, Content, Message, Role};

use crate::{Agent, AgentError};

const CURRENT_DATE_FORMAT: &str = "%Y-%m-%d";

impl Agent {
    /// The Chat's user context message, read when the Chat has none cached; the instruction
    /// files are read outside the Chats lock.
    pub(super) async fn user_context(&self, chat_id: ChatId) -> Result<Message, AgentError> {
        let folder = {
            let chats = self.inner.chats.lock().await;
            let chat = chats.get(&chat_id).ok_or(AgentError::NotFound)?;
            if let Some(cached) = chat.user_context.get() {
                return Ok(context_message(cached.clone()));
            }
            chat.snapshot.folder.clone()
        };
        let files = self
            .inner
            .settings
            .as_ref()
            .map(|settings| settings.instruction_files(folder.as_deref()))
            .unwrap_or_default();
        let current_date = self
            .scheduled_now()
            .with_timezone(&self.inner.schedule_clock.zone())
            .format(CURRENT_DATE_FORMAT)
            .to_string();
        let text = user_context_message(&files, &current_date);
        let chats = self.inner.chats.lock().await;
        let chat = chats.get(&chat_id).ok_or(AgentError::NotFound)?;
        Ok(context_message(
            chat.user_context.get_or_init(|| text).clone(),
        ))
    }
}

fn context_message(text: String) -> Message {
    Message {
        role: Role::User,
        content: vec![Content::Text { text }],
    }
}

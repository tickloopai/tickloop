//! The user context message, which leads each conversation request as Claude Code's does: the
//! instruction files, the date in the user's time zone and, in a git repository, the git status.

use std::path::Path;

use tickloop_harness_policy::instructions::{InstructionFile, user_context_message};
use tickloop_harness_protocol::{ChatId, Content, Message, Role};

use crate::{Agent, AgentError};

const CURRENT_DATE_FORMAT: &str = "%Y-%m-%d";

impl Agent {
    /// The Chat's user context message, read when the Chat has none cached; the instruction
    /// files are read on a blocking thread, outside the Chats lock.
    pub(super) async fn user_context(&self, chat_id: ChatId) -> Result<Message, AgentError> {
        let cached = {
            let chats = self.inner.chats.lock().await;
            let chat = chats.get(&chat_id).ok_or(AgentError::NotFound)?;
            chat.user_context.get().cloned().map(|cached| {
                let needs_environment = chat.environment.get().is_none();
                (cached, readable_folder(chat).filter(|_| needs_environment))
            })
        };
        if let Some((cached, folder)) = cached {
            // A Subagent takes its parent's user context, but reads its own environment.
            if let Some(local_folder) = folder {
                let environment = tokio::task::spawn_blocking(move || {
                    crate::environment_info::ChatEnvironment::capture(local_folder.as_deref())
                })
                .await
                .map_err(|error| {
                    AgentError::Invalid(format!("reading the environment failed: {error}"))
                })?;
                if let Some(chat) = self.inner.chats.lock().await.get(&chat_id) {
                    let _ = chat.environment.set(environment);
                }
            }
            return Ok(context_message(cached));
        }
        let files = self.instruction_files(chat_id).await?;
        let current_date = self.current_date();
        let folder = {
            let chats = self.inner.chats.lock().await;
            let chat = chats.get(&chat_id).ok_or(AgentError::NotFound)?;
            readable_folder(chat)
        };
        let (environment, git_status) = match folder {
            Some(local_folder) => tokio::task::spawn_blocking(move || {
                (
                    Some(crate::environment_info::ChatEnvironment::capture(
                        local_folder.as_deref(),
                    )),
                    local_folder
                        .as_deref()
                        .and_then(crate::environment_info::git_status),
                )
            })
            .await
            .map_err(|error| {
                AgentError::Invalid(format!("reading the environment failed: {error}"))
            })?,
            None => (None, None),
        };
        let text = user_context_message(&files, &current_date, git_status.as_deref());
        let mut chats = self.inner.chats.lock().await;
        let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
        if chat.user_context.get().is_none() {
            chat.announced_date = Some(current_date);
        }
        if let Some(environment) = environment {
            let _ = chat.environment.set(environment);
        }
        Ok(context_message(
            chat.user_context.get_or_init(|| text).clone(),
        ))
    }

    /// The Chat's instruction files as they read now, read on a blocking thread outside the
    /// Chats lock. A Folder only in the Execution Environment contributes none.
    pub(super) async fn instruction_files(
        &self,
        chat_id: ChatId,
    ) -> Result<Vec<InstructionFile>, AgentError> {
        let folder = {
            let chats = self.inner.chats.lock().await;
            let chat = chats.get(&chat_id).ok_or(AgentError::NotFound)?;
            chat.local_folder().map(Path::to_path_buf)
        };
        self.instruction_files_in(folder).await
    }

    /// The instruction files a Chat working in `folder` reads, read on a blocking thread.
    pub(super) async fn instruction_files_in(
        &self,
        folder: Option<std::path::PathBuf>,
    ) -> Result<Vec<InstructionFile>, AgentError> {
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

/// Whether the Chat's environment can be read, and where its Folder is on this machine: `None`
/// without a Folder or when the Folder's local directory is gone, `Some(None)` for a Folder the
/// Port places elsewhere, which runs nothing here.
fn readable_folder(chat: &crate::Chat) -> Option<Option<std::path::PathBuf>> {
    chat.snapshot.folder.as_ref()?;
    match chat.execution.local_folder() {
        Some(local_folder) => local_folder.is_dir().then_some(Some(local_folder)),
        None => Some(None),
    }
}

pub(super) fn context_message(text: String) -> Message {
    Message {
        role: Role::User,
        content: vec![Content::Text { text }],
    }
}

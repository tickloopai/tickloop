//! The Claude Code-format transcript a hook reads through `transcript_path`.
//!
//! Each Chat with a Folder keeps one, its Subagents their own, in the Chat's private temporary
//! directory. It is written only when asked for: the Chat's committed Lines are projected with
//! the Claude Code projection and only the lines not written yet are appended. A file someone
//! else changed or removed, or one left by an earlier process, is written again in full from the
//! whole stored Transcript, so it stays right after a compaction and after a reload. The Chat's
//! end deletes it.

use std::collections::HashMap;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tickloop_harness_base::private_fs::{PRIVATE_FILE_MODE, PrivateDirectory};
use tickloop_harness_protocol::ChatId;
use tickloop_harness_transcript::Line;
use tickloop_harness_transcript::claude_code::ClaudeCodeProjector;

use super::{Agent, AgentError};
use crate::tool_results::chat_temp_directory;

const HOOK_TRANSCRIPT_FILE: &str = "transcript.jsonl";

/// The hook transcripts written so far in this process, by Chat.
pub(super) type HookTranscripts = Arc<Mutex<HashMap<ChatId, HookTranscript>>>;

pub(super) struct HookTranscript {
    directory: PrivateDirectory,
    projector: ClaudeCodeProjector,
    /// The bytes this process wrote to the file; a file of any other length is not ours.
    written: u64,
}

impl HookTranscript {
    fn new(directory: PrivateDirectory) -> Self {
        Self {
            directory,
            projector: ClaudeCodeProjector::new(),
            written: 0,
        }
    }

    fn path(&self) -> PathBuf {
        self.directory.path().join(HOOK_TRANSCRIPT_FILE)
    }

    fn is_intact(&self) -> bool {
        std::fs::symlink_metadata(self.path())
            .is_ok_and(|metadata| metadata.is_file() && metadata.len() == self.written)
    }

    /// Starts the file again, empty, for a projection from the first Line.
    fn restart(&mut self) -> io::Result<()> {
        self.directory.create()?;
        let path = self.path();
        match std::fs::remove_file(&path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(PRIVATE_FILE_MODE)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)?;
        self.projector = ClaudeCodeProjector::new();
        self.written = 0;
        Ok(())
    }

    fn append(&mut self, lines: &[Line]) -> io::Result<()> {
        if !self.is_intact() {
            self.restart()?;
        }
        let mut appended = String::new();
        for projected in lines.iter().flat_map(|line| self.projector.apply(line)) {
            appended.push_str(&serde_json::to_string(&projected)?);
            appended.push('\n');
        }
        if appended.is_empty() {
            return Ok(());
        }
        std::fs::OpenOptions::new()
            .append(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.path())?
            .write_all(appended.as_bytes())?;
        self.written += appended.len() as u64;
        Ok(())
    }

    fn delete(&self) {
        let _ = std::fs::remove_file(self.path());
    }
}

impl Agent {
    /// Brings the Chat's Claude Code-format transcript up to date with everything the Chat has
    /// produced so far and gives its path, for a hook's `transcript_path`. `None` for a Chat
    /// without a Folder, which has no private temporary directory, and for a Chat that keeps
    /// no Transcript.
    pub async fn hook_transcript_path(
        &self,
        chat_id: ChatId,
    ) -> Result<Option<PathBuf>, AgentError> {
        let directory = {
            let chats = self.inner.chats.lock().await;
            let chat = chats.get(&chat_id).ok_or(AgentError::NotFound)?;
            if chat.tools.family_temp_directory().is_none() {
                return Ok(None);
            }
            chat_temp_directory(&chat.snapshot)
        };
        let Some(lines) = self.committed_lines(chat_id).await? else {
            return Ok(None);
        };
        let transcripts = self.inner.hook_transcripts.clone();
        tokio::task::spawn_blocking(move || {
            let mut transcripts = transcripts.lock().unwrap();
            let transcript = transcripts
                .entry(chat_id)
                .or_insert_with(|| HookTranscript::new(directory));
            transcript.append(&lines).map(|()| transcript.path())
        })
        .await
        .map_err(|error| AgentError::Storage(format!("hook transcript did not finish: {error}")))?
        .map(Some)
        .map_err(|error| AgentError::Storage(format!("cannot write the hook transcript: {error}")))
    }

    /// Deletes the Chat's hook transcript when it ends.
    pub fn discard_hook_transcript(&self, chat_id: ChatId) {
        if let Some(transcript) = self.inner.hook_transcripts.lock().unwrap().remove(&chat_id) {
            transcript.delete();
        }
    }

    /// Deletes every hook transcript this process wrote, as its Chats end with it.
    pub(super) fn discard_hook_transcripts(&self) {
        for (_, transcript) in self.inner.hook_transcripts.lock().unwrap().drain() {
            transcript.delete();
        }
    }
}

#[cfg(test)]
mod tests;

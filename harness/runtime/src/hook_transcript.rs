//! The Claude Code-format transcript a hook reads through `transcript_path`.
//!
//! Each Chat the local Transcript store keeps has one, its Subagents their own, beside its
//! Transcript in the store (`<project>/<chat id>/`), as Claude Code keeps its session file under
//! its configuration directory. Model tools cannot write there: the sandbox denies the
//! configuration root, and the family temp directory and scratchpad they can write are
//! elsewhere. It is written only when asked for: the Chat's committed Lines are projected with
//! the Claude Code projection and only the lines not written yet are appended.
//!
//! The file is never opened again by name: it is created under a new name, renamed into place
//! and written only through the descriptor kept from creating it. Before each write the kept
//! descriptor must still be the file's only link, the name must still lead to it, and its length
//! and change time must be those of the last write; otherwise the transcript is written again in
//! full from the whole stored Transcript as a new file, which also keeps it right after a
//! compaction and after a reload. The Chat's end deletes it.

use std::collections::HashMap;
use std::fs::{File, Metadata};
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tickloop_harness_base::private_fs::{PRIVATE_FILE_MODE, create_private_directory};
use tickloop_harness_protocol::ChatId;
use tickloop_harness_transcript::Line;
use tickloop_harness_transcript::claude_code::ClaudeCodeProjector;

use super::{Agent, AgentError};

const HOOK_TRANSCRIPT_FILE: &str = "hook-transcript.jsonl";

/// The hook transcripts written so far in this process, by Chat.
pub(super) type HookTranscripts = Arc<Mutex<HashMap<ChatId, HookTranscript>>>;

pub(super) struct HookTranscript {
    path: PathBuf,
    written: Option<WrittenFile>,
    projector: ClaudeCodeProjector,
}

/// The file as this process last left it.
struct WrittenFile {
    file: File,
    stamp: FileStamp,
}

/// What identifies the file and its last write by this process. The change time catches a
/// rewrite that keeps the length only once the file clock moved on; what keeps tools from
/// rewriting the file at all is where it lives.
#[derive(PartialEq, Eq)]
struct FileStamp {
    device: u64,
    inode: u64,
    length: u64,
    changed: (i64, i64),
}

impl FileStamp {
    fn of(metadata: &Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            length: metadata.len(),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        }
    }
}

impl WrittenFile {
    /// The kept descriptor is still the file's only link, its name still leads to it, and
    /// nothing changed it since this process last wrote it.
    fn is_intact(&self, path: &Path) -> bool {
        let (Ok(held), Ok(named)) = (self.file.metadata(), std::fs::symlink_metadata(path)) else {
            return false;
        };
        held.nlink() == 1
            && FileStamp::of(&held) == self.stamp
            && (named.dev(), named.ino()) == (self.stamp.device, self.stamp.inode)
    }

    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.file.write_all(bytes)?;
        self.stamp = FileStamp::of(&self.file.metadata()?);
        Ok(())
    }
}

impl HookTranscript {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            written: None,
            projector: ClaudeCodeProjector::new(),
        }
    }

    /// A new, empty file in place of whatever has the name, for a projection from the first
    /// Line.
    fn restart(&mut self) -> io::Result<()> {
        self.written = None;
        self.projector = ClaudeCodeProjector::new();
        let directory = self.path.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "a transcript path has a parent",
            )
        })?;
        create_private_directory(directory)?;
        let created = directory.join(format!(
            ".{HOOK_TRANSCRIPT_FILE}.{}.tmp",
            tickloop_ids::random_token()
        ));
        let file = std::fs::OpenOptions::new()
            .append(true)
            .create_new(true)
            .mode(PRIVATE_FILE_MODE)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&created)?;
        if let Err(error) = std::fs::rename(&created, &self.path) {
            let _ = std::fs::remove_file(&created);
            return Err(error);
        }
        let stamp = FileStamp::of(&file.metadata()?);
        self.written = Some(WrittenFile { file, stamp });
        Ok(())
    }

    fn append(&mut self, lines: &[Line]) -> io::Result<()> {
        if !self
            .written
            .as_ref()
            .is_some_and(|written| written.is_intact(&self.path))
        {
            self.restart()?;
        }
        let mut appended = String::new();
        for projected in lines.iter().flat_map(|line| self.projector.apply(line)) {
            appended.push_str(&serde_json::to_string(&projected)?);
            appended.push('\n');
        }
        match (&mut self.written, appended.is_empty()) {
            (_, true) => Ok(()),
            (Some(written), false) => written.append(appended.as_bytes()),
            (None, false) => Err(io::Error::other("the transcript has no file")),
        }
    }

    /// Removes the file, unless its name now leads to another one.
    fn delete(&self) {
        let Some(written) = &self.written else {
            return;
        };
        if std::fs::symlink_metadata(&self.path).is_ok_and(|named| {
            (named.dev(), named.ino()) == (written.stamp.device, written.stamp.inode)
        }) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

impl Agent {
    /// Brings the Chat's Claude Code-format transcript up to date with everything the Chat has
    /// produced so far and gives its path, for a hook's `transcript_path`. `None` for a Chat
    /// the local Transcript store does not keep.
    pub async fn hook_transcript_path(
        &self,
        chat_id: ChatId,
    ) -> Result<Option<PathBuf>, AgentError> {
        let Some(lines) = self.committed_lines(chat_id).await? else {
            return Ok(None);
        };
        let Some(project) = self
            .inner
            .recorder
            .as_ref()
            .and_then(|recorder| recorder.local_project_directory(chat_id))
        else {
            return Ok(None);
        };
        let path = project
            .join(chat_id.uuid().to_string())
            .join(HOOK_TRANSCRIPT_FILE);
        let transcripts = self.inner.hook_transcripts.clone();
        tokio::task::spawn_blocking(move || {
            let mut transcripts = transcripts.lock().unwrap();
            let transcript = transcripts
                .entry(chat_id)
                .or_insert_with(|| HookTranscript::new(path));
            transcript.append(&lines).map(|()| transcript.path.clone())
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

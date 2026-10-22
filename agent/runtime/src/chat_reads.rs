//! Narrow reads of one Chat that copy only what they answer, so a reader never holds the Chat
//! lock for the length of the conversation the way a full snapshot copy does.

use std::{
    fs::OpenOptions,
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::PathBuf,
};

use super::*;
use tickloop_agent_protocol::{
    ChatStatus, RecordedFileChangeReview, chat_display_title, chat_has_work, chat_preview,
};
use tickloop_agent_tools::file_change::{FileChangeLineKind, FileChangeReview};

const MAX_RECORDED_REVIEW_BYTES: u64 = 1024 * 1024;
const MAX_RECORDED_DIFF_BYTES: usize = 256 * 1024;

/// What an Agent row shows about a Chat.
#[derive(Clone, Debug)]
pub struct ChatSummary {
    pub folder: Option<PathBuf>,
    pub title: String,
    pub preview: Option<String>,
    pub has_work: bool,
    pub loops: Vec<LoopSnapshot>,
    pub user_shells: Vec<UserShellRecord>,
    pub waiting_permissions: usize,
    pub waiting_questions: usize,
    /// The Chat that delegated this one; `None` for a root Chat.
    pub parent_chat: Option<ChatId>,
}

impl Agent {
    /// What the Chat is doing now, without its history.
    pub async fn status(&self, chat: ChatId) -> Result<ChatStatus, AgentError> {
        let chats = self.inner.chats.lock().await;
        let mut status = ChatStatus::of(&chats.get(&chat).ok_or(AgentError::NotFound)?.snapshot);
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
            loops: snapshot.loops.clone(),
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

    /// Read only the sidecar recorded for this exact operation; never read the live target file.
    /// Missing operations/reviews return `None`; invalid or oversized recordings fail closed.
    pub async fn operation_review(
        &self,
        chat: ChatId,
        run_id: RunId,
        call_id: &str,
    ) -> Result<Option<RecordedFileChangeReview>, AgentError> {
        let Some(prepared) = self
            .operation(chat, run_id, call_id)
            .await?
            .and_then(|operation| operation.file_change)
        else {
            return Ok(None);
        };
        tokio::task::spawn_blocking(move || recorded_file_change_review(&prepared))
            .await
            .map_err(|error| {
                AgentError::Storage(format!("file review read task failed: {error}"))
            })?
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

fn review_error(reason: impl std::fmt::Display) -> AgentError {
    AgentError::Storage(format!("recorded file change review: {reason}"))
}

fn recorded_file_change_review(
    prepared: &tickloop_agent_protocol::PreparedFileChange,
) -> Result<Option<RecordedFileChangeReview>, AgentError> {
    if prepared.artifact.byte_len > MAX_RECORDED_REVIEW_BYTES {
        return Err(review_error("artifact exceeds the 1 MiB read limit"));
    }
    // Open nonblocking so a replaced sidecar cannot turn this query into a FIFO wait.
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&prepared.artifact.path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(review_error(error)),
    };
    let metadata = file.metadata().map_err(review_error)?;
    if !metadata.is_file() || metadata.len() != prepared.artifact.byte_len {
        return Err(review_error(
            "artifact type or length does not match its recording",
        ));
    }
    let mut bytes = Vec::new();
    file.take(MAX_RECORDED_REVIEW_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(review_error)?;
    if bytes.len() as u64 != prepared.artifact.byte_len {
        return Err(review_error("artifact length changed while reading"));
    }

    // The shared loader validates hash, JSON and version but reads its path without a cap.
    // Give it an owned bounded snapshot, not the original path that could grow after stat.
    let scratch = ReviewScratch::create()?;
    let path = scratch.0.join("review.json");
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .and_then(|mut file| file.write_all(&bytes))
        .map_err(review_error)?;
    let mut reference = prepared.artifact.clone();
    reference.path = path;
    let review = load_file_change_review(&reference).map_err(review_error)?;
    if review.summary() != prepared.summary {
        return Err(review_error(
            "artifact summary/path does not match the operation",
        ));
    }
    Ok(Some(RecordedFileChangeReview {
        path: review.path.clone(),
        unified_diff: recorded_unified_diff(&review)?,
    }))
}

struct ReviewScratch(PathBuf);

impl ReviewScratch {
    fn create() -> Result<Self, AgentError> {
        let path = std::env::temp_dir().join(format!("tickloop-review-{}", Uuid::new_v4()));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .map_err(review_error)?;
        Ok(Self(path))
    }
}

impl Drop for ReviewScratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(self.0.join("review.json"));
        let _ = std::fs::remove_dir(&self.0);
    }
}

fn diff_label(review: &FileChangeReview, before: bool) -> Result<String, AgentError> {
    if !(if before {
        &review.before
    } else {
        &review.after
    })
    .exists
    {
        return Ok("/dev/null".into());
    }
    let path = review
        .path
        .to_str()
        .ok_or_else(|| review_error("non-UTF-8 file path"))?;
    let label = format!("{}/{path}", if before { "a" } else { "b" });
    if label
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || c == '"' || c == '\\')
    {
        serde_json::to_string(&label).map_err(review_error)
    } else {
        Ok(label)
    }
}

fn append_diff(diff: &mut String, text: &str) -> Result<(), AgentError> {
    if text.len() > MAX_RECORDED_DIFF_BYTES.saturating_sub(diff.len()) {
        return Err(review_error(
            "unified diff exceeds the 256 KiB output limit",
        ));
    }
    diff.push_str(text);
    Ok(())
}

fn recorded_unified_diff(review: &FileChangeReview) -> Result<String, AgentError> {
    let mut diff = String::new();
    append_diff(
        &mut diff,
        &format!(
            "--- {}\n+++ {}\n",
            diff_label(review, true)?,
            diff_label(review, false)?
        ),
    )?;
    let (mut previous_old_end, mut previous_new_end) = (0, 0);
    for hunk in &review.hunks {
        let range = |start: u64, len: u64, version: &tickloop_agent_protocol::FileVersion| {
            let offset = if len == 0 {
                Some(start)
            } else {
                start.checked_sub(1)
            };
            offset
                .and_then(|offset| offset.checked_add(len).map(|end| (offset, end)))
                .filter(|(_, end)| *end <= version.line_count)
                .ok_or_else(|| review_error("invalid hunk range"))
        };
        let (old_offset, old_end) = range(hunk.old_start, hunk.old_len, &review.before)?;
        let (new_offset, new_end) = range(hunk.new_start, hunk.new_len, &review.after)?;
        if old_offset < previous_old_end || new_offset < previous_new_end {
            return Err(review_error("overlapping or out-of-order hunks"));
        }
        if old_offset - previous_old_end != new_offset - previous_new_end {
            return Err(review_error("inconsistent omitted context between hunks"));
        }
        previous_old_end = old_end;
        previous_new_end = new_end;
        append_diff(
            &mut diff,
            &format!(
                "@@ -{},{} +{},{} @@\n",
                hunk.old_start, hunk.old_len, hunk.new_start, hunk.new_len
            ),
        )?;
        let (mut old, mut new) = (old_offset, new_offset);
        for line in &hunk.lines {
            let (uses_old, uses_new, prefix) = match line.kind {
                FileChangeLineKind::Context => (true, true, " "),
                FileChangeLineKind::Added => (false, true, "+"),
                FileChangeLineKind::Removed => (true, false, "-"),
            };
            let old_line = if uses_old { old.checked_add(1) } else { None };
            let new_line = if uses_new { new.checked_add(1) } else { None };
            if (uses_old && old_line.is_none())
                || (uses_new && new_line.is_none())
                || line.old_line != old_line
                || line.new_line != new_line
                || line.text.contains('\n')
            {
                return Err(review_error("invalid recorded hunk line"));
            }
            let valid_newline =
                |number: Option<u64>, version: &tickloop_agent_protocol::FileVersion| {
                    number.is_none_or(|number| {
                        line.has_newline
                            == (number != version.line_count || version.ends_with_newline)
                    })
                };
            if !valid_newline(old_line, &review.before) || !valid_newline(new_line, &review.after) {
                return Err(review_error("invalid recorded EOF marker"));
            }
            old += u64::from(uses_old);
            new += u64::from(uses_new);
            append_diff(&mut diff, prefix)?;
            append_diff(&mut diff, &line.text)?;
            append_diff(&mut diff, "\n")?;
            if !line.has_newline {
                append_diff(&mut diff, "\\ No newline at end of file\n")?;
            }
        }
        if old != old_end || new != new_end {
            return Err(review_error("hunk lengths do not match recorded lines"));
        }
    }
    if review.before.line_count - previous_old_end != review.after.line_count - previous_new_end {
        return Err(review_error("inconsistent omitted trailing context"));
    }
    Ok(diff)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use tickloop_agent_protocol::{FileChangeAction, FileChangeArtifactRef, PreparedFileChange};
    use tickloop_agent_tools::file_change::build_file_change_review;

    fn review(before: &str, after: &str) -> FileChangeReview {
        build_file_change_review(
            FileChangeAction::Edit,
            PathBuf::from("notes.txt"),
            before,
            true,
            None,
            after,
            true,
        )
    }

    fn persist(directory: &std::path::Path, review: &FileChangeReview) -> PreparedFileChange {
        let bytes = serde_json::to_vec(review).unwrap();
        let path = directory.join(format!("{}.json", Uuid::new_v4()));
        std::fs::write(&path, &bytes).unwrap();
        PreparedFileChange {
            summary: review.summary(),
            artifact: FileChangeArtifactRef {
                path,
                byte_len: bytes.len() as u64,
                sha256: format!("{:x}", Sha256::digest(&bytes)),
            },
        }
    }

    #[test]
    fn recorded_review_keeps_exact_hunks_and_eof_markers() {
        let directory = tempfile::tempdir().unwrap();
        let prepared = persist(directory.path(), &review("same\nold", "same\nnew\n"));
        let result = recorded_file_change_review(&prepared).unwrap().unwrap();
        assert_eq!(result.path, PathBuf::from("notes.txt"));
        assert_eq!(
            result.unified_diff,
            "--- a/notes.txt\n+++ b/notes.txt\n@@ -1,2 +1,2 @@\n same\n-old\n\\ No newline at end of file\n+new\n"
        );
    }

    #[test]
    fn recorded_review_omits_unrecorded_whole_file_context() {
        let before = (0..30).map(|n| format!("line {n}\n")).collect::<String>();
        let after = before
            .replace("line 2\n", "first\n")
            .replace("line 27\n", "last\n");
        let review = review(&before, &after);
        assert_eq!(review.hunks.len(), 2);
        let diff = recorded_unified_diff(&review).unwrap();
        assert_eq!(diff.matches("@@ -").count(), 2);
        assert!(!diff.contains("line 15"));
        assert!(diff.contains("+first\n"));
        assert!(diff.contains("+last\n"));
    }

    #[test]
    fn recorded_review_create_delete_and_empty_ranges() {
        for (action, before, before_exists, after, after_exists, expected) in [
            (
                FileChangeAction::Create,
                "",
                false,
                "new\n",
                true,
                "--- /dev/null\n+++ b/notes.txt\n@@ -0,0 +1,1 @@\n+new\n",
            ),
            (
                FileChangeAction::Delete,
                "old\n",
                true,
                "",
                false,
                "--- a/notes.txt\n+++ /dev/null\n@@ -1,1 +0,0 @@\n-old\n",
            ),
            (
                FileChangeAction::Edit,
                "",
                true,
                "",
                true,
                "--- a/notes.txt\n+++ b/notes.txt\n",
            ),
        ] {
            let review = build_file_change_review(
                action,
                "notes.txt".into(),
                before,
                before_exists,
                None,
                after,
                after_exists,
            );
            assert_eq!(recorded_unified_diff(&review).unwrap(), expected);
        }
    }

    #[test]
    fn recorded_review_missing_is_distinct_from_corrupt() {
        let directory = tempfile::tempdir().unwrap();
        let prepared = persist(directory.path(), &review("old\n", "new\n"));
        std::fs::write(
            &prepared.artifact.path,
            vec![b'x'; prepared.artifact.byte_len as usize],
        )
        .unwrap();
        assert!(recorded_file_change_review(&prepared).is_err());
        std::fs::remove_file(&prepared.artifact.path).unwrap();
        assert!(recorded_file_change_review(&prepared).unwrap().is_none());
    }

    #[test]
    fn recorded_review_never_reads_current_target_file() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("notes.txt");
        let mut review = review("old\n", "new\n");
        review.path = target.clone();
        let prepared = persist(directory.path(), &review);
        std::fs::write(&target, "unrelated current contents\n").unwrap();
        let first = recorded_file_change_review(&prepared).unwrap().unwrap();
        std::fs::remove_file(&target).unwrap();
        let second = recorded_file_change_review(&prepared).unwrap().unwrap();
        assert_eq!(first, second);
        assert!(first.unified_diff.contains("-old\n+new\n"));
        assert!(!first.unified_diff.contains("unrelated"));
    }

    #[test]
    fn recorded_review_rejects_symlink_sidecar() {
        let directory = tempfile::tempdir().unwrap();
        let mut prepared = persist(directory.path(), &review("old\n", "new\n"));
        let link = directory.path().join("link.json");
        std::os::unix::fs::symlink(&prepared.artifact.path, &link).unwrap();
        prepared.artifact.path = link;
        assert!(recorded_file_change_review(&prepared).is_err());
    }

    #[test]
    fn recorded_review_rejects_summary_path_mismatch() {
        let directory = tempfile::tempdir().unwrap();
        let mut prepared = persist(directory.path(), &review("old\n", "new\n"));
        prepared.summary.path = "another.txt".into();
        assert!(recorded_file_change_review(&prepared).is_err());
    }

    #[test]
    fn recorded_review_rejects_oversized_artifact_and_output() {
        let directory = tempfile::tempdir().unwrap();
        let mut prepared = persist(directory.path(), &review("old\n", "new\n"));
        prepared.artifact.byte_len = MAX_RECORDED_REVIEW_BYTES + 1;
        assert!(recorded_file_change_review(&prepared).is_err());
        let large = "x".repeat(MAX_RECORDED_DIFF_BYTES);
        assert!(recorded_unified_diff(&review("old\n", &large)).is_err());
    }

    #[test]
    fn recorded_review_rejects_bad_hunk_coordinates_and_multiline_text() {
        let original = review("old\n", "new\n");
        let mut malformed = original.clone();
        malformed.hunks[0].old_len += 1;
        assert!(recorded_unified_diff(&malformed).is_err());
        let mut malformed = original.clone();
        malformed.hunks[0].lines[0].old_line = Some(99);
        assert!(recorded_unified_diff(&malformed).is_err());
        let mut malformed = original.clone();
        malformed.hunks[0].lines[0].text.push('\n');
        assert!(recorded_unified_diff(&malformed).is_err());
        let mut malformed = original;
        malformed.hunks[0].lines[0].has_newline = false;
        assert!(recorded_unified_diff(&malformed).is_err());
    }

    #[test]
    fn recorded_review_quotes_header_paths() {
        let mut review = review("old\n", "new\n");
        review.path = "new\nheader.txt".into();
        assert!(
            recorded_unified_diff(&review)
                .unwrap()
                .starts_with("--- \"a/new\\nheader.txt\"\n")
        );
    }

    #[tokio::test]
    async fn recorded_review_query_is_scoped_to_exact_run_and_call() {
        use tickloop_agent_test_support::{ScriptedModel, config};
        let agent = Agent::new_embedded(config(), Arc::new(ScriptedModel::new(vec![])));
        let directory = tempfile::tempdir().unwrap();
        let chat = agent
            .create_chat(directory.path().to_path_buf())
            .await
            .unwrap();
        let run = Uuid::new_v4();
        let prepared = persist(directory.path(), &review("old\n", "new\n"));
        agent
            .inner
            .chats
            .lock()
            .await
            .get_mut(&chat)
            .unwrap()
            .snapshot
            .operations
            .push(OperationRecord {
                task_binding: None,
                id: Uuid::new_v4(),
                loop_id: Uuid::new_v4(),
                run_id: run,
                call_id: "call".into(),
                name: "Edit".into(),
                input: serde_json::json!({"file_path": "notes.txt"}),
                file_change: Some(prepared),
                state: tickloop_agent_protocol::OperationState::Completed,
                result: None,
            });
        assert!(
            agent
                .operation_review(chat, run, "call")
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            agent
                .operation_review(chat, Uuid::new_v4(), "call")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            agent
                .operation_review(chat, run, "other")
                .await
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            agent.operation_review(ChatId::new_v4(), run, "call").await,
            Err(AgentError::NotFound)
        ));
        agent
            .inner
            .chats
            .lock()
            .await
            .get_mut(&chat)
            .unwrap()
            .snapshot
            .operations
            .last_mut()
            .unwrap()
            .file_change = None;
        assert!(
            agent
                .operation_review(chat, run, "call")
                .await
                .unwrap()
                .is_none()
        );
        agent.shutdown().await;
    }
}

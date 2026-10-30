//! Externalized storage for large tool results, following Claude Code's
//! `utils/toolResultStorage.ts`.
//!
//! Both mechanisms match Claude Code:
//! - **Per result**: before a tool result is written to the conversation, content over that tool's
//!   threshold is written to `<Chat directory>/tool-results/<toolUseId>.txt`, and the conversation
//!   keeps replacement text with the path and a preview.
//! - **Per-message budget**: before sending a request, when the tool results in one user message
//!   together exceed the budget, the largest results the model has never seen are externalized and
//!   replaced. Replacement decisions are written to the Transcript as content replacement entries
//!   and rebuilt on resume, keeping every later request byte-for-byte identical; results already
//!   seen but not replaced are never replaced.
//!
//! Both the budget and externalization use the tool result's text ([`ToolResultContent::text`]), so
//! the file extension is always `.txt` (Claude Code writes `.json` for content block arrays).
//! Lengths are counted in UTF-16 code units, following Claude Code.
//!
//! Call ids come from the model and may contain characters not allowed in file names, or repeat
//! across Runs (repeated ids in a request are projected to unique `tl_` ids). So the budget state
//! is keyed by the projected id in the request and rebuilt by the same projection on resume; a
//! replacement is only applied to a result whose content matches what it was generated from.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use sha2::{Digest, Sha256};

use super::{Agent, AgentError, Chat};
use crate::chat_execution::ChatExecution;
use crate::transcript::private_family_temp_directory;
use tickloop_harness_base::network::Reach;
use tickloop_harness_base::private_fs::{PRIVATE_FILE_MODE, PrivateDirectory};
use tickloop_harness_protocol::projection;
use tickloop_harness_protocol::{
    ChatId, ChatSnapshot, Content, Message, Receipt, Role, ToolResult, ToolResultContent,
};
use tickloop_harness_tools::{
    BASH_TOOL_NAME, ChatDirectory, GREP_TOOL_NAME, PERSISTED_OUTPUT_TAG, PREVIEW_LABEL,
    READ_TOOL_NAME, SAVED_TO_LABEL, TASK_OUTPUT_SUBDIRECTORY, Tools, generate_preview,
    large_output_message, utf16_length,
};
use tickloop_harness_transcript::ContentReplacementRecord;

/// Subdirectory of the Chat directory for large tool results.
const MCP_MAX_RESULT_BYTES: usize = 20 * 1024;
const MCP_PREVIEW_SIDE_BYTES: usize = 9000;
const TOOL_RESULTS_SUBDIRECTORY: &str = "tool-results";
/// Claude Code's `DEFAULT_MAX_RESULT_SIZE_CHARS`: each tool's declared limit is further capped by
/// this value.
const DEFAULT_MAX_RESULT_SIZE_CHARS: usize = 50_000;
/// Claude Code `MAX_TOOL_RESULTS_PER_MESSAGE_CHARS`。
const MAX_TOOL_RESULTS_PER_MESSAGE_CHARS: usize = 200_000;
/// The `maxResultSizeChars` declared by most Claude Code tools.
const COMMON_MAX_RESULT_SIZE_CHARS: usize = 100_000;
const MAX_TOOL_USE_ID_LENGTH: usize = 200;
/// When a call id cannot be used directly as a file name or is already taken by different content,
/// name the file "sanitized id-content hash" instead.
const FALLBACK_STEM_LENGTH: usize = 64;
const CONTENT_DIGEST_HEX_LENGTH: usize = 16;
const FALLBACK_NAME_ATTEMPTS: usize = 8;
const FALLBACK_STEM: &str = "tool-result";

impl Agent {
    /// Tools that reach no Folder, which bind no Execution Environment and create no directory.
    pub(super) fn folderless_chat_tools(&self, snapshot: &ChatSnapshot) -> Arc<Tools> {
        Arc::new(
            Tools::folderless(self.inner.tool_policy.folderless_reach)
                .with_search_configuration(
                    snapshot.search_config.clone(),
                    self.inner.config_root.clone(),
                )
                .with_search_provider_factory(self.inner.search_provider_factory.clone())
                .with_search_settings(self.inner.web_search_settings.clone()),
        )
    }

    /// The tools of a Chat whose directories `execution` is bound for.
    pub(super) fn chat_tools_on(
        &self,
        snapshot: &ChatSnapshot,
        execution: &ChatExecution,
    ) -> Arc<Tools> {
        if snapshot.folder.is_none() {
            return self.folderless_chat_tools(snapshot);
        }
        let chat_id = snapshot.id;
        let in_environment = execution.folder_in_environment();
        let family_temp =
            (!in_environment).then(|| private_family_temp_directory(family_root(snapshot).into()));
        let persistent_project = self
            .inner
            .recorder
            .as_ref()
            .and_then(|recorder| recorder.local_project_directory(chat_id));
        let temporary_chat = chat_temp_directory(snapshot);
        let chat_directory = persistent_project.map_or_else(
            || temporary_chat.clone(),
            |project| PrivateDirectory::trusted(project).join(chat_id.uuid().to_string()),
        );
        let task_output = temporary_chat.join(TASK_OUTPUT_SUBDIRECTORY);
        let shells = Arc::new(crate::environment::shell_host::EnvironmentShellHost::new(
            execution.clone(),
            Arc::new(crate::environment::shell_host::CurrentLocalShellHost::new(
                self.downgrade(),
                chat_id.into(),
            )),
        ));
        let reach = if in_environment {
            self.inner.tool_policy.folderless_reach
        } else {
            Reach::Anywhere
        };
        Arc::new(
            Tools::for_chat(
                ChatDirectory {
                    chat: chat_directory,
                    task_output,
                    family_temp,
                },
                shells,
                Arc::new(crate::environment::file_host::EnvironmentFileHost::new(
                    execution.clone(),
                )),
            )
            .with_reach(reach)
            .with_search_configuration(
                snapshot.search_config.clone(),
                self.inner.config_root.clone(),
            )
            .with_search_provider_factory(self.inner.search_provider_factory.clone())
            .with_search_settings(self.inner.web_search_settings.clone()),
        )
    }

    /// Third-party MCP results over 20 KB keep both ends; the full text goes to the Family's
    /// temporary directory, which a Chat without a Folder does not have.
    pub(super) async fn limit_third_party_mcp_result(
        &self,
        receipt: &Receipt,
        tool_use_id: &str,
        mut result: ToolResult,
    ) -> Result<ToolResult, AgentError> {
        if result.content.len() <= MCP_MAX_RESULT_BYTES {
            return Ok(result);
        }
        let family = {
            let chats = self.inner.chats.lock().await;
            let chat = chats.get(&receipt.chat_id).ok_or(AgentError::NotFound)?;
            chat.tools
                .family_temp_directory()
                .map(|_| private_family_temp_directory(family_root(&chat.snapshot).into()))
        };
        let call_id = tool_use_id.to_owned();
        tokio::task::spawn_blocking(move || {
            let persisted = family.and_then(|directory| {
                persist_tool_result(&directory, &call_id, &result.content).ok()
            });
            let note = persisted.as_ref().map_or_else(
                || "Full output could not be saved".into(),
                |output| format!("Full output saved to: {}", output.path.display()),
            );
            let head = result.content.floor_char_boundary(MCP_PREVIEW_SIDE_BYTES);
            let tail = result
                .content
                .ceil_char_boundary(result.content.len() - MCP_PREVIEW_SIDE_BYTES);
            result.content = format!(
                "{}\n\n[Output truncated in the middle. {note}]\n\n{}",
                &result.content[..head],
                &result.content[tail..]
            );
            if let Some(persisted) = persisted {
                result.output_ref = Some(persisted.path);
            }
            result
        })
        .await
        .map_err(|_| AgentError::Storage("MCP output storage did not finish".into()))
    }

    /// Externalizes tool results by threshold before they are written to the conversation;
    /// afterwards the result content becomes the replacement text, and results without their own
    /// output reference point at the externalized file. A Chat without a Folder stores nothing, so
    /// the result is cut instead.
    pub(super) async fn persist_large_tool_result(
        &self,
        receipt: &Receipt,
        tool_name: &str,
        tool_use_id: &str,
        mut result: ToolResult,
    ) -> Result<ToolResult, AgentError> {
        if tool_name.starts_with("mcp__") {
            return self
                .limit_third_party_mcp_result(receipt, tool_use_id, result)
                .await;
        }
        let chat_directory = self
            .inner
            .chats
            .lock()
            .await
            .get(&receipt.chat_id)
            .ok_or(AgentError::NotFound)?
            .tools
            .chat_directory()
            .cloned();
        let Some(chat_directory) = chat_directory else {
            if let Some(truncated) = truncate_large_tool_result(tool_name, &result.content) {
                result.content = truncated;
            }
            return Ok(result);
        };
        let tool_name = tool_name.to_owned();
        let tool_use_id = tool_use_id.to_owned();
        tokio::task::spawn_blocking(move || {
            if let Some(persisted) = persist_large_tool_result(
                &chat_directory,
                &tool_name,
                &tool_use_id,
                &result.content,
            ) {
                result.content = persisted.replacement;
                result.output_ref.get_or_insert(persisted.path);
            }
            result
        })
        .await
        .map_err(|error| {
            AgentError::Storage(format!("tool result storage did not finish: {error}"))
        })
    }
}

fn family_root(snapshot: &ChatSnapshot) -> ChatId {
    snapshot
        .delegation
        .as_ref()
        .map_or(snapshot.id, |delegation| delegation.root_chat_id())
}

/// The Chat's private temporary directory: its family's for the root, and a directory of its own
/// inside it for a Subagent.
fn chat_temp_directory(snapshot: &ChatSnapshot) -> PrivateDirectory {
    let root = family_root(snapshot);
    let family_temp = private_family_temp_directory(root.into());
    if root == snapshot.id {
        family_temp
    } else {
        family_temp.join(snapshot.id.uuid().to_string())
    }
}

/// The result a call that ends while its Run waits records for the model, without the store a
/// Chat with a Folder writes large results to: cut as [`Agent::persist_large_tool_result`] cuts
/// it in a Chat without one, and as it is otherwise.
pub(super) fn waited_call_result(
    chat: &Chat,
    tool_name: &str,
    mut result: ToolResult,
) -> ToolResult {
    if chat.tools.chat_directory().is_none()
        && let Some(truncated) = truncate_large_tool_result(tool_name, &result.content)
    {
        result.content = truncated;
    }
    result
}

/// The tool's externalization threshold; `None` means never externalize (Claude Code's Read
/// declares Infinity: externalizing a file read result to a file for the model to read back would
/// be circular).
fn persistence_threshold(tool_name: &str) -> Option<usize> {
    let declared = match tool_name {
        READ_TOOL_NAME => return None,
        GREP_TOOL_NAME => 20_000,
        BASH_TOOL_NAME => 30_000,
        _ => COMMON_MAX_RESULT_SIZE_CHARS,
    };
    Some(declared.min(DEFAULT_MAX_RESULT_SIZE_CHARS))
}

/// A result over its tool's threshold, cut to the threshold with a note on the rest, for a Chat
/// with no Folder to store it in; `None` when it is within the threshold.
fn truncate_large_tool_result(tool_name: &str, content: &str) -> Option<String> {
    let threshold = persistence_threshold(tool_name)?;
    (utf16_length(content) > threshold).then(|| truncated_result(content, threshold))
}

/// The start of `content`, at most `kept_units` UTF-16 code units, with a note on the rest.
fn truncated_result(content: &str, kept_units: usize) -> String {
    let (kept, _) = generate_preview(content, kept_units);
    format!(
        "{kept}\n\n[Output truncated: the result has {} characters; only the first {} are shown, and this Chat has no Folder to keep the rest.]",
        utf16_length(content),
        utf16_length(kept)
    )
}

/// The per-message budget of [`enforce_tool_result_budget`] for a Chat without a Folder, which
/// has nowhere to store results: each tool round over the budget has its largest results cut in
/// place to an equal share of it. The cut depends only on the messages, so every request repeats
/// it byte for byte.
pub(super) fn cut_tool_results_over_budget(messages: &mut [Message]) {
    let mut cuts = HashMap::new();
    for group in candidate_groups(messages) {
        let total = group.iter().map(|candidate| candidate.size).sum::<usize>();
        if total <= MAX_TOOL_RESULTS_PER_MESSAGE_CHARS {
            continue;
        }
        let share = MAX_TOOL_RESULTS_PER_MESSAGE_CHARS / group.len();
        let mut largest_first = group.iter().collect::<Vec<_>>();
        largest_first.sort_by(|left, right| {
            right
                .size
                .cmp(&left.size)
                .then_with(|| left.tool_use_id.cmp(right.tool_use_id))
        });
        let mut remaining = total;
        for candidate in largest_first {
            if remaining <= MAX_TOOL_RESULTS_PER_MESSAGE_CHARS || candidate.size <= share {
                break;
            }
            cuts.insert(
                candidate.tool_use_id.to_owned(),
                truncated_result(&candidate.content, share),
            );
            remaining = remaining - candidate.size + share;
        }
    }
    if cuts.is_empty() {
        return;
    }
    for content in messages
        .iter_mut()
        .filter(|message| message.role == Role::User)
        .flat_map(|message| message.content.iter_mut())
    {
        if let Content::ToolResult {
            tool_use_id,
            content,
            ..
        } = content
            && let Some(cut) = cuts.get(tool_use_id.as_str())
        {
            *content = ToolResultContent::Text(cut.clone());
        }
    }
}

pub(super) struct PersistedToolResult {
    pub path: PathBuf,
    pub replacement: String,
}

/// Following Claude Code's `maybePersistLargeToolResult`: externalizes and returns replacement text
/// when over the threshold; returns `None` when under the threshold or the write fails, and the
/// caller keeps the original content.
pub(super) fn persist_large_tool_result(
    chat_directory: &PrivateDirectory,
    tool_name: &str,
    tool_use_id: &str,
    content: &str,
) -> Option<PersistedToolResult> {
    let threshold = persistence_threshold(tool_name)?;
    if content.is_empty() || utf16_length(content) <= threshold {
        return None;
    }
    persist_tool_result(chat_directory, tool_use_id, content).ok()
}

/// Writes without overwriting. When the call id can be used directly as a file name, writes
/// `<toolUseId>.txt` following Claude Code; reuses an existing file with identical content (Claude
/// Code treats any `EEXIST` as already written). When the id contains illegal characters or the
/// file is taken by different content, writes `<sanitized id>-<content hash>.txt` instead of giving
/// up on externalization.
fn persist_tool_result(
    chat_directory: &PrivateDirectory,
    tool_use_id: &str,
    content: &str,
) -> io::Result<PersistedToolResult> {
    let directory = chat_directory.join(TOOL_RESULTS_SUBDIRECTORY);
    directory.create()?;
    for file_name in candidate_file_names(tool_use_id, content) {
        let path = directory.path().join(file_name);
        let created = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(PRIVATE_FILE_MODE)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path);
        match created {
            Ok(mut file) => {
                if let Err(error) = file.write_all(content.as_bytes()) {
                    let _ = std::fs::remove_file(&path);
                    return Err(error);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                if std::fs::read(&path)? != content.as_bytes() {
                    continue;
                }
            }
            Err(error) => return Err(error),
        }
        return Ok(PersistedToolResult {
            replacement: build_large_tool_result_message(&path, content),
            path,
        });
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!(
            "every file name for tool call {tool_use_id:?} in {} holds different content",
            directory.path().display()
        ),
    ))
}

fn candidate_file_names(tool_use_id: &str, content: &str) -> impl Iterator<Item = String> {
    let exact = is_safe_file_stem(tool_use_id).then(|| format!("{tool_use_id}.txt"));
    let mut stem = tool_use_id
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_') {
                char::from(byte)
            } else {
                '_'
            }
        })
        .take(FALLBACK_STEM_LENGTH)
        .collect::<String>();
    if stem.is_empty() {
        stem = FALLBACK_STEM.to_owned();
    }
    let digest = Sha256::digest(content.as_bytes())
        .iter()
        .take(CONTENT_DIGEST_HEX_LENGTH / 2)
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let fallbacks = (0..FALLBACK_NAME_ATTEMPTS).map(move |attempt| match attempt {
        0 => format!("{stem}-{digest}.txt"),
        _ => format!("{stem}-{digest}-{attempt}.txt"),
    });
    exact.into_iter().chain(fallbacks)
}

/// Claude Code `buildLargeToolResultMessage` for a result held entirely in `content`.
fn build_large_tool_result_message(path: &Path, content: &str) -> String {
    large_output_message(path, utf16_length(content), content)
}

fn is_safe_file_stem(tool_use_id: &str) -> bool {
    !tool_use_id.is_empty()
        && tool_use_id.len() <= MAX_TOOL_USE_ID_LENGTH
        && tool_use_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

/// Whether the replacement text was generated from this content: ids repeated across Runs or reused
/// after a rollback do not pick up another result's replacement.
fn is_replacement_for(replacement: &str, content: &str) -> bool {
    replacement
        .strip_prefix(PERSISTED_OUTPUT_TAG)
        .and_then(|rest| rest.split_once(SAVED_TO_LABEL))
        .and_then(|(_, rest)| rest.split_once(PREVIEW_LABEL))
        .is_some_and(|(path, _)| {
            build_large_tool_result_message(Path::new(path), content) == replacement
        })
}

/// Claude Code's `ContentReplacementState`: exists only at runtime and is rebuilt from content
/// replacement entries on resume.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub(super) struct ContentReplacementState {
    /// Results that have passed the budget check; they are never newly replaced afterwards.
    seen: HashSet<String>,
    /// Replaced results and the original text the model saw.
    replacements: HashMap<String, String>,
}

impl ContentReplacementState {
    /// Claude Code's `reconstructContentReplacementState`: candidate results appearing in the
    /// conversation are all treated as seen; only replacement records still present in the
    /// conversation and generated from that result's content are adopted. Candidates come from the
    /// projected conversation sent to the model, matching the keys used at request time.
    pub(super) fn reconstruct(
        snapshot: &ChatSnapshot,
        records: &[ContentReplacementRecord],
    ) -> Self {
        let messages =
            projection::projected_messages(snapshot, &BTreeMap::new()).unwrap_or_default();
        Self::reconstruct_from_messages(&messages, records)
    }

    fn reconstruct_from_messages(
        messages: &[Message],
        records: &[ContentReplacementRecord],
    ) -> Self {
        let candidates = candidate_groups(messages)
            .into_iter()
            .flatten()
            .map(|candidate| (candidate.tool_use_id, candidate.content))
            .collect::<HashMap<_, _>>();
        let replacements = records
            .iter()
            .filter_map(|record| match record {
                ContentReplacementRecord::ToolResult {
                    tool_use_id,
                    replacement,
                } if candidates
                    .get(tool_use_id.as_str())
                    .is_some_and(|content| is_replacement_for(replacement, content)) =>
                {
                    Some((tool_use_id.clone(), replacement.clone()))
                }
                _ => None,
            })
            .collect();
        Self {
            seen: candidates.keys().map(|id| (*id).to_owned()).collect(),
            replacements,
        }
    }

    /// Whether the model sees the result of `tool_use_id` as a saved file's preview.
    pub(super) fn is_replaced(&self, tool_use_id: &str) -> bool {
        self.replacements.contains_key(tool_use_id)
    }

    fn replacement_for(&self, tool_use_id: &str, content: &str) -> Option<&String> {
        self.replacements
            .get(tool_use_id)
            .filter(|replacement| is_replacement_for(replacement, content))
    }

    /// Replacement decisions still applicable to the model-visible conversation in the snapshot
    /// (after the last compaction boundary). Compaction uses this to re-record them after the
    /// boundary it writes: loading a Transcript over 5MB skips records before the boundary.
    pub(super) fn records_for(&self, snapshot: &ChatSnapshot) -> Vec<ContentReplacementRecord> {
        Self::reconstruct(snapshot, &self.records()).records()
    }

    /// Exports replacement decisions as records, for `--fork-session` and Branch to re-record under
    /// a new chatId.
    pub(super) fn records(&self) -> Vec<ContentReplacementRecord> {
        let mut records = self
            .replacements
            .iter()
            .map(
                |(tool_use_id, replacement)| ContentReplacementRecord::ToolResult {
                    tool_use_id: tool_use_id.clone(),
                    replacement: replacement.clone(),
                },
            )
            .collect::<Vec<_>>();
        records.sort_by(|left, right| left.tool_use_id().cmp(right.tool_use_id()));
        records
    }
}

struct Candidate<'a> {
    tool_use_id: &'a str,
    content: Cow<'a, str>,
    size: usize,
}

/// Claude Code's `collectCandidatesByMessage`: groups the tool results of adjacent user messages,
/// delimited by assistant messages (they are merged into one user message in the request). Results
/// that are already replaced, empty, or contain images are not candidates (Claude Code's
/// `hasImageBlock`: externalization stores only text, so replacement would drop the images).
fn candidate_groups(messages: &[Message]) -> Vec<Vec<Candidate<'_>>> {
    let mut groups = Vec::new();
    let mut current = Vec::new();
    for message in messages {
        match message.role {
            Role::Assistant => {
                if !current.is_empty() {
                    groups.push(std::mem::take(&mut current));
                }
            }
            Role::User => {
                current.extend(message.content.iter().filter_map(|content| {
                    let Content::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } = content
                    else {
                        return None;
                    };
                    if content.has_images() {
                        return None;
                    }
                    let content = content.text();
                    (!content.is_empty() && !content.starts_with(PERSISTED_OUTPUT_TAG)).then(|| {
                        Candidate {
                            tool_use_id,
                            size: utf16_length(&content),
                            content,
                        }
                    })
                }));
            }
            Role::System => {}
        }
    }
    if !current.is_empty() {
        groups.push(current);
    }
    groups
}

/// Claude Code's `enforceToolResultBudget`: replaces tool results in the request messages in place,
/// updates the state, and returns the replacement decisions newly made this time (reapplied old
/// replacements are not returned).
pub(super) fn enforce_tool_result_budget(
    messages: &mut [Message],
    state: &mut ContentReplacementState,
    chat_directory: &PrivateDirectory,
) -> Vec<ContentReplacementRecord> {
    let never_persisted = messages
        .iter()
        .filter(|message| message.role == Role::Assistant)
        .flat_map(|message| &message.content)
        .filter_map(|content| match content {
            Content::ToolUse { id, name, .. } if persistence_threshold(name).is_none() => {
                Some(id.clone())
            }
            _ => None,
        })
        .collect::<HashSet<_>>();
    let mut newly_replaced = Vec::new();
    for group in candidate_groups(messages) {
        let is_replaced = |candidate: &Candidate| {
            state
                .replacement_for(candidate.tool_use_id, &candidate.content)
                .is_some()
        };
        let fresh = group
            .iter()
            .filter(|candidate| {
                !is_replaced(candidate) && !state.seen.contains(candidate.tool_use_id)
            })
            .collect::<Vec<_>>();
        if fresh.is_empty() {
            continue;
        }
        let eligible = fresh
            .iter()
            .filter(|candidate| !never_persisted.contains(candidate.tool_use_id))
            .collect::<Vec<_>>();
        let frozen_size = group
            .iter()
            .filter(|candidate| {
                state.seen.contains(candidate.tool_use_id) && !is_replaced(candidate)
            })
            .map(|candidate| candidate.size)
            .sum::<usize>();
        let fresh_size = eligible
            .iter()
            .map(|candidate| candidate.size)
            .sum::<usize>();
        let mut selected = Vec::new();
        if frozen_size + fresh_size > MAX_TOOL_RESULTS_PER_MESSAGE_CHARS {
            let mut largest_first = eligible.clone();
            largest_first.sort_by_key(|candidate| std::cmp::Reverse(candidate.size));
            let mut remaining = frozen_size + fresh_size;
            for candidate in largest_first {
                if remaining <= MAX_TOOL_RESULTS_PER_MESSAGE_CHARS {
                    break;
                }
                selected.push(*candidate);
                remaining -= candidate.size;
            }
        }
        for candidate in &group {
            state.seen.insert(candidate.tool_use_id.to_owned());
        }
        for candidate in selected {
            let Ok(persisted) =
                persist_tool_result(chat_directory, candidate.tool_use_id, &candidate.content)
            else {
                continue;
            };
            state.replacements.insert(
                candidate.tool_use_id.to_owned(),
                persisted.replacement.clone(),
            );
            newly_replaced.push(ContentReplacementRecord::ToolResult {
                tool_use_id: candidate.tool_use_id.to_owned(),
                replacement: persisted.replacement,
            });
        }
    }

    apply_tool_result_replacements(messages, state);
    newly_replaced
}

/// Puts each result's recorded replacement in its place, replacing nothing new.
pub(super) fn apply_tool_result_replacements(
    messages: &mut [Message],
    state: &ContentReplacementState,
) {
    if state.replacements.is_empty() {
        return;
    }
    for content in messages
        .iter_mut()
        .filter(|message| message.role == Role::User)
        .flat_map(|message| message.content.iter_mut())
    {
        if let Content::ToolResult {
            tool_use_id,
            content,
            ..
        } = content
            && !content.has_images()
            && let text = content.text()
            && !text.starts_with(PERSISTED_OUTPUT_TAG)
            && let Some(replacement) = state.replacement_for(tool_use_id, &text)
        {
            *content = ToolResultContent::Text(replacement.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tickloop_harness_tools::{format_file_size, generate_preview};

    #[test]
    fn file_sizes_and_previews_follow_claude_code() {
        assert_eq!(format_file_size(512), "512 bytes");
        assert_eq!(format_file_size(2000), "2KB");
        assert_eq!(format_file_size(2048), "2KB");
        assert_eq!(format_file_size(52_736), "51.5KB");
        assert_eq!(format_file_size(3 * 1024 * 1024), "3MB");

        let short = "a".repeat(2000);
        assert_eq!(generate_preview(&short, 2000), (short.as_str(), false));
        let late_newline = format!("{}\n{}", "a".repeat(1500), "b".repeat(1000));
        assert_eq!(
            generate_preview(&late_newline, 2000),
            ("a".repeat(1500).as_str(), true)
        );
        let early_newline = format!("{}\n{}", "a".repeat(900), "b".repeat(2000));
        let (preview, has_more) = generate_preview(&early_newline, 2000);
        assert!(has_more);
        assert_eq!(utf16_length(preview), 2000);
        let wide = "😀".repeat(1500);
        assert_eq!(utf16_length(generate_preview(&wide, 2000).0), 2000);
    }

    #[test]
    fn large_results_are_persisted_under_a_unique_name_for_repeated_or_unsafe_ids() {
        let directory = tempfile::tempdir().unwrap();
        let chat_directory = PrivateDirectory::trusted(directory.path().to_path_buf());
        let content = "x".repeat(30_001);
        assert!(persist_large_tool_result(&chat_directory, "Grep", "call-1", "short").is_none());
        assert!(persist_large_tool_result(&chat_directory, "Read", "call-1", &content).is_none());
        let persisted =
            persist_large_tool_result(&chat_directory, BASH_TOOL_NAME, "call-1", &content).unwrap();
        assert_eq!(
            persisted.path,
            directory.path().join("tool-results/call-1.txt")
        );
        assert_eq!(std::fs::read_to_string(&persisted.path).unwrap(), content);
        assert!(
            persisted.replacement.starts_with(
                "<persisted-output>\nOutput too large (29.3KB). Full output saved to: "
            )
        );
        assert!(is_replacement_for(&persisted.replacement, &content));
        let again = persist_large_tool_result(&chat_directory, BASH_TOOL_NAME, "call-1", &content);
        assert_eq!(again.unwrap().replacement, persisted.replacement);

        let different = "y".repeat(30_001);
        let repeated =
            persist_large_tool_result(&chat_directory, BASH_TOOL_NAME, "call-1", &different)
                .unwrap();
        let repeated_name = repeated.path.file_name().unwrap().to_string_lossy();
        assert!(repeated_name.starts_with("call-1-"), "{repeated_name}");
        assert_eq!(std::fs::read_to_string(&repeated.path).unwrap(), different);
        assert!(!is_replacement_for(&persisted.replacement, &different));

        let unsafe_id =
            persist_large_tool_result(&chat_directory, BASH_TOOL_NAME, "../escape", &content)
                .unwrap();
        assert_eq!(
            unsafe_id.path.parent().unwrap(),
            directory.path().join("tool-results")
        );
        assert!(
            unsafe_id
                .path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("___escape-")
        );
        assert_eq!(std::fs::read_to_string(&unsafe_id.path).unwrap(), content);
    }

    fn tool_round(id: &str, content: &str) -> [Message; 2] {
        [
            Message {
                role: Role::Assistant,
                content: vec![Content::ToolUse {
                    id: id.into(),
                    name: "Glob".into(),
                    input: serde_json::json!({}),
                }],
            },
            Message {
                role: Role::User,
                content: vec![Content::ToolResult {
                    tool_use_id: id.into(),
                    content: content.into(),
                    is_error: false,
                }],
            },
        ]
    }

    fn tool_result_contents(messages: &[Message]) -> Vec<String> {
        messages
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(|content| match content {
                Content::ToolResult { content, .. } => Some(content.text().into_owned()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_replacement_is_never_applied_to_a_different_result_with_the_same_id() {
        let directory = tempfile::tempdir().unwrap();
        let chat_directory = PrivateDirectory::trusted(directory.path().to_path_buf());
        let original = "a".repeat(MAX_TOOL_RESULTS_PER_MESSAGE_CHARS + 1);
        let mut state = ContentReplacementState::default();
        let mut first = tool_round("reused", &original).to_vec();
        let records = enforce_tool_result_budget(&mut first, &mut state, &chat_directory);
        assert_eq!(records.len(), 1);
        assert!(tool_result_contents(&first)[0].starts_with(PERSISTED_OUTPUT_TAG));

        // The same id was reused by a new result after a rollback: the old replacement no longer
        // applies and is not adopted on rebuild either.
        let reused = "b".repeat(1000);
        let mut second = tool_round("reused", &reused).to_vec();
        assert!(enforce_tool_result_budget(&mut second, &mut state, &chat_directory).is_empty());
        assert_eq!(tool_result_contents(&second), [reused]);

        let rebuilt = ContentReplacementState::reconstruct_from_messages(&second, &records);
        assert!(rebuilt.replacements.is_empty());
        assert!(rebuilt.seen.contains("reused"));
        let rebuilt_original = ContentReplacementState::reconstruct_from_messages(
            &tool_round("reused", &original),
            &records,
        );
        assert_eq!(rebuilt_original.records(), records);
    }

    #[test]
    fn results_with_images_are_never_persisted_by_the_message_budget() {
        let directory = tempfile::tempdir().unwrap();
        let chat_directory = PrivateDirectory::trusted(directory.path().to_path_buf());
        let text = "a".repeat(MAX_TOOL_RESULTS_PER_MESSAGE_CHARS + 1);
        let mut messages = tool_round("with-image", "").to_vec();
        let image = tickloop_harness_protocol::ToolResultBlock::Image {
            source: tickloop_harness_protocol::ImageSource::Base64 {
                media_type: tickloop_harness_protocol::ImageMediaType::Png,
                data: "iVBORw0KGgo=".into(),
            },
            width: 8,
            height: 8,
        };
        let content = ToolResultContent::Blocks(vec![
            tickloop_harness_protocol::ToolResultBlock::Text { text: text.clone() },
            image,
        ]);
        let Content::ToolResult {
            content: result, ..
        } = &mut messages[1].content[0]
        else {
            unreachable!();
        };
        *result = content.clone();
        let mut state = ContentReplacementState::default();

        assert!(enforce_tool_result_budget(&mut messages, &mut state, &chat_directory).is_empty());

        assert!(matches!(
            &messages[1].content[0],
            Content::ToolResult { content: kept, .. } if *kept == content
        ));
        assert!(!directory.path().join(TOOL_RESULTS_SUBDIRECTORY).exists());
    }

    #[test]
    fn a_large_result_without_a_folder_is_cut_at_its_threshold_with_a_note() {
        let threshold = persistence_threshold("WebFetch").unwrap();
        assert!(truncate_large_tool_result("WebFetch", &"a".repeat(threshold)).is_none());
        let content = format!("{}{}", "a".repeat(threshold), "b".repeat(threshold));
        let truncated = truncate_large_tool_result("WebFetch", &content).unwrap();
        let (kept, note) = truncated.split_once("\n\n[Output truncated").unwrap();
        assert_eq!(kept, "a".repeat(threshold));
        assert!(note.contains(&format!("has {} characters", 2 * threshold)));
        assert!(note.contains("no Folder"));
        assert!(truncate_large_tool_result(READ_TOOL_NAME, &content).is_none());
    }

    #[test]
    fn a_round_over_the_message_budget_without_a_folder_has_its_largest_results_cut_in_place() {
        let small = "s".repeat(1_000);
        let large = "l".repeat(MAX_TOOL_RESULTS_PER_MESSAGE_CHARS);
        let round = |results: &[(&str, &str)]| {
            vec![
                Message {
                    role: Role::Assistant,
                    content: results
                        .iter()
                        .map(|(id, _)| Content::ToolUse {
                            id: (*id).into(),
                            name: "WebFetch".into(),
                            input: serde_json::json!({}),
                        })
                        .collect(),
                },
                Message {
                    role: Role::User,
                    content: results
                        .iter()
                        .map(|(id, text)| Content::ToolResult {
                            tool_use_id: (*id).into(),
                            content: ToolResultContent::Text((*text).into()),
                            is_error: false,
                        })
                        .collect(),
                },
            ]
        };
        let mut within = round(&[("a", &small), ("b", &small)]);
        let unchanged = within.clone();
        cut_tool_results_over_budget(&mut within);
        assert_eq!(within, unchanged);

        let mut over = round(&[("a", &small), ("b", &large), ("c", &large)]);
        cut_tool_results_over_budget(&mut over);
        let mut again = round(&[("a", &small), ("b", &large), ("c", &large)]);
        cut_tool_results_over_budget(&mut again);
        assert_eq!(over, again);
        let texts = over[1]
            .content
            .iter()
            .map(|content| match content {
                Content::ToolResult { content, .. } => content.text().into_owned(),
                _ => unreachable!(),
            })
            .collect::<Vec<_>>();
        assert_eq!(texts[0], small);
        let share = MAX_TOOL_RESULTS_PER_MESSAGE_CHARS / 3;
        for cut in &texts[1..] {
            assert!(
                cut.starts_with(&"l".repeat(share)) && !cut.starts_with(&"l".repeat(share + 1))
            );
            assert!(cut.contains("no Folder"));
        }
    }
}

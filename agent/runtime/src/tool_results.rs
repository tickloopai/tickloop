//! 大工具结果外存，照 Claude Code `utils/toolResultStorage.ts`。
//!
//! 两种机制与 Claude Code 相同：
//! - **单个结果**：工具结果写入对话前，超过该工具阈值的内容写入
//!   `<Chat 目录>/tool-results/<toolUseId>.txt`，对话中保存带路径和预览的替换文本。
//! - **单条消息预算**：发请求前，同一条 user 消息中的工具结果合计超过预算时，把最大的、
//!   模型从未见过的结果外存并替换。替换决定作为内容替换条目写入 Transcript，恢复时重建，
//!   保证之后每次请求逐字节一致；已见过但未替换的结果永远不再替换。
//!
//! 预算与外存都按工具结果的文字（[`ToolResultContent::text`]）计算，因此文件扩展名总是
//! `.txt`（Claude Code 内容块数组写 `.json`）。长度照 Claude Code 按 UTF-16 码元计算。
//!
//! 调用 id 由模型提供，可能含文件名不允许的字符，也可能跨 Run 重复（请求中重复的 id 投影为
//! 唯一的 `tl_` id）。因此预算状态按请求中的投影 id 键控，恢复时同样按投影重建；替换只套用到
//! 与之生成时内容一致的结果上。

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use sha2::{Digest, Sha256};

use super::{Agent, AgentError};
use crate::transcript::{ContentReplacementRecord, private_family_temp_directory};
use tickloop_agent_base::private_fs::{PRIVATE_FILE_MODE, PrivateDirectory};
use tickloop_agent_protocol::projection;
use tickloop_agent_protocol::{
    ChatSnapshot, Content, Message, Receipt, Role, ToolResult, ToolResultContent,
};
use tickloop_agent_tools::{
    BASH_TOOL_NAME, GREP_TOOL_NAME, PERSISTED_OUTPUT_TAG, PREVIEW_LABEL, READ_TOOL_NAME,
    SAVED_TO_LABEL, TASK_OUTPUT_SUBDIRECTORY, Tools, large_output_message, utf16_length,
};

/// Chat 目录中大工具结果的子目录。
const TOOL_RESULTS_SUBDIRECTORY: &str = "tool-results";
/// Claude Code `DEFAULT_MAX_RESULT_SIZE_CHARS`：各工具声明的上限再受此值约束。
const DEFAULT_MAX_RESULT_SIZE_CHARS: usize = 50_000;
/// Claude Code `MAX_TOOL_RESULTS_PER_MESSAGE_CHARS`。
const MAX_TOOL_RESULTS_PER_MESSAGE_CHARS: usize = 200_000;
/// Claude Code 大多数工具声明的 `maxResultSizeChars`。
const COMMON_MAX_RESULT_SIZE_CHARS: usize = 100_000;
const MAX_TOOL_USE_ID_LENGTH: usize = 200;
/// 调用 id 不能直接作文件名或已被不同内容占用时，改用"清洗后的 id-内容哈希"命名。
const FALLBACK_STEM_LENGTH: usize = 64;
const CONTENT_DIGEST_HEX_LENGTH: usize = 16;
const FALLBACK_NAME_ATTEMPTS: usize = 8;
const FALLBACK_STEM: &str = "tool-result";

impl Agent {
    /// Each family owns one private temporary root; children share it without widening
    /// access to other families in the same project.
    pub(super) fn create_chat_tools(&self, snapshot: &ChatSnapshot) -> Arc<Tools> {
        let chat_id = snapshot.id;
        let family_id = snapshot
            .delegation
            .as_ref()
            .map_or(chat_id, |delegation| delegation.parent_chat_id);
        let family_temp = private_family_temp_directory(family_id);
        let persistent_project = self
            .inner
            .recorder
            .as_ref()
            .and_then(|recorder| recorder.project_directory_of(chat_id));
        let temporary_chat = if family_id == chat_id {
            family_temp.clone()
        } else {
            family_temp.join(chat_id.to_string())
        };
        let chat_directory = persistent_project.map_or_else(
            || temporary_chat.clone(),
            |project| PrivateDirectory::trusted(project).join(chat_id.to_string()),
        );
        let task_output_directory = temporary_chat.join(TASK_OUTPUT_SUBDIRECTORY);
        Arc::new(Tools::for_chat(
            chat_directory,
            task_output_directory,
            family_temp,
            self.inner.shells.for_chat(chat_id),
        ))
    }

    /// 工具结果写入对话前按阈值外存；外存后结果内容换成替换文本，未自带输出引用的结果
    /// 指向外存文件。
    pub(super) async fn persist_large_tool_result(
        &self,
        receipt: &Receipt,
        tool_name: &str,
        tool_use_id: &str,
        mut result: ToolResult,
    ) -> Result<ToolResult, AgentError> {
        let chat_directory = self
            .inner
            .chats
            .lock()
            .await
            .get(&receipt.chat_id)
            .ok_or(AgentError::NotFound)?
            .tools
            .chat_directory()
            .clone();
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

/// 工具的外存阈值；`None` 表示从不外存（Claude Code 的 Read 声明为 Infinity：
/// 把读文件的结果外存成文件再让模型读回是循环的）。
fn persistence_threshold(tool_name: &str) -> Option<usize> {
    let declared = match tool_name {
        READ_TOOL_NAME => return None,
        GREP_TOOL_NAME => 20_000,
        BASH_TOOL_NAME => 30_000,
        _ => COMMON_MAX_RESULT_SIZE_CHARS,
    };
    Some(declared.min(DEFAULT_MAX_RESULT_SIZE_CHARS))
}

pub(super) struct PersistedToolResult {
    pub path: PathBuf,
    pub replacement: String,
}

/// 照 Claude Code `maybePersistLargeToolResult`：超过阈值时外存并返回替换文本；
/// 未超过阈值或写入失败时返回 `None`，调用方保留原内容。
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

/// 以不覆盖的方式写入。调用 id 可以直接作文件名时照 Claude Code 写 `<toolUseId>.txt`；
/// 已有文件内容相同时沿用（Claude Code 把 `EEXIST` 一律视为已写入）。id 含非法字符或
/// 文件已被不同内容占用时改写 `<清洗后的 id>-<内容哈希>.txt`，不放弃外存。
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

/// 替换文本是否由这份内容生成：跨 Run 重复或回退后重用的 id 不会套用别的结果的替换。
fn is_replacement_for(replacement: &str, content: &str) -> bool {
    replacement
        .strip_prefix(PERSISTED_OUTPUT_TAG)
        .and_then(|rest| rest.split_once(SAVED_TO_LABEL))
        .and_then(|(_, rest)| rest.split_once(PREVIEW_LABEL))
        .is_some_and(|(path, _)| {
            build_large_tool_result_message(Path::new(path), content) == replacement
        })
}

/// Claude Code `ContentReplacementState`：只存在于运行期，恢复时由内容替换条目重建。
#[derive(Clone, Debug, Default)]
pub(super) struct ContentReplacementState {
    /// 已经过预算检查的结果；之后永远不会再被新替换。
    seen: HashSet<String>,
    /// 已替换的结果及模型看到的原文。
    replacements: HashMap<String, String>,
}

impl ContentReplacementState {
    /// Claude Code `reconstructContentReplacementState`：对话中出现的候选结果都视为已见过；
    /// 只采用对话中仍存在、且由该结果内容生成的替换记录。候选取自发给模型的投影对话，
    /// 与请求时的键一致。
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

    fn replacement_for(&self, tool_use_id: &str, content: &str) -> Option<&String> {
        self.replacements
            .get(tool_use_id)
            .filter(|replacement| is_replacement_for(replacement, content))
    }

    /// 仍适用于快照中模型可见对话（最后一个压缩边界之后）的替换决定。压缩写出边界时以此在
    /// 边界之后重新记录：超过 5MB 的 Transcript 加载时跳过边界之前的记录。
    pub(super) fn records_for(&self, snapshot: &ChatSnapshot) -> Vec<ContentReplacementRecord> {
        Self::reconstruct(snapshot, &self.records()).records()
    }

    /// 以记录形式导出替换决定，供 `--fork-session` 与 Branch 以新 chatId 重新记录。
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

/// Claude Code `collectCandidatesByMessage`：以 assistant 消息为界，把相邻 user 消息中的
/// 工具结果归为一组（请求中它们合并成一条 user 消息）。已替换、为空或含图片的结果不是候选
/// （Claude Code `hasImageBlock`：外存只保存文字，替换会丢掉图片）。
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

/// Claude Code `enforceToolResultBudget`：就地替换请求消息中的工具结果，更新状态，
/// 返回本次新做出的替换决定（重新套用的旧替换不返回）。
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

    if !state.replacements.is_empty() {
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
    newly_replaced
}

#[cfg(test)]
mod tests {
    use super::*;
    use tickloop_agent_tools::{format_file_size, generate_preview};

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

        // 回退后同一 id 被新结果重用：旧的替换不再套用，重建时也不采用。
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
        let image = tickloop_agent_protocol::ToolResultBlock::Image {
            source: tickloop_agent_protocol::ImageSource::Base64 {
                media_type: tickloop_agent_protocol::ImageMediaType::Png,
                data: "iVBORw0KGgo=".into(),
            },
            width: 8,
            height: 8,
        };
        let content = ToolResultContent::Blocks(vec![
            tickloop_agent_protocol::ToolResultBlock::Text { text: text.clone() },
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
}

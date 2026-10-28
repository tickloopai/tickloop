use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use tickloop_harness_protocol::Content;
use tickloop_harness_transcript::{ForkedFrom, Line};
use uuid::Uuid;

use super::path::TRANSCRIPT_FILE_SUFFIX;

/// Following Claude Code's `LITE_READ_BUF_SIZE`: listing reads only 64KB from the head and tail of
/// each file.
pub(super) const LITE_READ_BUF_SIZE: usize = 64 * 1024;
const FORKED_FROM_PATTERN: &str = "\"forkedFrom\":{";
/// A Subagent's Transcript opens with its delegation; the line can outgrow the listing's head read.
const DELEGATION_LINE_PREFIX: &str = "{\"type\":\"delegation\"";

#[derive(Clone, Debug)]
pub struct TranscriptSummary {
    pub chat_id: Uuid,
    pub modified: SystemTime,
    pub cwd: Option<PathBuf>,
    pub title: Option<String>,
    pub first_prompt: Option<String>,
    pub last_prompt: Option<String>,
    pub git_branch: Option<String>,
    pub forked_from: Option<ForkedFrom>,
}

/// Lists the Transcripts of Chats of their own in the project directory, newest modification time
/// first; empty when the directory does not exist. A Subagent's Transcript belongs to its family,
/// which opens with the root, so it is not listed.
pub(crate) fn list_project(project_directory: &Path) -> io::Result<Vec<TranscriptSummary>> {
    let entries = match std::fs::read_dir(project_directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut summaries = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let Some(chat_id) = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(TRANSCRIPT_FILE_SUFFIX))
            .and_then(|stem| Uuid::parse_str(stem).ok())
        else {
            continue;
        };
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let Ok((head, tail)) = read_head_and_tail(&path, metadata.len()) else {
            continue;
        };
        if head.starts_with(DELEGATION_LINE_PREFIX) {
            continue;
        }
        summaries.push(TranscriptSummary {
            chat_id,
            modified: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            cwd: first_json_string_field(&head, "cwd").map(PathBuf::from),
            title: last_json_string_field(&tail, "customTitle")
                .or_else(|| last_json_string_field(&head, "customTitle"))
                .or_else(|| last_json_string_field(&tail, "aiTitle"))
                .or_else(|| last_json_string_field(&head, "aiTitle")),
            first_prompt: first_prompt(&head),
            last_prompt: last_json_string_field(&tail, "lastPrompt"),
            git_branch: last_json_string_field(&tail, "gitBranch")
                .or_else(|| first_json_string_field(&head, "gitBranch")),
            forked_from: forked_from(&tail).or_else(|| forked_from(&head)),
        });
    }
    summaries.sort_by_key(|summary| std::cmp::Reverse(summary.modified));
    Ok(summaries)
}

/// The root Chat of the family whose Subagent wrote the Transcript at `path`; `None` for a Chat of
/// its own.
pub(super) fn family_root(path: &Path) -> io::Result<Option<Uuid>> {
    let mut first_line = String::new();
    BufReader::new(std::fs::File::open(path)?).read_line(&mut first_line)?;
    if !first_line.starts_with(DELEGATION_LINE_PREFIX) {
        return Ok(None);
    }
    match serde_json::from_str::<Line>(&first_line)? {
        Line::Delegation(entry) => Ok(Some(entry.delegation.root_chat_id().uuid())),
        _ => Ok(None),
    }
}

fn read_head_and_tail(path: &Path, length: u64) -> io::Result<(String, String)> {
    let mut file = std::fs::File::open(path)?;
    let mut head = vec![0; LITE_READ_BUF_SIZE.min(length as usize)];
    file.read_exact(&mut head)?;
    let tail = if length as usize <= LITE_READ_BUF_SIZE {
        head.clone()
    } else {
        file.seek(SeekFrom::Start(length - LITE_READ_BUF_SIZE as u64))?;
        let mut tail = Vec::with_capacity(LITE_READ_BUF_SIZE);
        file.read_to_end(&mut tail)?;
        tail
    };
    Ok((
        String::from_utf8_lossy(&head).into_owned(),
        String::from_utf8_lossy(&tail).into_owned(),
    ))
}

/// First complete non-meta user text entry in the head; only candidate lines are fully parsed.
fn first_prompt(head: &str) -> Option<String> {
    let complete = head.rfind('\n').map_or("", |end| &head[..end]);
    complete
        .lines()
        .filter(|line| line.starts_with("{\"type\":\"user\"") && !line.contains("\"isMeta\":true"))
        .find_map(|line| match serde_json::from_str::<Line>(line).ok()? {
            Line::User(entry) if entry.agent_message_origin.is_none() => {
                let texts = entry
                    .message
                    .content
                    .iter()
                    .map(|content| match content {
                        Content::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Option<Vec<_>>>()?;
                let text = texts.join("\n");
                (!text.trim().is_empty()).then_some(text)
            }
            _ => None,
        })
}

fn forked_from(text: &str) -> Option<ForkedFrom> {
    let start = text.rfind(FORKED_FROM_PATTERN)? + FORKED_FROM_PATTERN.len() - 1;
    let end = text[start..].find('}')? + start + 1;
    serde_json::from_str(&text[start..end]).ok()
}

pub(crate) fn last_json_string_field(text: &str, key: &str) -> Option<String> {
    let pattern = format!("\"{key}\":\"");
    let mut search_end = text.len();
    while let Some(start) = text[..search_end].rfind(&pattern) {
        if let Some(value) = json_string_at(&text[start + pattern.len()..]) {
            return Some(value);
        }
        search_end = start;
    }
    None
}

fn first_json_string_field(text: &str, key: &str) -> Option<String> {
    let pattern = format!("\"{key}\":\"");
    let mut offset = 0;
    while let Some(found) = text[offset..].find(&pattern) {
        let value_start = offset + found + pattern.len();
        if let Some(value) = json_string_at(&text[value_start..]) {
            return Some(value);
        }
        offset = value_start;
    }
    None
}

/// Reads the JSON string value right after an opening quote; returns `None` on truncation or an
/// invalid escape.
fn json_string_at(text: &str) -> Option<String> {
    let mut escaped = false;
    for (index, character) in text.char_indices() {
        match character {
            '\\' if !escaped => escaped = true,
            '"' if !escaped => {
                return serde_json::from_str(&format!("\"{}\"", &text[..index])).ok();
            }
            '\n' => return None,
            _ => escaped = false,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn string_fields_are_extracted_with_escapes_and_truncation() {
        let text = "{\"customTitle\":\"first\"}\n{\"customTitle\":\"say \\\"hi\\\"\"}\n{\"customTitle\":\"trunc";
        assert_eq!(
            last_json_string_field(text, "customTitle").as_deref(),
            Some("say \"hi\"")
        );
        assert_eq!(
            first_json_string_field(text, "customTitle").as_deref(),
            Some("first")
        );
        let origin = format!(
            "{{\"type\":\"branch-origin\",\"forkedFrom\":{{\"chatId\":\"{}\",\"messageUuid\":null}}}}",
            Uuid::nil()
        );
        assert_eq!(forked_from(&origin).unwrap().chat_id, Uuid::nil());
    }
}

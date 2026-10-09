use std::{path::PathBuf, time::SystemTime};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
    pub output_ref: Option<PathBuf>,
}

/// What a Chat last saw of one file, following Claude Code's `readFileState` entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileReadRecord {
    /// The returned lines joined by `\n`, with `\r\n` normalized and without line numbers.
    pub content: String,
    /// The file's modification time when it was read.
    pub modified: SystemTime,
    /// The first returned line, counted from 1.
    pub offset: usize,
    /// The line limit the read asked for; `None` when it relied on the default.
    pub limit: Option<usize>,
    /// Whether `content` covers the whole file.
    pub is_full_read: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum CommandState {
    Running,
    Background,
    Cancelling,
    Completed,
    Failed,
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommandSnapshot {
    pub operation_id: Uuid,
    pub loop_id: Uuid,
    pub command: String,
    /// What the model said the command does; task notifications name the command by it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub cwd: PathBuf,
    pub pid: Option<u32>,
    pub state: CommandState,
    #[serde(default)]
    pub was_background: bool,
    /// Whether the foreground timeout, rather than the user, moved the command to the background.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub backgrounded_on_timeout: bool,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub reason: Option<String>,
    pub output_ref: PathBuf,
    pub output_bytes: u64,
    #[serde(default)]
    pub updated_at_ms: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SearchConfig {
    pub endpoint: String,
    pub api_key_env: Option<String>,
}

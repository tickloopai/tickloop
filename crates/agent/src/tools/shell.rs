use std::{
    collections::HashMap,
    fs::OpenOptions,
    io::{Read, Seek, SeekFrom},
    os::{
        fd::AsRawFd,
        unix::{
            fs::OpenOptionsExt,
            process::{CommandExt, ExitStatusExt},
        },
    },
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Mutex, broadcast, watch};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{
    DEFAULT_OUTPUT_BYTES, Operation, ToolDefinition, ToolError, ToolResult, io_error,
    open_existing, read::semantic_integer, write::semantic_boolean,
};
use crate::transcript::PrivateDirectory;

mod command_tables;
mod interpreter;
mod model_result;
mod path_constraints;
pub(super) use path_constraints::auto_edit_safety_check;
mod permission;
mod read_only;
mod security;
mod sed;
mod syntax;

pub const BASH_TOOL_NAME: &str = "Bash";
pub const TASK_STOP_TOOL_NAME: &str = "TaskStop";
/// Claude Code `getDefaultBashTimeoutMs`.
const DEFAULT_TIMEOUT_MS: u64 = 120_000;
/// Claude Code `getMaxBashTimeoutMs`.
const MAX_TIMEOUT_MS: u64 = 600_000;
const TERMINATION_GRACE: Duration = Duration::from_millis(200);
const PROCESS_CHECK_INTERVAL: Duration = Duration::from_millis(10);
const OUTPUT_POLL_INTERVAL: Duration = Duration::from_millis(40);
/// 单个任务输出文件上限。Claude Code 为 5GB；Tick Loop 按工单 34 的决定保留 64 MiB。
pub(crate) const MAX_COMMAND_OUTPUT_BYTES: u64 = 64 * 1024 * 1024;
/// 命令被停止或随 Agent 关闭而终止时记录的原因。
pub(crate) const CANCELLED_COMMAND_REASON: &str = "cancelled";
const TIMED_OUT_REASON: &str = "timed_out";
const OUTPUT_LIMIT_REASON: &str = "output_limit_exceeded";
/// Claude Code `DISALLOWED_AUTO_BACKGROUND_COMMANDS`: commands that still end at their foreground
/// timeout unless the model ran them with `run_in_background`.
const DISALLOWED_AUTO_BACKGROUND_COMMANDS: [&str; 1] = ["sleep"];

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ShellInput {
    command: String,
    #[serde(default, deserialize_with = "semantic_integer")]
    timeout: Option<usize>,
    description: Option<String>,
    #[serde(default, deserialize_with = "semantic_boolean")]
    run_in_background: bool,
    cwd: Option<String>,
    /// Internal directories whose files shell commands may read without approval.
    #[serde(skip)]
    pub(super) readable_directories: Vec<PathBuf>,
}

pub(super) struct UserShellExecution {
    pub(super) command: String,
    pub(super) operation_id: Uuid,
    pub(super) chat_id: Uuid,
    pub(super) background: bool,
}

impl ShellInput {
    /// Claude Code's rule-free read-only admission, evaluated with default-mode path checks.
    pub(super) fn is_read_only(&self, cwd: &Path, workspace: &super::Workspace) -> bool {
        permission::is_allowed_without_rules(
            &self.command,
            &self.permission_context(cwd, workspace),
            false,
        )
    }

    /// Claude Code's rule-free Accept Edits admission for the same command.
    pub(super) fn is_allowed_in_accept_edits(
        &self,
        cwd: &Path,
        workspace: &super::Workspace,
    ) -> bool {
        permission::is_allowed_without_rules(
            &self.command,
            &self.permission_context(cwd, workspace),
            true,
        )
    }

    fn permission_context<'a>(
        &'a self,
        cwd: &'a Path,
        workspace: &super::Workspace,
    ) -> permission::ShellPermissionContext<'a> {
        permission::ShellPermissionContext {
            cwd,
            working_directories: std::iter::once(&workspace.workdir)
                .chain(&workspace.additional_directories)
                .cloned()
                .collect(),
            readable_directories: &self.readable_directories,
        }
    }

    /// How long the command may run in the foreground: Claude Code treats a missing or zero
    /// timeout as the default, and the documented maximum caps longer requests.
    fn foreground_timeout(&self) -> Duration {
        let milliseconds = self
            .timeout
            .filter(|timeout| *timeout > 0)
            .map_or(DEFAULT_TIMEOUT_MS, |timeout| timeout as u64);
        Duration::from_millis(milliseconds.min(MAX_TIMEOUT_MS))
    }
}

/// Claude Code `isAutobackgroundingAllowed`: the first word of the first subcommand decides
/// whether a command that outlives its foreground timeout moves to the background.
fn allows_auto_background(command: &str) -> bool {
    let first_command = syntax::parse(command)
        .and_then(|commands| commands.first().map(|command| command.text.clone()))
        .unwrap_or_else(|| {
            command
                .split(['|', '&', ';', '\n'])
                .next()
                .unwrap_or(command)
                .to_owned()
        });
    let base_command = first_command.split_whitespace().next().unwrap_or_default();
    !DISALLOWED_AUTO_BACKGROUND_COMMANDS.contains(&base_command)
}

const BASH_DESCRIPTION_PARAMETER: &str = r#"Clear, concise description of what this command does in active voice. Never use words like "complex" or "risk" in the description - just describe what it does.

For simple commands (git, npm, standard CLI tools), keep it brief (5-10 words):
- ls → "List files in current directory"
- git status → "Show working tree status"
- npm install → "Install package dependencies"

For commands that are harder to parse at a glance (piped commands, obscure flags, etc.), add enough context to clarify what it does:
- find . -name "*.tmp" -exec rm {} \; → "Find and delete all .tmp files recursively"
- git reset --hard origin/main → "Discard all local changes and match remote main"
- curl -s url | jq '.data[]' → "Fetch JSON from URL and extract data array elements""#;

/// Adapted from Claude Code `BashTool/prompt.ts` `getSimplePrompt`, without the git commit, pull
/// request and sandbox sections. Unlike Claude Code, the working directory does not carry over
/// between commands.
fn bash_description() -> String {
    use super::{EDIT_TOOL_NAME, GLOB_TOOL_NAME, GREP_TOOL_NAME, READ_TOOL_NAME, WRITE_TOOL_NAME};
    let default_minutes = DEFAULT_TIMEOUT_MS / 60_000;
    let max_minutes = MAX_TIMEOUT_MS / 60_000;
    format!(
        r#"Executes a given bash command and returns its output.

Each command starts in this Chat's working directory, or in `cwd` when given, and runs with bash or zsh; the working directory and other shell state do not persist between commands. Read-only commands inside the workspace run without approval; other commands may ask the user for permission.

IMPORTANT: Avoid using this tool to run `find`, `grep`, `cat`, `head`, `tail`, `sed`, `awk`, or `echo` commands, unless explicitly instructed or after you have verified that a dedicated tool cannot accomplish your task. Instead, use the appropriate dedicated tool as this will provide a much better experience for the user:

 - File search: Use {GLOB_TOOL_NAME} (NOT find or ls)
 - Content search: Use {GREP_TOOL_NAME} (NOT grep or rg)
 - Read files: Use {READ_TOOL_NAME} (NOT cat/head/tail)
 - Edit files: Use {EDIT_TOOL_NAME} (NOT sed/awk)
 - Write files: Use {WRITE_TOOL_NAME} (NOT echo >/cat <<EOF)
 - Communication: Output text directly (NOT echo/printf)
While the {BASH_TOOL_NAME} tool can do similar things, it’s better to use the built-in tools as they provide a better user experience and make it easier to review tool calls and give permission.

# Instructions
 - If your command will create new directories or files, first use this tool to run `ls` to verify the parent directory exists and is the correct location.
 - Always quote file paths that contain spaces with double quotes in your command (e.g., cd "path with spaces/file.txt")
 - Use absolute paths or the `cwd` parameter instead of `cd`; a `cd` does not carry over to the next command.
 - You may specify an optional timeout in milliseconds (up to {MAX_TIMEOUT_MS}ms / {max_minutes} minutes). By default, your command will timeout after {DEFAULT_TIMEOUT_MS}ms ({default_minutes} minutes).
 - You can use the `run_in_background` parameter to run the command in the background. Only use this if you don't need the result immediately and are OK being notified when the command completes later. You do not need to check the output right away - you'll be notified when it finishes. You do not need to use '&' at the end of the command when using this parameter. The result names the task ID and its output file: use {READ_TOOL_NAME} on that file to see the output so far and {TASK_STOP_TOOL_NAME} with the task ID to stop it.
 - When issuing multiple commands:
  - If the commands are independent, make multiple {BASH_TOOL_NAME} tool calls in a single message. Example: if you need to run "git status" and "git diff", send a single message with two {BASH_TOOL_NAME} tool calls.
  - If the commands depend on each other and must run sequentially, use a single {BASH_TOOL_NAME} call with '&&' to chain them together.
  - Use ';' only when you need to run commands sequentially but don't care if earlier commands fail.
  - DO NOT use newlines to separate commands (newlines are ok in quoted strings).
 - Avoid unnecessary `sleep` commands:
  - Do not sleep between commands that can run immediately — just run them.
  - If your command is long running and you would like to be notified when it finishes — use `run_in_background`. No sleep needed.
  - Do not retry failing commands in a sleep loop — diagnose the root cause.
  - If waiting for a background task you started with `run_in_background`, you will be notified when it completes — do not poll.
  - If you must poll an external process, use a check command (e.g. `gh run view`) rather than sleeping first.
  - If you must sleep, keep the duration short (1-5 seconds) to avoid blocking the user."#
    )
}

pub(super) fn definition() -> ToolDefinition {
    ToolDefinition {
        name: BASH_TOOL_NAME.into(),
        description: bash_description(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "The command to execute"},
                "timeout": {
                    "type": "number",
                    "description": format!("Optional timeout in milliseconds (max {MAX_TIMEOUT_MS})")
                },
                "description": {"type": "string", "description": BASH_DESCRIPTION_PARAMETER},
                "run_in_background": {
                    "type": "boolean",
                    "description": format!("Set to true to run this command in the background. Use {} to read the output later.", super::READ_TOOL_NAME)
                },
                "cwd": {
                    "type": "string",
                    "description": "Optional directory to run the command in, instead of this Chat's working directory"
                }
            },
            "required": ["command"],
            "additionalProperties": false
        }),
    }
}

pub(super) fn prepare_input(input: &Value) -> Result<(String, Operation, Value), ToolError> {
    let parsed: ShellInput = serde_json::from_value(input.clone())
        .map_err(|error| ToolError::InvalidInput(error.to_string()))?;
    if parsed.command.trim().is_empty() || parsed.command.contains('\0') {
        return Err(ToolError::InvalidInput(
            "command cannot be empty or contain NUL".into(),
        ));
    }
    let mut normalized_input = input.clone();
    if let Some(timeout) = normalized_input.get_mut("timeout") {
        *timeout = json!(parsed.timeout);
    }
    if let Some(run_in_background) = normalized_input.get_mut("run_in_background") {
        *run_in_background = json!(parsed.run_in_background);
    }
    Ok((
        parsed.cwd.clone().unwrap_or_else(|| ".".into()),
        Operation::Shell(parsed),
        normalized_input,
    ))
}

/// A `TaskStop` request for one of this Loop's commands.
#[derive(Clone, Debug)]
pub(super) struct TaskStopInput {
    pub(super) task_id: String,
}

pub(super) fn task_stop_definition() -> ToolDefinition {
    ToolDefinition {
        name: TASK_STOP_TOOL_NAME.into(),
        description: "
- Stops a running background task by its ID
- Takes a task_id parameter identifying the task to stop
- Returns a success or failure status
- Use this tool when you need to terminate a long-running task
"
        .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "task_id": {"type": "string", "description": "The ID of the background task to stop"},
                "shell_id": {"type": "string", "description": "Deprecated: use task_id instead"}
            },
            "additionalProperties": false
        }),
    }
}

/// Claude Code `TaskStopTool.validateInput`: `task_id`, or the deprecated `shell_id`, is required.
pub(super) fn prepare_task_stop(input: &Value) -> Result<(String, Operation, Value), ToolError> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Input {
        task_id: Option<String>,
        shell_id: Option<String>,
    }
    let parsed: Input = serde_json::from_value(input.clone())
        .map_err(|error| ToolError::InvalidInput(error.to_string()))?;
    let task_id = parsed
        .task_id
        .or(parsed.shell_id)
        .filter(|task_id| !task_id.is_empty())
        .ok_or_else(|| ToolError::Rejected("Missing required parameter: task_id".into()))?;
    Ok((
        ".".into(),
        Operation::TaskStop(TaskStopInput {
            task_id: task_id.clone(),
        }),
        json!({ "task_id": task_id }),
    ))
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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UserCommandSnapshot {
    pub operation_id: Uuid,
    pub chat_id: Uuid,
    pub command: String,
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

#[derive(Clone)]
enum ProcessSnapshot {
    Agent(CommandSnapshot),
    User(UserCommandSnapshot),
}

#[derive(Clone, Copy)]
enum CommandOwner {
    Agent { operation_id: Uuid, loop_id: Uuid },
    User { operation_id: Uuid, chat_id: Uuid },
}

#[derive(Clone, Copy)]
enum OwnerScope {
    Agent,
    User { chat_id: Uuid },
}

/// What moved a foreground command to the background.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BackgroundTrigger {
    User,
    Timeout,
}

impl ProcessSnapshot {
    fn state(&self) -> &CommandState {
        match self {
            Self::Agent(snapshot) => &snapshot.state,
            Self::User(snapshot) => &snapshot.state,
        }
    }

    fn set_state(&mut self, state: CommandState) {
        if self.state() != &state {
            self.mark_updated();
        }
        match self {
            Self::Agent(snapshot) => snapshot.state = state,
            Self::User(snapshot) => snapshot.state = state,
        }
    }

    fn cwd(&self) -> &Path {
        match self {
            Self::Agent(snapshot) => &snapshot.cwd,
            Self::User(snapshot) => &snapshot.cwd,
        }
    }

    fn output_ref(&self) -> &Path {
        match self {
            Self::Agent(snapshot) => &snapshot.output_ref,
            Self::User(snapshot) => &snapshot.output_ref,
        }
    }

    fn set_reason(&mut self, reason: String) {
        match self {
            Self::Agent(snapshot) => snapshot.reason = Some(reason),
            Self::User(snapshot) => snapshot.reason = Some(reason),
        }
    }

    fn mark_backgrounded(&mut self, trigger: BackgroundTrigger) {
        let on_timeout = trigger == BackgroundTrigger::Timeout;
        match self {
            Self::Agent(snapshot) => {
                snapshot.was_background = true;
                snapshot.backgrounded_on_timeout |= on_timeout;
            }
            Self::User(snapshot) => {
                snapshot.was_background = true;
                snapshot.backgrounded_on_timeout |= on_timeout;
            }
        }
    }

    fn merge_background_history(&mut self, authoritative: &Self) {
        match (self, authoritative) {
            (Self::Agent(snapshot), Self::Agent(authoritative)) => {
                snapshot.was_background |= authoritative.was_background;
                snapshot.backgrounded_on_timeout |= authoritative.backgrounded_on_timeout;
            }
            (Self::User(snapshot), Self::User(authoritative)) => {
                snapshot.was_background |= authoritative.was_background;
                snapshot.backgrounded_on_timeout |= authoritative.backgrounded_on_timeout;
            }
            _ => {}
        }
    }

    fn reason(&self) -> Option<&str> {
        match self {
            Self::Agent(snapshot) => snapshot.reason.as_deref(),
            Self::User(snapshot) => snapshot.reason.as_deref(),
        }
    }

    fn finish(&mut self, exit_code: Option<i32>, signal: Option<i32>, output_bytes: u64) {
        let is_success = exit_code == Some(0) && signal.is_none() && self.reason().is_none();
        match self {
            Self::Agent(snapshot) => {
                snapshot.exit_code = exit_code;
                snapshot.signal = signal;
                snapshot.output_bytes = output_bytes;
            }
            Self::User(snapshot) => {
                snapshot.exit_code = exit_code;
                snapshot.signal = signal;
                snapshot.output_bytes = output_bytes;
            }
        }
        self.mark_updated();
        self.set_state(if is_success {
            CommandState::Completed
        } else {
            CommandState::Failed
        });
    }

    fn mark_updated(&mut self) {
        let now = Some(crate::private_fs::unix_time_milliseconds());
        match self {
            Self::Agent(snapshot) => snapshot.updated_at_ms = now,
            Self::User(snapshot) => snapshot.updated_at_ms = now,
        }
    }

    fn update_output_bytes(&mut self) {
        if let Ok(metadata) = std::fs::metadata(self.output_ref()) {
            let output_bytes = metadata.len();
            if output_bytes != self.output_bytes() {
                self.mark_updated();
            }
            match self {
                Self::Agent(snapshot) => snapshot.output_bytes = output_bytes,
                Self::User(snapshot) => snapshot.output_bytes = output_bytes,
            }
        }
    }

    fn output_bytes(&self) -> u64 {
        match self {
            Self::Agent(snapshot) => snapshot.output_bytes,
            Self::User(snapshot) => snapshot.output_bytes,
        }
    }

    /// The status Claude Code gives a task in the same situation.
    fn task_status(&self) -> &'static str {
        match self.state() {
            CommandState::Running | CommandState::Background => "running",
            CommandState::Completed => "completed",
            CommandState::Cancelling => "killed",
            CommandState::Failed if self.reason() == Some(CANCELLED_COMMAND_REASON) => "killed",
            CommandState::Failed | CommandState::Unknown => "failed",
        }
    }

    fn agent(&self) -> Option<&CommandSnapshot> {
        match self {
            Self::Agent(snapshot) => Some(snapshot),
            Self::User(_) => None,
        }
    }

    fn user(&self) -> Option<&UserCommandSnapshot> {
        match self {
            Self::User(snapshot) => Some(snapshot),
            Self::Agent(_) => None,
        }
    }

    fn ensure_owner(&self, owner: OwnerScope) -> Result<(), ToolError> {
        match (self, owner) {
            (Self::Agent(_), OwnerScope::Agent) => Ok(()),
            (Self::User(_), OwnerScope::Agent) => Err(ToolError::InvalidInput(
                "command belongs to the interactive user".into(),
            )),
            (Self::User(snapshot), OwnerScope::User { chat_id }) if snapshot.chat_id == chat_id => {
                Ok(())
            }
            (_, OwnerScope::User { .. }) => Err(ToolError::InvalidInput(
                "command belongs to another owner".into(),
            )),
        }
    }
}

#[derive(Clone)]
struct Update {
    snapshot: ProcessSnapshot,
    result: Option<ToolResult>,
}
struct Entry {
    updates: watch::Sender<Update>,
    cancel: CancellationToken,
    /// Set when the model stopped the command with `TaskStop`; like Claude Code's `stopTask`, its
    /// end then needs no task notification.
    is_stopped_by_model: bool,
}

#[derive(Clone)]
pub(super) struct Registry {
    entries: Arc<Mutex<HashMap<Uuid, Entry>>>,
    events: broadcast::Sender<CommandSnapshot>,
    user_events: broadcast::Sender<UserCommandSnapshot>,
    /// The bash or zsh that runs every command, or why none is available.
    shell: Arc<Result<PathBuf, String>>,
}

impl Registry {
    pub(super) fn new() -> Self {
        Self::with_shell(interpreter::selected_shell())
    }

    fn with_shell(shell: Result<PathBuf, String>) -> Self {
        Self {
            entries: Arc::new(Mutex::new(HashMap::new())),
            events: broadcast::channel(256).0,
            user_events: broadcast::channel(256).0,
            shell: Arc::new(shell),
        }
    }
    pub(super) fn subscribe(&self) -> broadcast::Receiver<CommandSnapshot> {
        self.events.subscribe()
    }
    pub(super) fn subscribe_user(&self) -> broadcast::Receiver<UserCommandSnapshot> {
        self.user_events.subscribe()
    }

    fn emit(&self, snapshot: &ProcessSnapshot) {
        match snapshot {
            ProcessSnapshot::Agent(snapshot) => {
                let _ = self.events.send(snapshot.clone());
            }
            ProcessSnapshot::User(snapshot) => {
                let _ = self.user_events.send(snapshot.clone());
            }
        }
    }

    pub(super) async fn snapshots(&self) -> Vec<CommandSnapshot> {
        self.entries
            .lock()
            .await
            .values()
            .filter_map(|entry| {
                let mut snapshot = entry.updates.borrow().snapshot.clone();
                snapshot.update_output_bytes();
                match snapshot {
                    ProcessSnapshot::Agent(snapshot) => Some(snapshot),
                    ProcessSnapshot::User(_) => None,
                }
            })
            .collect()
    }

    pub(super) async fn user_snapshots(&self) -> Vec<UserCommandSnapshot> {
        self.entries
            .lock()
            .await
            .values()
            .filter_map(|entry| {
                let mut snapshot = entry.updates.borrow().snapshot.clone();
                snapshot.update_output_bytes();
                match snapshot {
                    ProcessSnapshot::User(snapshot) => Some(snapshot),
                    ProcessSnapshot::Agent(_) => None,
                }
            })
            .collect()
    }
    async fn background_process(
        &self,
        id: Uuid,
        owner: OwnerScope,
    ) -> Result<ProcessSnapshot, ToolError> {
        let entries = self.entries.lock().await;
        let entry = entries.get(&id).ok_or_else(|| {
            ToolError::InvalidInput("command does not exist in this live host".into())
        })?;
        entry.updates.borrow().snapshot.ensure_owner(owner)?;
        Ok(self.move_to_background(entry, BackgroundTrigger::User))
    }

    /// Claude Code's `onTimeout` → `startBackgrounding`: a foreground command that outlives its
    /// timeout keeps running in the background.
    async fn background_on_timeout(&self, id: Uuid) -> Option<ProcessSnapshot> {
        let entries = self.entries.lock().await;
        let entry = entries.get(&id)?;
        Some(self.move_to_background(entry, BackgroundTrigger::Timeout))
    }

    /// Publishes a running command as backgrounded; a command in any other state is left as is.
    fn move_to_background(&self, entry: &Entry, trigger: BackgroundTrigger) -> ProcessSnapshot {
        let mut update = entry.updates.borrow().clone();
        if update.snapshot.state() == &CommandState::Running {
            update.snapshot.set_state(CommandState::Background);
            update.snapshot.mark_backgrounded(trigger);
            entry.updates.send_replace(update.clone());
            self.emit(&update.snapshot);
        }
        update.snapshot
    }

    pub(super) async fn background(&self, id: Uuid) -> Result<CommandSnapshot, ToolError> {
        self.background_process(id, OwnerScope::Agent)
            .await?
            .agent()
            .cloned()
            .ok_or_else(|| {
                ToolError::InvalidInput("command belongs to the interactive user".into())
            })
    }

    pub(super) async fn background_user(
        &self,
        id: Uuid,
        chat_id: Uuid,
    ) -> Result<UserCommandSnapshot, ToolError> {
        let snapshot = self
            .background_process(id, OwnerScope::User { chat_id })
            .await?;
        snapshot
            .user()
            .filter(|snapshot| snapshot.chat_id == chat_id)
            .cloned()
            .ok_or_else(|| ToolError::InvalidInput("command belongs to another owner".into()))
    }

    async fn stop_process(
        &self,
        id: Uuid,
        owner: OwnerScope,
    ) -> Result<ProcessSnapshot, ToolError> {
        let mut updates = {
            let entries = self.entries.lock().await;
            let entry = entries.get(&id).ok_or_else(|| {
                ToolError::InvalidInput("command does not exist in this live host".into())
            })?;
            let mut update = entry.updates.borrow().clone();
            update.snapshot.ensure_owner(owner)?;
            if matches!(
                update.snapshot.state(),
                CommandState::Running | CommandState::Background
            ) {
                update.snapshot.set_state(CommandState::Cancelling);
                entry.updates.send_replace(update.clone());
                self.emit(&update.snapshot);
                entry.cancel.cancel();
            }
            entry.updates.subscribe()
        };
        loop {
            let update = updates.borrow().clone();
            if update.result.is_some() {
                return Ok(update.snapshot);
            }
            if updates.changed().await.is_err() {
                return Err(ToolError::InvalidInput(
                    "command status channel closed before a result".into(),
                ));
            }
        }
    }

    pub(super) async fn stop(&self, id: Uuid) -> Result<CommandSnapshot, ToolError> {
        self.stop_process(id, OwnerScope::Agent)
            .await?
            .agent()
            .cloned()
            .ok_or_else(|| {
                ToolError::InvalidInput("command belongs to the interactive user".into())
            })
    }

    pub(super) async fn stop_user(
        &self,
        id: Uuid,
        chat_id: Uuid,
    ) -> Result<UserCommandSnapshot, ToolError> {
        let snapshot = self.stop_process(id, OwnerScope::User { chat_id }).await?;
        snapshot
            .user()
            .filter(|snapshot| snapshot.chat_id == chat_id)
            .cloned()
            .ok_or_else(|| ToolError::InvalidInput("command belongs to another owner".into()))
    }
    pub(super) async fn stop_loop(&self, id: Uuid) -> Vec<CommandSnapshot> {
        let commands = self.snapshots().await;
        let mut results = Vec::new();
        for command in commands.into_iter().filter(|command| command.loop_id == id) {
            if let Ok(result) = self.stop(command.operation_id).await {
                results.push(result);
            }
        }
        results
    }
    pub(super) async fn read(
        &self,
        id: Uuid,
        offset: u64,
        limit: usize,
    ) -> Result<ToolResult, ToolError> {
        let snapshot = self
            .snapshots()
            .await
            .into_iter()
            .find(|snapshot| snapshot.operation_id == id)
            .ok_or_else(|| {
                ToolError::InvalidInput("command does not exist in this live host".into())
            })?;
        command_output(&snapshot, offset, limit)
    }

    pub(super) async fn read_user(
        &self,
        id: Uuid,
        chat_id: Uuid,
        offset: u64,
        limit: usize,
    ) -> Result<ToolResult, ToolError> {
        let snapshot = self
            .user_snapshots()
            .await
            .into_iter()
            .find(|snapshot| snapshot.operation_id == id && snapshot.chat_id == chat_id)
            .ok_or_else(|| {
                ToolError::InvalidInput("command does not exist for this Chat".into())
            })?;
        process_output(&ProcessSnapshot::User(snapshot), offset, limit)
    }
    /// Claude Code `TaskStopTool.validateInput`: the task must exist and still be running.
    pub(super) async fn stoppable_task(&self, task_id: &str) -> Result<(), ToolError> {
        self.running_task(task_id).await.map(|_| ())
    }

    async fn running_task(&self, task_id: &str) -> Result<ProcessSnapshot, ToolError> {
        let not_found = || ToolError::Rejected(format!("No task found with ID: {task_id}"));
        let id = Uuid::parse_str(task_id).map_err(|_| not_found())?;
        let snapshot = self
            .entries
            .lock()
            .await
            .get(&id)
            .map(|entry| entry.updates.borrow().snapshot.clone())
            .ok_or_else(not_found)?;
        if !matches!(
            snapshot.state(),
            CommandState::Running | CommandState::Background
        ) {
            return Err(ToolError::Rejected(format!(
                "Task {task_id} is not running (status: {})",
                snapshot.task_status()
            )));
        }
        Ok(snapshot)
    }

    /// Stops one of `loop_id`'s commands for the model; the user's commands and other goals'
    /// commands are left to the user.
    pub(super) async fn stop_task(
        &self,
        task_id: &str,
        loop_id: Uuid,
    ) -> Result<ToolResult, ToolError> {
        let snapshot = self.running_task(task_id).await?;
        let Some(command) = snapshot
            .agent()
            .filter(|command| command.loop_id == loop_id)
        else {
            return Err(ToolError::Rejected(format!(
                "Task {task_id} does not belong to this goal; only the user can stop it"
            )));
        };
        if let Some(entry) = self.entries.lock().await.get_mut(&command.operation_id) {
            entry.is_stopped_by_model = true;
        }
        let stopped = self.stop(command.operation_id).await?;
        model_result::task_stopped(&stopped)
    }

    pub(super) async fn is_stopped_by_model(&self, id: Uuid) -> bool {
        self.entries
            .lock()
            .await
            .get(&id)
            .is_some_and(|entry| entry.is_stopped_by_model)
    }

    /// The result a finished command gave, or would have given, the model.
    pub(super) async fn finished_result(&self, id: Uuid) -> Option<ToolResult> {
        self.entries
            .lock()
            .await
            .get(&id)
            .and_then(|entry| entry.updates.borrow().result.clone())
    }

    pub(super) async fn state(&self, id: Uuid) -> Option<CommandState> {
        self.entries
            .lock()
            .await
            .get(&id)
            .map(|entry| entry.updates.borrow().snapshot.state().clone())
    }

    pub(super) async fn execute(
        &self,
        cwd: &Path,
        input: ShellInput,
        task_output_dir: &PrivateDirectory,
        operation_id: Uuid,
        loop_id: Uuid,
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        self.execute_process(
            cwd,
            input,
            task_output_dir,
            CommandOwner::Agent {
                operation_id,
                loop_id,
            },
            cancel,
            true,
        )
        .await
    }

    pub(super) async fn execute_user(
        &self,
        cwd: &Path,
        task_output_dir: &PrivateDirectory,
        execution: UserShellExecution,
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        let UserShellExecution {
            command,
            operation_id,
            chat_id,
            background,
        } = execution;
        if command.trim().is_empty() || command.contains('\0') {
            return Err(ToolError::InvalidInput(
                "command cannot be empty or contain NUL".into(),
            ));
        }
        self.execute_process(
            cwd,
            ShellInput {
                command,
                timeout: None,
                description: None,
                run_in_background: background,
                cwd: None,
                readable_directories: Vec::new(),
            },
            task_output_dir,
            CommandOwner::User {
                operation_id,
                chat_id,
            },
            cancel,
            false,
        )
        .await
    }

    async fn execute_process(
        &self,
        cwd: &Path,
        input: ShellInput,
        task_output_dir: &PrivateDirectory,
        owner: CommandOwner,
        cancel: CancellationToken,
        wait_for_completion: bool,
    ) -> Result<ToolResult, ToolError> {
        if cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let shell = self
            .shell
            .as_ref()
            .as_ref()
            .map_err(|reason| ToolError::Rejected(reason.clone()))?;
        let operation_id = match owner {
            CommandOwner::Agent { operation_id, .. } | CommandOwner::User { operation_id, .. } => {
                operation_id
            }
        };
        let directory = open_existing(cwd, true)?;
        task_output_dir
            .create()
            .map_err(|error| io_error(task_output_dir.path(), error))?;
        // 照 Claude Code `initTaskOutput`：独占创建且不跟随符号链接，路径在任务启动时确定。
        let output_path = task_output_dir
            .path()
            .join(format!("{operation_id}.output"));
        let output = OpenOptions::new()
            .read(true)
            .append(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&output_path)
            .map_err(|error| io_error(&output_path, error))?;
        let mut command = tokio::process::Command::new(shell);
        command.arg("-c").arg(&input.command);
        command
            .stdin(Stdio::null())
            .stdout(
                output
                    .try_clone()
                    .map_err(|error| io_error(&output_path, error))?,
            )
            .stderr(
                output
                    .try_clone()
                    .map_err(|error| io_error(&output_path, error))?,
            )
            .kill_on_drop(true);
        command.as_std_mut().process_group(0);
        unsafe {
            command.pre_exec(move || {
                if libc::fchdir(directory.as_raw_fd()) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().map_err(|error| io_error(cwd, error))?;
        let pid = child.id().ok_or_else(|| ToolError::Io {
            path: cwd.into(),
            reason: "command has no process identifier".into(),
        })?;
        let initial_state = if input.run_in_background {
            CommandState::Background
        } else {
            CommandState::Running
        };
        let snapshot = match owner {
            CommandOwner::Agent {
                operation_id,
                loop_id,
            } => ProcessSnapshot::Agent(CommandSnapshot {
                updated_at_ms: Some(crate::private_fs::unix_time_milliseconds()),
                operation_id,
                loop_id,
                command: input.command.clone(),
                description: input.description.clone(),
                cwd: cwd.into(),
                pid: Some(pid),
                state: initial_state,
                was_background: input.run_in_background,
                backgrounded_on_timeout: false,
                exit_code: None,
                signal: None,
                reason: None,
                output_ref: output_path,
                output_bytes: 0,
            }),
            CommandOwner::User {
                operation_id,
                chat_id,
            } => ProcessSnapshot::User(UserCommandSnapshot {
                updated_at_ms: Some(crate::private_fs::unix_time_milliseconds()),
                operation_id,
                chat_id,
                command: input.command.clone(),
                cwd: cwd.into(),
                pid: Some(pid),
                state: initial_state,
                was_background: input.run_in_background,
                backgrounded_on_timeout: false,
                exit_code: None,
                signal: None,
                reason: None,
                output_ref: output_path,
                output_bytes: 0,
            }),
        };
        let (updates, mut receiver) = watch::channel(Update {
            snapshot: snapshot.clone(),
            result: None,
        });
        let command_cancel = CancellationToken::new();
        self.entries.lock().await.insert(
            operation_id,
            Entry {
                updates,
                cancel: command_cancel.clone(),
                is_stopped_by_model: false,
            },
        );
        self.emit(&snapshot);
        let started_snapshot = snapshot.clone();
        let input_started_in_background = input.run_in_background;
        let registry = self.clone();
        let foreground_timeout = input.foreground_timeout();
        let moves_to_background_on_timeout = allows_auto_background(&input.command);
        tokio::spawn(async move {
            let mut snapshot = snapshot;
            // Like Claude Code's `ShellCommand.background`, moving to the background ends the
            // foreground timeout.
            let mut is_foreground = !input.run_in_background;
            let timeout = tokio::time::sleep(foreground_timeout);
            tokio::pin!(timeout);
            let mut output_poll = tokio::time::interval(OUTPUT_POLL_INTERVAL);
            output_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            output_poll.tick().await;
            let status = loop {
                tokio::select! {
                    status=child.wait()=>break status.map_err(|error|io_error(snapshot.cwd(),error)),
                    _=cancel.cancelled()=>{snapshot.set_reason(CANCELLED_COMMAND_REASON.into());break terminate(&mut child,pid,snapshot.cwd()).await;},
                    _=command_cancel.cancelled()=>{snapshot.set_reason(CANCELLED_COMMAND_REASON.into());break terminate(&mut child,pid,snapshot.cwd()).await;},
                    _=&mut timeout, if is_foreground=>{
                        if !moves_to_background_on_timeout {
                            snapshot.set_reason(TIMED_OUT_REASON.into());
                            break terminate(&mut child,pid,snapshot.cwd()).await;
                        }
                        is_foreground = false;
                        if let Some(shared_snapshot) = registry.background_on_timeout(operation_id).await {
                            snapshot.set_state(shared_snapshot.state().clone());
                            snapshot.merge_background_history(&shared_snapshot);
                        }
                    },
                    _=output_poll.tick()=>{
                        let shared_snapshot = registry
                            .entries
                            .lock()
                            .await
                            .get(&operation_id)
                            .map(|entry| entry.updates.borrow().snapshot.clone());
                        if let Some(shared_snapshot) = shared_snapshot
                            && matches!(shared_snapshot.state(), CommandState::Background | CommandState::Cancelling)
                        {
                            snapshot.set_state(shared_snapshot.state().clone());
                            snapshot.merge_background_history(&shared_snapshot);
                            is_foreground &= snapshot.state() != &CommandState::Background;
                        }
                        let previous_bytes = snapshot.output_bytes();
                        snapshot.update_output_bytes();
                        if snapshot.output_bytes() > MAX_COMMAND_OUTPUT_BYTES {
                            let _ = output.set_len(MAX_COMMAND_OUTPUT_BYTES);
                            snapshot.update_output_bytes();
                            snapshot.set_reason(OUTPUT_LIMIT_REASON.into());
                            break terminate(&mut child,pid,snapshot.cwd()).await;
                        }
                        if snapshot.output_bytes() != previous_bytes {
                            if let Some(entry) = registry.entries.lock().await.get(&operation_id) {
                                entry.updates.send_replace(Update { snapshot: snapshot.clone(), result: None });
                            }
                            registry.emit(&snapshot);
                        }
                    }
                }
            };
            match status {
                Ok(status) => {
                    let output_bytes = output
                        .metadata()
                        .map(|metadata| metadata.len())
                        .unwrap_or(0);
                    snapshot.finish(status.code(), status.signal(), output_bytes);
                }
                Err(error) => {
                    snapshot.set_state(CommandState::Unknown);
                    snapshot.set_reason(error.to_string());
                }
            }
            if process_group_exists(pid) {
                snapshot.set_state(CommandState::Unknown);
                snapshot.set_reason("shell exited while its process group still contains descendants; their result is unresolved".into());
                terminate_process_group(pid).await;
            }
            if output
                .metadata()
                .is_ok_and(|metadata| metadata.len() > MAX_COMMAND_OUTPUT_BYTES)
            {
                let _ = output.set_len(MAX_COMMAND_OUTPUT_BYTES);
                if snapshot.reason().is_none() {
                    snapshot.set_reason(OUTPUT_LIMIT_REASON.into());
                    snapshot.set_state(CommandState::Failed);
                }
            }
            if let Err(error) = output.sync_all() {
                snapshot.set_state(CommandState::Unknown);
                snapshot.set_reason(format!("cannot save command output: {error}"));
            }
            snapshot.update_output_bytes();
            let result = match &snapshot {
                ProcessSnapshot::Agent(command) => {
                    model_result::finished(command, foreground_timeout)
                }
                ProcessSnapshot::User(_) => process_output(&snapshot, 0, DEFAULT_OUTPUT_BYTES)
                    .unwrap_or_else(|error| ToolResult {
                        content: error.to_string(),
                        is_error: true,
                        output_ref: Some(snapshot.output_ref().into()),
                    }),
            };
            if let Some(entry) = registry.entries.lock().await.get(&operation_id) {
                snapshot.merge_background_history(&entry.updates.borrow().snapshot);
                entry.updates.send_replace(Update {
                    snapshot: snapshot.clone(),
                    result: Some(result),
                });
            }
            registry.emit(&snapshot);
        });
        if !wait_for_completion {
            return process_output(&started_snapshot, 0, DEFAULT_OUTPUT_BYTES);
        }
        loop {
            let update = receiver.borrow().clone();
            if let Some(result) = update.result {
                return Ok(result);
            }
            if update.snapshot.state() == &CommandState::Background {
                return match &update.snapshot {
                    ProcessSnapshot::Agent(command) => Ok(model_result::background_started(
                        command,
                        !input_started_in_background && !command.backgrounded_on_timeout,
                    )),
                    ProcessSnapshot::User(_) => {
                        process_output(&update.snapshot, 0, DEFAULT_OUTPUT_BYTES)
                    }
                };
            }
            if receiver.changed().await.is_err() {
                return Err(ToolError::InvalidInput(
                    "command status channel closed before a result".into(),
                ));
            }
        }
    }
}

pub(super) fn command_output(
    snapshot: &CommandSnapshot,
    offset: u64,
    limit: usize,
) -> Result<ToolResult, ToolError> {
    process_output(&ProcessSnapshot::Agent(snapshot.clone()), offset, limit)
}

pub(super) fn user_command_output(
    snapshot: &UserCommandSnapshot,
    offset: u64,
    limit: usize,
) -> Result<ToolResult, ToolError> {
    process_output(&ProcessSnapshot::User(snapshot.clone()), offset, limit)
}

fn process_output(
    snapshot: &ProcessSnapshot,
    offset: u64,
    limit: usize,
) -> Result<ToolResult, ToolError> {
    if limit == 0 || limit > super::MAX_OUTPUT_BYTES {
        return Err(ToolError::InvalidInput(
            "output limit must be between 1 and 65536 bytes".into(),
        ));
    }
    let mut output = open_existing(snapshot.output_ref(), false)?;
    let total = output
        .metadata()
        .map_err(|error| io_error(snapshot.output_ref(), error))?
        .len();
    if offset > total {
        return Err(ToolError::InvalidInput(
            "output offset exceeds the current file length".into(),
        ));
    }
    output
        .seek(SeekFrom::Start(offset))
        .map_err(|error| io_error(snapshot.output_ref(), error))?;
    let mut bytes = Vec::new();
    output
        .take((total - offset).min(limit as u64))
        .read_to_end(&mut bytes)
        .map_err(|error| io_error(snapshot.output_ref(), error))?;
    let mut next_offset = offset + bytes.len() as u64;
    let mut truncated = next_offset < total;
    if truncated
        && let Err(error) = std::str::from_utf8(&bytes)
        && error.error_len().is_none()
        && error.valid_up_to() > 0
    {
        bytes.truncate(error.valid_up_to());
        next_offset = offset + bytes.len() as u64;
        truncated = true;
    }
    let value = match snapshot {
        ProcessSnapshot::Agent(snapshot) => {
            json!({"operation_id":snapshot.operation_id,"loop_id":snapshot.loop_id,"command":snapshot.command,"cwd":snapshot.cwd.to_string_lossy(),"pid":snapshot.pid,"state":snapshot.state,"was_background":snapshot.was_background,"exit_code":snapshot.exit_code,"signal":snapshot.signal,"reason":snapshot.reason,"output":String::from_utf8_lossy(&bytes),"encoding":"utf-8-lossy","total_bytes":total,"offset":offset,"truncated":truncated,"next_offset":if truncated {Some(next_offset)} else {None},"output_ref":snapshot.output_ref.to_string_lossy()})
        }
        ProcessSnapshot::User(snapshot) => {
            json!({"operation_id":snapshot.operation_id,"chat_id":snapshot.chat_id,"origin":"user_shell","command":snapshot.command,"cwd":snapshot.cwd.to_string_lossy(),"pid":snapshot.pid,"state":snapshot.state,"was_background":snapshot.was_background,"exit_code":snapshot.exit_code,"signal":snapshot.signal,"reason":snapshot.reason,"output":String::from_utf8_lossy(&bytes),"encoding":"utf-8-lossy","total_bytes":total,"offset":offset,"truncated":truncated,"next_offset":if truncated {Some(next_offset)} else {None},"output_ref":snapshot.output_ref.to_string_lossy()})
        }
    };
    Ok(ToolResult {
        content: value.to_string(),
        is_error: matches!(
            snapshot.state(),
            CommandState::Failed | CommandState::Unknown
        ),
        output_ref: Some(snapshot.output_ref().into()),
    })
}

async fn terminate(
    child: &mut tokio::process::Child,
    pid: u32,
    cwd: &Path,
) -> Result<std::process::ExitStatus, ToolError> {
    let deadline = tokio::time::Instant::now() + TERMINATION_GRACE;
    unsafe { libc::kill(-(pid as i32), libc::SIGTERM) };
    let status = tokio::time::timeout_at(deadline, child.wait()).await;
    while process_group_exists(pid) && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(PROCESS_CHECK_INTERVAL).await;
    }
    if process_group_exists(pid) {
        unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
    }
    let status = match status {
        Ok(result) => result,
        Err(_) => tokio::time::timeout(Duration::from_secs(1), child.wait())
            .await
            .map_err(|_| {
                io_error(
                    cwd,
                    std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "cannot confirm command termination",
                    ),
                )
            })?,
    };
    let deadline = tokio::time::Instant::now() + TERMINATION_GRACE;
    while process_group_exists(pid) && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(PROCESS_CHECK_INTERVAL).await;
    }
    status.map_err(|error| io_error(cwd, error))
}

async fn terminate_process_group(pid: u32) {
    unsafe { libc::kill(-(pid as i32), libc::SIGTERM) };
    let deadline = tokio::time::Instant::now() + TERMINATION_GRACE;
    while process_group_exists(pid) && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(PROCESS_CHECK_INTERVAL).await;
    }
    if process_group_exists(pid) {
        unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    while process_group_exists(pid) && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(PROCESS_CHECK_INTERVAL).await;
    }
}

fn process_group_exists(pid: u32) -> bool {
    (unsafe { libc::kill(-(pid as i32), 0) == 0 })
        || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::Value;
    use tokio::sync::broadcast;

    use super::*;

    fn shell_input(command: &str) -> ShellInput {
        ShellInput {
            command: command.into(),
            timeout: None,
            description: None,
            run_in_background: false,
            cwd: None,
            readable_directories: Vec::new(),
        }
    }

    async fn run_agent_command(registry: &Registry, input: ShellInput) -> ToolResult {
        let directory = tempfile::tempdir().unwrap();
        let working_directory = directory.path().canonicalize().unwrap();
        registry
            .execute(
                &working_directory,
                input,
                &PrivateDirectory::trusted(working_directory.join("output")),
                Uuid::new_v4(),
                Uuid::new_v4(),
                CancellationToken::new(),
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn commands_run_with_the_selected_shell() {
        for (shell, version_variable) in
            [("/bin/bash", "BASH_VERSION"), ("/bin/zsh", "ZSH_VERSION")]
        {
            if !Path::new(shell).exists() {
                continue;
            }
            let registry = Registry::with_shell(Ok(shell.into()));
            let result = run_agent_command(
                &registry,
                shell_input(&format!(
                    "printf '%s' \"${{{version_variable}:+{version_variable}}}\""
                )),
            )
            .await;
            assert_eq!(result.content, version_variable, "{shell}");
        }
    }

    #[tokio::test]
    async fn a_missing_shell_explains_that_bash_or_zsh_is_required() {
        let directory = tempfile::tempdir().unwrap();
        let working_directory = directory.path().canonicalize().unwrap();
        let output_directory = working_directory.join("output");
        let registry = Registry::with_shell(Err(interpreter::NO_SUITABLE_SHELL.into()));
        let error = registry
            .execute(
                &working_directory,
                shell_input("pwd"),
                &PrivateDirectory::trusted(output_directory.clone()),
                Uuid::new_v4(),
                Uuid::new_v4(),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), interpreter::NO_SUITABLE_SHELL);
        assert!(!output_directory.exists());
    }

    #[tokio::test]
    async fn the_timeout_ends_a_foreground_sleep_but_not_a_background_command() {
        let registry = Registry::new();
        let mut foreground = shell_input("sleep 5; printf never");
        foreground.timeout = Some(100);
        let result = run_agent_command(&registry, foreground).await;
        assert_eq!(result.content, "Exit code 143\nCommand timed out after 0s");
        assert!(result.is_error);

        let mut background = shell_input("sleep 0.3; printf finished");
        background.timeout = Some(100);
        background.run_in_background = true;
        let directory = tempfile::tempdir().unwrap();
        let working_directory = directory.path().canonicalize().unwrap();
        let operation_id = Uuid::new_v4();
        let mut updates = registry.subscribe();
        let started = registry
            .execute(
                &working_directory,
                background,
                &PrivateDirectory::trusted(working_directory.join("output")),
                operation_id,
                Uuid::new_v4(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(started.content.starts_with(&format!(
            "Command running in background with ID: {operation_id}. Output is being written to: "
        )));
        let finished = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let snapshot = updates.recv().await.unwrap();
                if snapshot.operation_id == operation_id
                    && matches!(
                        snapshot.state,
                        CommandState::Completed | CommandState::Failed
                    )
                {
                    break snapshot;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(finished.state, CommandState::Completed);
        assert_eq!(
            registry
                .finished_result(operation_id)
                .await
                .unwrap()
                .content,
            "finished"
        );
    }

    async fn wait_for_terminal_agent_snapshot(
        updates: &mut broadcast::Receiver<CommandSnapshot>,
        operation_id: Uuid,
    ) -> CommandSnapshot {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let snapshot = updates.recv().await.unwrap();
                if snapshot.operation_id == operation_id
                    && matches!(
                        snapshot.state,
                        CommandState::Completed | CommandState::Failed | CommandState::Unknown
                    )
                {
                    break snapshot;
                }
            }
        })
        .await
        .expect("agent command did not reach a terminal state")
    }

    #[tokio::test]
    async fn a_foreground_command_that_outlives_its_timeout_keeps_running_in_the_background() {
        let directory = tempfile::tempdir().unwrap();
        let working_directory = directory.path().canonicalize().unwrap();
        let output_directory = working_directory.join("output");
        let registry = Registry::new();
        let mut updates = registry.subscribe();
        let operation_id = Uuid::new_v4();
        let mut input = shell_input(
            "printf started; while [ ! -e release ]; do sleep 0.02; done; printf ' finished'",
        );
        input.timeout = Some(100);
        let result = registry
            .execute(
                &working_directory,
                input,
                &PrivateDirectory::trusted(output_directory.clone()),
                operation_id,
                Uuid::new_v4(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let output_ref = output_directory.join(format!("{operation_id}.output"));
        assert_eq!(
            result.content,
            format!(
                "Command running in background with ID: {operation_id}. Output is being written to: {}",
                output_ref.display()
            )
        );
        assert!(!result.is_error);
        let moved = registry
            .snapshots()
            .await
            .into_iter()
            .find(|snapshot| snapshot.operation_id == operation_id)
            .unwrap();
        assert_eq!(moved.state, CommandState::Background);
        assert!(moved.was_background && moved.backgrounded_on_timeout);

        std::fs::write(working_directory.join("release"), "").unwrap();
        let finished = wait_for_terminal_agent_snapshot(&mut updates, operation_id).await;
        assert_eq!(finished.state, CommandState::Completed);
        assert!(finished.was_background && finished.backgrounded_on_timeout);
        assert_eq!(
            registry
                .finished_result(operation_id)
                .await
                .unwrap()
                .content,
            "started finished"
        );
    }

    #[tokio::test]
    async fn a_user_command_that_outlives_its_timeout_moves_to_the_background_too() {
        let directory = tempfile::tempdir().unwrap();
        let working_directory = directory.path().canonicalize().unwrap();
        let registry = Registry::new();
        let mut updates = registry.subscribe_user();
        let operation_id = Uuid::new_v4();
        let chat_id = Uuid::new_v4();
        let mut input = shell_input("while :; do sleep 0.02; done");
        input.timeout = Some(100);
        registry
            .execute_process(
                &working_directory,
                input,
                &PrivateDirectory::trusted(working_directory.join("output")),
                CommandOwner::User {
                    operation_id,
                    chat_id,
                },
                CancellationToken::new(),
                false,
            )
            .await
            .unwrap();
        let moved = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let snapshot = updates.recv().await.unwrap();
                if snapshot.operation_id == operation_id
                    && snapshot.state == CommandState::Background
                {
                    break snapshot;
                }
            }
        })
        .await
        .expect("the user command did not move to the background");
        assert!(moved.was_background && moved.backgrounded_on_timeout);
        let stopped = registry.stop_user(operation_id, chat_id).await.unwrap();
        assert_eq!(stopped.reason.as_deref(), Some(CANCELLED_COMMAND_REASON));
        assert!(stopped.backgrounded_on_timeout);
    }

    #[test]
    fn only_commands_led_by_sleep_stay_in_the_foreground_at_their_timeout() {
        for (command, allows) in [
            ("sleep 5", false),
            ("  sleep 0.5 && printf done", false),
            ("sleep", false),
            ("printf waiting; sleep 30", true),
            ("npm run dev", true),
            ("while [ ! -e ready ]; do sleep 1; done", true),
            ("sleepy-server --port 8080", true),
        ] {
            assert_eq!(allows_auto_background(command), allows, "{command}");
        }
    }

    #[test]
    fn timeouts_follow_claude_code_defaults_and_cap_at_ten_minutes() {
        let timeout = |requested| {
            let mut input = shell_input("true");
            input.timeout = requested;
            input.foreground_timeout()
        };
        assert_eq!(timeout(None), Duration::from_millis(DEFAULT_TIMEOUT_MS));
        assert_eq!(timeout(Some(0)), Duration::from_millis(DEFAULT_TIMEOUT_MS));
        assert_eq!(timeout(Some(5_000)), Duration::from_secs(5));
        assert_eq!(
            timeout(Some(3_600_000)),
            Duration::from_millis(MAX_TIMEOUT_MS)
        );
    }

    #[test]
    fn shell_admission_follows_claude_code_read_only_and_accept_edits_rules() {
        let directory = tempfile::tempdir().unwrap();
        let cwd = directory.path().canonicalize().unwrap();
        let workspace = super::super::Workspace {
            workdir: directory.path().to_path_buf(),
            additional_directories: Vec::new(),
        };
        let read_only = |command: &str| shell_input(command).is_read_only(&cwd, &workspace);
        let accept_edits =
            |command: &str| shell_input(command).is_allowed_in_accept_edits(&cwd, &workspace);

        assert!(read_only("git diff --cached --stat | head -30"));
        assert!(read_only("ls -la && git status"));
        assert!(!read_only("touch notes.txt"));
        assert!(!read_only("cat ../outside.txt"));

        assert!(accept_edits("git diff --cached --stat | head -30"));
        assert!(accept_edits("touch notes.txt"));
        assert!(accept_edits("mkdir -p reports/daily"));
        assert!(accept_edits("touch notes.txt && rm -rf reports"));
        assert!(!accept_edits("touch ../outside.txt"));
        assert!(!accept_edits("cp --target-directory=reports source.txt"));
        assert!(!accept_edits("rm -rf *"));
    }

    #[test]
    fn readable_internal_directories_admit_read_only_commands_only() {
        let directory = tempfile::tempdir().unwrap();
        let cwd = directory.path().canonicalize().unwrap();
        let internal = tempfile::tempdir().unwrap();
        let internal = internal.path().canonicalize().unwrap();
        let workspace = super::super::Workspace {
            workdir: cwd.clone(),
            additional_directories: Vec::new(),
        };
        let mut input = shell_input(&format!("cat {}/task.output", internal.display()));
        assert!(!input.is_read_only(&cwd, &workspace));
        input.readable_directories = vec![internal.clone()];
        assert!(input.is_read_only(&cwd, &workspace));
        input.command = format!("touch {}/task.output", internal.display());
        assert!(!input.is_allowed_in_accept_edits(&cwd, &workspace));
    }

    #[tokio::test]
    async fn read_only_commands_run_through_the_shell() {
        let directory = tempfile::tempdir().unwrap();
        let working_directory = directory.path().canonicalize().unwrap();
        let registry = Registry::new();
        let result = registry
            .execute(
                &working_directory,
                shell_input("pwd && echo via-shell"),
                &PrivateDirectory::trusted(working_directory.join("output")),
                Uuid::new_v4(),
                Uuid::new_v4(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            result.content,
            format!("{}\nvia-shell", working_directory.display())
        );
    }

    async fn wait_for_terminal_user_snapshot(
        updates: &mut broadcast::Receiver<UserCommandSnapshot>,
        operation_id: Uuid,
    ) -> UserCommandSnapshot {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let snapshot = updates.recv().await.unwrap();
                if snapshot.operation_id == operation_id
                    && matches!(
                        snapshot.state,
                        CommandState::Completed | CommandState::Failed | CommandState::Unknown
                    )
                {
                    break snapshot;
                }
            }
        })
        .await
        .expect("user shell did not reach a terminal state")
    }

    async fn user_snapshot(registry: &Registry, operation_id: Uuid) -> UserCommandSnapshot {
        registry
            .user_snapshots()
            .await
            .into_iter()
            .find(|snapshot| snapshot.operation_id == operation_id)
            .expect("user shell snapshot")
    }

    #[tokio::test]
    async fn agent_snapshot_keeps_explicit_background_history_through_completion() {
        let directory = tempfile::tempdir().unwrap();
        let working_directory = directory.path().canonicalize().unwrap();
        let output_directory = PrivateDirectory::trusted(working_directory.join("output"));
        let registry = Registry::new();
        let mut updates = registry.subscribe();
        let operation_id = Uuid::new_v4();
        let loop_id = Uuid::new_v4();
        let execution = {
            let registry = registry.clone();
            let working_directory = working_directory.clone();
            tokio::spawn(async move {
                registry
                    .execute(
                        &working_directory,
                        shell_input("sleep 0.1"),
                        &output_directory,
                        operation_id,
                        loop_id,
                        CancellationToken::new(),
                    )
                    .await
                    .unwrap()
            })
        };
        loop {
            let snapshot = updates.recv().await.unwrap();
            if snapshot.operation_id == operation_id && snapshot.state == CommandState::Running {
                assert!(!snapshot.was_background);
                break;
            }
        }
        let background = registry.background(operation_id).await.unwrap();
        assert_eq!(background.state, CommandState::Background);
        assert!(background.was_background);
        execution.await.unwrap();
        let terminal = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let snapshot = updates.recv().await.unwrap();
                if snapshot.operation_id == operation_id
                    && matches!(
                        snapshot.state,
                        CommandState::Completed | CommandState::Failed
                    )
                {
                    break snapshot;
                }
            }
        })
        .await
        .unwrap();
        assert!(terminal.was_background);
    }

    #[tokio::test]
    async fn user_snapshot_keeps_explicit_background_history_through_completion() {
        let directory = tempfile::tempdir().unwrap();
        let working_directory = directory.path().canonicalize().unwrap();
        let registry = Registry::new();
        let mut updates = registry.subscribe_user();
        let operation_id = Uuid::new_v4();
        let chat_id = Uuid::new_v4();
        registry
            .execute_user(
                &working_directory,
                &PrivateDirectory::trusted(working_directory.join("output")),
                UserShellExecution {
                    command: "sleep 0.1".into(),
                    operation_id,
                    chat_id,
                    background: false,
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();
        loop {
            let snapshot = updates.recv().await.unwrap();
            if snapshot.operation_id == operation_id && snapshot.state == CommandState::Running {
                assert!(!snapshot.was_background);
                break;
            }
        }
        let background = registry
            .background_user(operation_id, chat_id)
            .await
            .unwrap();
        assert_eq!(background.state, CommandState::Background);
        assert!(background.was_background);
        let terminal = wait_for_terminal_user_snapshot(&mut updates, operation_id).await;
        assert!(terminal.was_background);
    }

    #[tokio::test]
    async fn user_shell_cleans_up_descendants_before_publishing_its_final_state() {
        let directory = tempfile::tempdir().unwrap();
        let working_directory = directory.path().canonicalize().unwrap();
        let output_directory = PrivateDirectory::trusted(working_directory.join("output"));
        let registry = Registry::new();
        let mut updates = registry.subscribe_user();
        let operation_id = Uuid::new_v4();
        let chat_id = Uuid::new_v4();

        registry
            .execute_user(
                &working_directory,
                &output_directory,
                UserShellExecution {
                    command: "/bin/sh -c 'trap \"\" TERM; echo $$ > descendant.pid; sleep 0.8; echo late > late-write' &".into(),
                    operation_id,
                    chat_id,
                    background: false,
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();

        let finished = wait_for_terminal_user_snapshot(&mut updates, operation_id).await;
        assert_eq!(finished.state, CommandState::Unknown);
        assert!(finished.reason.as_deref().unwrap().contains("descendants"));
        assert!(
            !process_group_exists(finished.pid.unwrap()),
            "the final state was published before the process group was gone"
        );
        tokio::time::sleep(Duration::from_millis(900)).await;
        assert!(
            !working_directory.join("late-write").exists(),
            "a descendant produced a side effect after the final state"
        );
    }

    #[tokio::test]
    async fn user_shell_output_is_capped_at_sixty_four_mib() {
        let directory = tempfile::tempdir().unwrap();
        let working_directory = directory.path().canonicalize().unwrap();
        let output_directory = PrivateDirectory::trusted(working_directory.join("output"));
        let registry = Registry::new();
        let mut updates = registry.subscribe_user();
        let operation_id = Uuid::new_v4();

        registry
            .execute_user(
                &working_directory,
                &output_directory,
                UserShellExecution {
                    command: "dd if=/dev/zero bs=1048576 count=65 2>/dev/null".into(),
                    operation_id,
                    chat_id: Uuid::new_v4(),
                    background: false,
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();

        let finished = wait_for_terminal_user_snapshot(&mut updates, operation_id).await;
        assert_eq!(finished.state, CommandState::Failed);
        assert_eq!(finished.reason.as_deref(), Some("output_limit_exceeded"));
        assert_eq!(finished.output_bytes, MAX_COMMAND_OUTPUT_BYTES);
        assert_eq!(
            std::fs::metadata(&finished.output_ref).unwrap().len(),
            MAX_COMMAND_OUTPUT_BYTES
        );
        let result = registry
            .read_user(operation_id, finished.chat_id, 0, DEFAULT_OUTPUT_BYTES)
            .await
            .unwrap();
        let value: Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(value["total_bytes"], MAX_COMMAND_OUTPUT_BYTES);
        assert_eq!(value["reason"], "output_limit_exceeded");
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn wrong_owner_endpoints_do_not_change_a_user_shell_state() {
        let directory = tempfile::tempdir().unwrap();
        let working_directory = directory.path().canonicalize().unwrap();
        let registry = Registry::new();
        let operation_id = Uuid::new_v4();
        let chat_id = Uuid::new_v4();

        registry
            .execute_user(
                &working_directory,
                &PrivateDirectory::trusted(working_directory.join("output")),
                UserShellExecution {
                    command: "sleep 30".into(),
                    operation_id,
                    chat_id,
                    background: false,
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();

        assert!(registry.background(operation_id).await.is_err());
        assert_eq!(
            user_snapshot(&registry, operation_id).await.state,
            CommandState::Running
        );
        assert!(registry.stop(operation_id).await.is_err());
        assert_eq!(
            user_snapshot(&registry, operation_id).await.state,
            CommandState::Running
        );
        assert!(
            registry
                .background_user(operation_id, Uuid::new_v4())
                .await
                .is_err()
        );
        assert_eq!(
            user_snapshot(&registry, operation_id).await.state,
            CommandState::Running
        );
        assert!(
            registry
                .stop_user(operation_id, Uuid::new_v4())
                .await
                .is_err()
        );
        assert_eq!(
            user_snapshot(&registry, operation_id).await.state,
            CommandState::Running
        );

        let stopped = registry.stop_user(operation_id, chat_id).await.unwrap();
        assert_eq!(stopped.reason.as_deref(), Some("cancelled"));
    }
}

use std::{
    collections::HashMap,
    fs::OpenOptions,
    io::{Read, Seek, SeekFrom},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Mutex, broadcast, watch};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{
    DEFAULT_OUTPUT_BYTES, Operation, ToolDefinition, ToolError, io_error, open_existing,
    read::semantic_integer, write::semantic_boolean,
};
use tickloop_agent_base::private_fs::PrivateDirectory;

mod command_tables;
mod host;
pub use host::{ShellExit, ShellHost, ShellLaunch, ShellProcess, ShellStartError};
mod interpreter;
mod model_result;
mod path_constraints;
pub(super) use path_constraints::auto_edit_safety_check;
use tickloop_agent_protocol::{CommandSnapshot, CommandState, ToolResult};
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
/// How often a running command's output size is published.
const OUTPUT_POLL_INTERVAL: Duration = Duration::from_millis(40);
/// 单个任务输出文件上限。Claude Code 为 5GB；Tickloop 按工单 34 的决定保留 64 MiB，由运行命令的
/// Tickhost 执行。
pub(crate) const MAX_COMMAND_OUTPUT_BYTES: u64 = 64 * 1024 * 1024;
/// 命令被停止或随 Agent 关闭而终止时记录的原因。
pub const CANCELLED_COMMAND_REASON: &str = "cancelled";
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
    #[serde(skip)]
    pub(super) editable_directory: Option<PathBuf>,
    /// The working directory the command was prepared and approved for; it runs only in that
    /// same directory, never in one that replaced it at its path.
    #[serde(skip)]
    pub(super) approved_directory: Option<DirectoryIdentity>,
}

/// A directory as its device and inode, which a rename keeps and a replacement does not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct DirectoryIdentity {
    device: u64,
    inode: u64,
}

impl DirectoryIdentity {
    pub(super) fn of(metadata: &std::fs::Metadata) -> Option<Self> {
        metadata.is_dir().then(|| Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
}

impl ShellInput {
    /// Claude Code's rule-free read-only admission, evaluated with default-mode path checks.
    pub(super) fn is_read_only(&self, cwd: &Path, workspace: &super::Workspace) -> bool {
        let mut context = self.permission_context(cwd, workspace);
        context.editable_directory = None;
        permission::is_allowed_without_rules(&self.command, &context, false)
    }

    /// Admission through the family temp, which needs the canonical `cwd` inside the family temp
    /// or the workspace: elsewhere even a path-free read such as `git status` reads outside them.
    pub(super) fn is_allowed_with_family_temp(
        &self,
        cwd: &Path,
        workspace: &super::Workspace,
    ) -> bool {
        self.editable_directory
            .as_deref()
            .is_some_and(|family_temp| {
                (cwd.starts_with(family_temp) || workspace.contains(cwd))
                    && permission::is_allowed_without_rules(
                        &self.command,
                        &self.permission_context(cwd, workspace),
                        false,
                    )
            })
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
            working_directories: workspace
                .folder
                .iter()
                .chain(&workspace.additional_directories)
                .cloned()
                .collect(),
            readable_directories: &self.readable_directories,
            editable_directory: self.editable_directory.as_deref(),
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

    fn set_pid(&mut self, pid: u32) {
        match self {
            Self::Agent(snapshot) => snapshot.pid = Some(pid),
            Self::User(snapshot) => snapshot.pid = Some(pid),
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

    /// Records how the command ended, as its host reported it.
    fn finish(&mut self, exit: ShellExit) {
        let ShellExit {
            exit_code,
            signal,
            reason,
            state,
        } = exit;
        match self {
            Self::Agent(snapshot) => {
                snapshot.exit_code = exit_code;
                snapshot.signal = signal;
            }
            Self::User(snapshot) => {
                snapshot.exit_code = exit_code;
                snapshot.signal = signal;
            }
        }
        if let Some(reason) = reason {
            self.set_reason(reason);
        }
        self.update_output_bytes();
        self.mark_updated();
        // A command this Agent stopped never counts as completed, even when it ended first.
        let is_stopped = self.reason().is_some();
        self.set_state(match state {
            CommandState::Completed if is_stopped => CommandState::Failed,
            state => state,
        });
    }

    fn mark_updated(&mut self) {
        let now = Some(tickloop_agent_base::private_fs::unix_time_milliseconds());
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
    host: Arc<dyn ShellHost>,
}

impl Registry {
    pub(super) fn new(host: Arc<dyn ShellHost>) -> Self {
        Self::with_shell(interpreter::selected_shell(), host)
    }

    /// A Registry that cannot run commands.
    pub(super) fn without_host() -> Self {
        Self::new(Arc::new(host::NoShellHost))
    }

    /// The Registry of a Chat without a Folder: it runs no command and selects no shell, so it
    /// reads neither the process environment nor the file system.
    pub(super) fn without_folder() -> Self {
        Self::with_shell(
            Err(super::FOLDERLESS_REJECTION.into()),
            Arc::new(host::NoShellHost),
        )
    }

    fn with_shell(shell: Result<PathBuf, String>, host: Arc<dyn ShellHost>) -> Self {
        Self {
            entries: Arc::new(Mutex::new(HashMap::new())),
            events: broadcast::channel(256).0,
            user_events: broadcast::channel(256).0,
            shell: Arc::new(shell),
            host,
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
        input: ShellInput,
        task_output_dir: &PrivateDirectory,
        operation_id: Uuid,
        chat_id: Uuid,
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        self.execute_process(
            cwd,
            input,
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
        let opened = directory.metadata().map_err(|error| io_error(cwd, error))?;
        if input.approved_directory.is_none()
            || input.approved_directory != DirectoryIdentity::of(&opened)
        {
            return Err(ToolError::PathChanged(cwd.into()));
        }
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
        let initial_state = if input.run_in_background {
            CommandState::Background
        } else {
            CommandState::Running
        };
        // Listed from the start, without a process yet, so it can already be moved to the
        // background or stopped while the Tickhost starts it.
        let snapshot = match owner {
            CommandOwner::Agent {
                operation_id,
                loop_id,
            } => ProcessSnapshot::Agent(CommandSnapshot {
                updated_at_ms: Some(tickloop_agent_base::private_fs::unix_time_milliseconds()),
                operation_id,
                loop_id,
                command: input.command.clone(),
                description: input.description.clone(),
                cwd: cwd.into(),
                pid: None,
                state: initial_state.clone(),
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
                updated_at_ms: Some(tickloop_agent_base::private_fs::unix_time_milliseconds()),
                operation_id,
                chat_id,
                command: input.command.clone(),
                cwd: cwd.into(),
                pid: None,
                state: initial_state.clone(),
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
            snapshot,
            result: None,
        });
        // Stopping the command and cancelling its operation both cancel it.
        let command_cancel = cancel.child_token();
        self.entries.lock().await.insert(
            operation_id,
            Entry {
                updates,
                cancel: command_cancel.clone(),
                is_stopped_by_model: false,
            },
        );
        let started = self
            .host
            .start(ShellLaunch {
                operation_id,
                shell: shell.clone(),
                command: input.command.clone(),
                description: input.description.clone(),
                cwd: cwd.into(),
                directory,
                output,
                cancel: command_cancel.clone(),
            })
            .await;
        let foreground_timeout = input.foreground_timeout();
        let started_snapshot = match started {
            Ok(process) => {
                self.follow(
                    operation_id,
                    process,
                    command_cancel,
                    foreground_timeout,
                    allows_auto_background(&input.command),
                )
                .await
            }
            Err(ShellStartError::NotStarted(ToolError::Cancelled)) => {
                // Nothing was dispatched, so nothing ran; it ends as a command stopped before it
                // started, without a process, for whoever stopped it to see.
                let exit = ShellExit {
                    exit_code: None,
                    signal: None,
                    reason: Some(CANCELLED_COMMAND_REASON.into()),
                    state: CommandState::Failed,
                };
                self.end_unstarted(operation_id, exit, foreground_timeout)
                    .await
            }
            Err(ShellStartError::NotStarted(error)) => {
                self.forget_unstarted(operation_id, &initial_state, &error)
                    .await;
                return Err(error);
            }
            Err(ShellStartError::OutcomeUnknown(reason)) => {
                let exit = ShellExit {
                    exit_code: None,
                    signal: None,
                    reason: Some(reason),
                    state: CommandState::Unknown,
                };
                self.end_unstarted(operation_id, exit, foreground_timeout)
                    .await
            }
        };
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
                        !input.run_in_background && !command.backgrounded_on_timeout,
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

    /// Follows a command its host started until it ends, and returns it as it started: in the
    /// background or stopping when that was asked for while it started.
    async fn follow(
        &self,
        operation_id: Uuid,
        mut process: Box<dyn ShellProcess>,
        cancel: CancellationToken,
        foreground_timeout: Duration,
        moves_to_background_on_timeout: bool,
    ) -> ProcessSnapshot {
        let snapshot = {
            let entries = self.entries.lock().await;
            let entry = entries
                .get(&operation_id)
                .expect("a starting command stays listed");
            let mut update = entry.updates.borrow().clone();
            update.snapshot.set_pid(process.pid());
            entry.updates.send_replace(update.clone());
            update.snapshot
        };
        self.emit(&snapshot);
        let started_snapshot = snapshot.clone();
        let registry = self.clone();
        tokio::spawn(async move {
            let mut snapshot = snapshot;
            // Like Claude Code's `ShellCommand.background`, moving to the background ends the
            // foreground timeout.
            let mut is_foreground = snapshot.state() == &CommandState::Running;
            let timeout = tokio::time::sleep(foreground_timeout);
            tokio::pin!(timeout);
            let mut output_poll = tokio::time::interval(OUTPUT_POLL_INTERVAL);
            output_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            output_poll.tick().await;
            let exit = loop {
                tokio::select! {
                    exit = process.exited() => break exit,
                    _ = cancel.cancelled() => {
                        snapshot.set_reason(CANCELLED_COMMAND_REASON.into());
                        process.stop(CANCELLED_COMMAND_REASON).await;
                        break process.exited().await;
                    }
                    _ = &mut timeout, if is_foreground => {
                        if !moves_to_background_on_timeout {
                            snapshot.set_reason(TIMED_OUT_REASON.into());
                            process.stop(TIMED_OUT_REASON).await;
                            break process.exited().await;
                        }
                        is_foreground = false;
                        if let Some(shared_snapshot) = registry.background_on_timeout(operation_id).await {
                            snapshot.set_state(shared_snapshot.state().clone());
                            snapshot.merge_background_history(&shared_snapshot);
                        }
                    },
                    _ = output_poll.tick() => {
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
                        if snapshot.output_bytes() != previous_bytes {
                            if let Some(entry) = registry.entries.lock().await.get(&operation_id) {
                                entry.updates.send_replace(Update { snapshot: snapshot.clone(), result: None });
                            }
                            registry.emit(&snapshot);
                        }
                    }
                }
            };
            // The host forgets the command once its exit is recorded here.
            drop(process);
            registry
                .record_end(operation_id, snapshot, exit, foreground_timeout)
                .await;
        });
        started_snapshot
    }

    /// Records how a command ended and publishes it with the result it gives.
    async fn record_end(
        &self,
        operation_id: Uuid,
        mut snapshot: ProcessSnapshot,
        exit: ShellExit,
        foreground_timeout: Duration,
    ) -> ProcessSnapshot {
        snapshot.finish(exit);
        let result = match &snapshot {
            ProcessSnapshot::Agent(command) => model_result::finished(command, foreground_timeout),
            ProcessSnapshot::User(_) => process_output(&snapshot, 0, DEFAULT_OUTPUT_BYTES)
                .unwrap_or_else(|error| ToolResult {
                    content: error.to_string(),
                    is_error: true,
                    output_ref: Some(snapshot.output_ref().into()),
                }),
        };
        if let Some(entry) = self.entries.lock().await.get(&operation_id) {
            snapshot.merge_background_history(&entry.updates.borrow().snapshot);
            entry.updates.send_replace(Update {
                snapshot: snapshot.clone(),
                result: Some(result),
            });
        }
        self.emit(&snapshot);
        snapshot
    }

    /// Ends a listed command its host did not start as asked, without a process.
    async fn end_unstarted(
        &self,
        operation_id: Uuid,
        exit: ShellExit,
        foreground_timeout: Duration,
    ) -> ProcessSnapshot {
        let snapshot = self
            .entries
            .lock()
            .await
            .get(&operation_id)
            .expect("a starting command stays listed")
            .updates
            .borrow()
            .snapshot
            .clone();
        self.record_end(operation_id, snapshot, exit, foreground_timeout)
            .await
    }

    /// Forgets a command its host refused to start, like one refused before it was listed. A stop
    /// or move to the background published it while it started; it is then published once more,
    /// ended for `error`, which also answers whoever waits for its end.
    async fn forget_unstarted(
        &self,
        operation_id: Uuid,
        initial_state: &CommandState,
        error: &ToolError,
    ) {
        let Some(entry) = self.entries.lock().await.remove(&operation_id) else {
            return;
        };
        let mut update = entry.updates.borrow().clone();
        if update.snapshot.state() == initial_state {
            return;
        }
        update.snapshot.finish(ShellExit {
            exit_code: None,
            signal: None,
            reason: Some(error.to_string()),
            state: CommandState::Failed,
        });
        update.result = Some(ToolResult {
            content: error.to_string(),
            is_error: true,
            output_ref: Some(update.snapshot.output_ref().into()),
        });
        entry.updates.send_replace(update.clone());
        self.emit(&update.snapshot);
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

#[cfg(test)]
mod tests {
    use std::{io::Write, time::Duration};

    use tokio::sync::broadcast;

    use super::*;

    /// A host whose commands end when a test says so, or when they are stopped.
    #[derive(Clone, Default)]
    struct ScriptedHost {
        launches: Arc<std::sync::Mutex<Vec<Launched>>>,
        started: Arc<tokio::sync::Notify>,
        /// Held by a test to keep commands starting.
        starting: Arc<Mutex<()>>,
        /// Set by a test to lose the answer to every start after dispatching it.
        loses_answers: Arc<std::sync::atomic::AtomicBool>,
    }

    const LOST_ANSWER: &str = "the host's answer was lost; the command may have run";

    struct Launched {
        operation_id: Uuid,
        shell: PathBuf,
        command: String,
        directory: Option<DirectoryIdentity>,
        output: std::fs::File,
        exit: watch::Sender<Option<ShellExit>>,
        stops: Arc<std::sync::Mutex<Vec<String>>>,
    }

    struct ScriptedProcess {
        exit: watch::Receiver<Option<ShellExit>>,
        ends: watch::Sender<Option<ShellExit>>,
        stops: Arc<std::sync::Mutex<Vec<String>>>,
    }

    /// How a shell reports a command SIGTERM ended.
    const TERMINATED_EXIT_CODE: i32 = 143;

    #[async_trait::async_trait]
    impl ShellHost for ScriptedHost {
        async fn start(
            &self,
            launch: ShellLaunch,
        ) -> Result<Box<dyn ShellProcess>, ShellStartError> {
            drop(self.starting.lock().await);
            // As the Tickhost's spawns, a launch cancelled while it waited is never dispatched.
            if launch.cancel.is_cancelled() {
                return Err(ShellStartError::NotStarted(ToolError::Cancelled));
            }
            if self.loses_answers.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(ShellStartError::OutcomeUnknown(LOST_ANSWER.into()));
            }
            let exit = watch::channel(None).0;
            let stops = Arc::default();
            let process = ScriptedProcess {
                exit: exit.subscribe(),
                ends: exit.clone(),
                stops: Arc::clone(&stops),
            };
            self.launches.lock().unwrap().push(Launched {
                operation_id: launch.operation_id,
                shell: launch.shell,
                command: launch.command,
                directory: DirectoryIdentity::of(&launch.directory.metadata().unwrap()),
                output: launch.output,
                exit,
                stops,
            });
            self.started.notify_waiters();
            Ok(Box::new(process))
        }
    }

    #[async_trait::async_trait]
    impl ShellProcess for ScriptedProcess {
        fn pid(&self) -> u32 {
            4242
        }

        async fn exited(&mut self) -> ShellExit {
            self.exit
                .wait_for(Option::is_some)
                .await
                .unwrap()
                .clone()
                .unwrap()
        }

        async fn stop(&mut self, reason: &str) {
            self.stops.lock().unwrap().push(reason.into());
            self.ends.send_if_modified(|exit| {
                let is_running = exit.is_none();
                if is_running {
                    *exit = Some(ShellExit {
                        exit_code: Some(TERMINATED_EXIT_CODE),
                        signal: None,
                        reason: Some(reason.into()),
                        state: CommandState::Failed,
                    });
                }
                is_running
            });
        }
    }

    impl ScriptedHost {
        fn registry(&self) -> Registry {
            Registry::with_shell(Ok("/bin/zsh".into()), Arc::new(self.clone()))
        }

        async fn launched(&self, operation_id: Uuid) {
            loop {
                let started = self.started.notified();
                if self
                    .launches
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|launch| launch.operation_id == operation_id)
                {
                    return;
                }
                started.await;
            }
        }

        fn with_launch<T>(&self, operation_id: Uuid, read: impl FnOnce(&Launched) -> T) -> T {
            let launches = self.launches.lock().unwrap();
            read(
                launches
                    .iter()
                    .find(|launch| launch.operation_id == operation_id)
                    .expect("the command was launched"),
            )
        }

        /// Ends a command after it wrote `output`.
        fn finish(&self, operation_id: Uuid, output: &str, exit: ShellExit) {
            self.with_launch(operation_id, |launch| {
                (&launch.output).write_all(output.as_bytes()).unwrap();
                launch.exit.send_replace(Some(exit));
            });
        }

        fn stops(&self, operation_id: Uuid) -> Vec<String> {
            self.with_launch(operation_id, |launch| launch.stops.lock().unwrap().clone())
        }

        fn is_launched(&self, operation_id: Uuid) -> bool {
            self.launches
                .lock()
                .unwrap()
                .iter()
                .any(|launch| launch.operation_id == operation_id)
        }
    }

    fn is_terminal(state: &CommandState) -> bool {
        matches!(
            state,
            CommandState::Completed | CommandState::Failed | CommandState::Unknown
        )
    }

    fn exited_with(exit_code: i32) -> ShellExit {
        ShellExit {
            exit_code: Some(exit_code),
            signal: None,
            reason: None,
            state: if exit_code == 0 {
                CommandState::Completed
            } else {
                CommandState::Failed
            },
        }
    }

    struct Workdir {
        _directory: tempfile::TempDir,
        path: PathBuf,
    }

    impl Workdir {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().canonicalize().unwrap();
            Self {
                _directory: directory,
                path,
            }
        }

        fn output(&self) -> PrivateDirectory {
            PrivateDirectory::trusted(self.path.join("output"))
        }

        /// A command approved for this directory.
        fn input(&self, command: &str) -> ShellInput {
            ShellInput {
                command: command.into(),
                timeout: None,
                description: None,
                run_in_background: false,
                cwd: None,
                readable_directories: Vec::new(),
                editable_directory: None,
                approved_directory: DirectoryIdentity::of(&std::fs::metadata(&self.path).unwrap()),
            }
        }
    }

    fn agent_command(
        registry: &Registry,
        workdir: &Workdir,
        input: ShellInput,
        operation_id: Uuid,
    ) -> tokio::task::JoinHandle<Result<ToolResult, ToolError>> {
        let registry = registry.clone();
        let cwd = workdir.path.clone();
        let output = workdir.output();
        tokio::spawn(async move {
            registry
                .execute(
                    &cwd,
                    input,
                    &output,
                    operation_id,
                    Uuid::new_v4(),
                    CancellationToken::new(),
                )
                .await
        })
    }

    #[tokio::test]
    async fn a_command_starts_in_its_approved_directory_with_the_selected_shell() {
        let host = ScriptedHost::default();
        let registry = host.registry();
        let workdir = Workdir::new();
        let operation_id = Uuid::new_v4();
        let execution = agent_command(&registry, &workdir, workdir.input("pwd"), operation_id);
        host.launched(operation_id).await;
        host.with_launch(operation_id, |launch| {
            assert_eq!(launch.shell, Path::new("/bin/zsh"));
            assert_eq!(launch.command, "pwd");
            assert_eq!(
                launch.directory,
                DirectoryIdentity::of(&std::fs::metadata(&workdir.path).unwrap())
            );
        });
        host.finish(operation_id, "done", exited_with(0));
        let result = execution.await.unwrap().unwrap();
        assert_eq!(result.content, "done");
        assert_eq!(
            result.output_ref.unwrap(),
            workdir
                .path
                .join("output")
                .join(format!("{operation_id}.output"))
        );
    }

    #[tokio::test]
    async fn a_missing_shell_explains_that_bash_or_zsh_is_required() {
        let host = ScriptedHost::default();
        let workdir = Workdir::new();
        let registry = Registry::with_shell(
            Err(interpreter::NO_SUITABLE_SHELL.into()),
            Arc::new(host.clone()),
        );
        let error = agent_command(&registry, &workdir, workdir.input("pwd"), Uuid::new_v4())
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(error.to_string(), interpreter::NO_SUITABLE_SHELL);
        assert!(!workdir.path.join("output").exists());
        assert!(host.launches.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_directory_replaced_after_approval_is_refused_before_anything_starts() {
        let host = ScriptedHost::default();
        let registry = host.registry();
        let workdir = Workdir::new();
        let approved = workdir.path.join("approved");
        std::fs::create_dir(&approved).unwrap();
        let mut input = workdir.input("touch probe");
        input.approved_directory = DirectoryIdentity::of(&std::fs::metadata(&approved).unwrap());
        std::fs::rename(&approved, workdir.path.join("moved")).unwrap();
        std::fs::create_dir(&approved).unwrap();
        let error = registry
            .execute(
                &approved,
                input,
                &workdir.output(),
                Uuid::new_v4(),
                Uuid::new_v4(),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::PathChanged(_)), "{error:?}");
        assert!(host.launches.lock().unwrap().is_empty());
        assert!(!workdir.path.join("output").exists());
    }

    #[tokio::test]
    async fn the_timeout_stops_a_foreground_sleep_but_not_a_background_command() {
        let host = ScriptedHost::default();
        let registry = host.registry();
        let workdir = Workdir::new();
        let sleeping = Uuid::new_v4();
        let mut foreground = workdir.input("sleep 5; printf never");
        foreground.timeout = Some(100);
        let result = agent_command(&registry, &workdir, foreground, sleeping)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.content, "Exit code 143\nCommand timed out after 0s");
        assert!(result.is_error);
        assert_eq!(host.stops(sleeping), [TIMED_OUT_REASON]);

        let operation_id = Uuid::new_v4();
        let mut background = workdir.input("sleep 0.3; printf finished");
        background.timeout = Some(100);
        background.run_in_background = true;
        let started = agent_command(&registry, &workdir, background, operation_id)
            .await
            .unwrap()
            .unwrap();
        assert!(started.content.starts_with(&format!(
            "Command running in background with ID: {operation_id}. Output is being written to: "
        )));
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(host.stops(operation_id).is_empty());
        let mut updates = registry.subscribe();
        host.finish(operation_id, "finished", exited_with(0));
        let finished = wait_for_terminal_agent_snapshot(&mut updates, operation_id).await;
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
                if snapshot.operation_id == operation_id && is_terminal(&snapshot.state) {
                    break snapshot;
                }
            }
        })
        .await
        .expect("agent command did not reach a terminal state")
    }

    #[tokio::test]
    async fn a_foreground_command_that_outlives_its_timeout_keeps_running_in_the_background() {
        let host = ScriptedHost::default();
        let registry = host.registry();
        let workdir = Workdir::new();
        let mut updates = registry.subscribe();
        let operation_id = Uuid::new_v4();
        let mut input = workdir.input("while [ ! -e release ]; do sleep 0.02; done");
        input.timeout = Some(100);
        let result = agent_command(&registry, &workdir, input, operation_id)
            .await
            .unwrap()
            .unwrap();
        let output_ref = workdir
            .path
            .join("output")
            .join(format!("{operation_id}.output"));
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
        assert!(host.stops(operation_id).is_empty());

        host.finish(operation_id, "started finished", exited_with(0));
        let finished = wait_for_terminal_agent_snapshot(&mut updates, operation_id).await;
        assert_eq!(finished.state, CommandState::Completed);
        assert!(finished.was_background && finished.backgrounded_on_timeout);
        assert_eq!(finished.output_bytes, "started finished".len() as u64);
        assert_eq!(
            registry
                .finished_result(operation_id)
                .await
                .unwrap()
                .content,
            "started finished"
        );
    }

    async fn user_command(
        registry: &Registry,
        workdir: &Workdir,
        input: ShellInput,
        operation_id: Uuid,
        chat_id: Uuid,
    ) {
        registry
            .execute_user(
                &workdir.path,
                input,
                &workdir.output(),
                operation_id,
                chat_id,
                CancellationToken::new(),
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_user_command_that_outlives_its_timeout_moves_to_the_background_too() {
        let host = ScriptedHost::default();
        let registry = host.registry();
        let workdir = Workdir::new();
        let mut updates = registry.subscribe_user();
        let operation_id = Uuid::new_v4();
        let chat_id = Uuid::new_v4();
        let mut input = workdir.input("while :; do sleep 0.02; done");
        input.timeout = Some(100);
        user_command(&registry, &workdir, input, operation_id, chat_id).await;
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
        assert_eq!(stopped.state, CommandState::Failed);
        assert!(stopped.backgrounded_on_timeout);
        assert_eq!(host.stops(operation_id), [CANCELLED_COMMAND_REASON]);
    }

    #[tokio::test]
    async fn the_hosts_report_decides_how_a_command_ended() {
        let host = ScriptedHost::default();
        let registry = host.registry();
        let workdir = Workdir::new();
        let chat_id = Uuid::new_v4();
        let mut updates = registry.subscribe_user();
        for (exit, expected_state) in [
            (
                ShellExit {
                    exit_code: Some(0),
                    signal: None,
                    reason: Some(
                        "shell exited while its process group still contains descendants".into(),
                    ),
                    state: CommandState::Unknown,
                },
                CommandState::Unknown,
            ),
            (
                ShellExit {
                    exit_code: None,
                    signal: Some(15),
                    reason: Some(OUTPUT_LIMIT_REASON.into()),
                    state: CommandState::Failed,
                },
                CommandState::Failed,
            ),
        ] {
            let operation_id = Uuid::new_v4();
            user_command(
                &registry,
                &workdir,
                workdir.input("true"),
                operation_id,
                chat_id,
            )
            .await;
            host.launched(operation_id).await;
            host.finish(operation_id, "partial", exit.clone());
            let finished = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let snapshot = updates.recv().await.unwrap();
                    if snapshot.operation_id == operation_id && is_terminal(&snapshot.state) {
                        break snapshot;
                    }
                }
            })
            .await
            .unwrap();
            assert_eq!(finished.state, expected_state);
            assert_eq!(finished.reason, exit.reason);
            assert_eq!(finished.output_bytes, "partial".len() as u64);
            let read = registry
                .read_user(operation_id, chat_id, 0, DEFAULT_OUTPUT_BYTES)
                .await
                .unwrap();
            let value: Value = serde_json::from_str(&read.content).unwrap();
            assert_eq!(value["reason"], json!(exit.reason));
            assert!(read.is_error);
        }
    }

    #[tokio::test]
    async fn a_starting_command_can_move_to_the_background_but_one_stopped_meanwhile_never_starts()
    {
        let host = ScriptedHost::default();
        let registry = host.registry();
        let workdir = Workdir::new();
        let chat_id = Uuid::new_v4();
        let mut updates = registry.subscribe_user();
        let starting = host.starting.lock().await;
        let backgrounded = Uuid::new_v4();
        let stopped = Uuid::new_v4();
        for operation_id in [backgrounded, stopped] {
            let registry = registry.clone();
            let input = workdir.input("sleep 30");
            let cwd = workdir.path.clone();
            let output = workdir.output();
            tokio::spawn(async move {
                registry
                    .execute_user(
                        &cwd,
                        input,
                        &output,
                        operation_id,
                        chat_id,
                        CancellationToken::new(),
                    )
                    .await
            });
        }
        let listed = |operation_id| {
            let registry = registry.clone();
            async move {
                loop {
                    if let Some(snapshot) = registry
                        .user_snapshots()
                        .await
                        .into_iter()
                        .find(|snapshot| snapshot.operation_id == operation_id)
                    {
                        return snapshot;
                    }
                    tokio::task::yield_now().await;
                }
            }
        };
        assert_eq!(listed(backgrounded).await.pid, None);
        listed(stopped).await;
        let moved = registry
            .background_user(backgrounded, chat_id)
            .await
            .unwrap();
        assert_eq!(moved.state, CommandState::Background);
        let stopping = tokio::spawn({
            let registry = registry.clone();
            async move { registry.stop_user(stopped, chat_id).await }
        });
        drop(starting);
        let stopped_snapshot = stopping.await.unwrap().unwrap();
        assert_eq!(
            (
                stopped_snapshot.state,
                stopped_snapshot.reason.as_deref(),
                stopped_snapshot.pid
            ),
            (CommandState::Failed, Some(CANCELLED_COMMAND_REASON), None)
        );
        let published_end = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let snapshot = updates.recv().await.unwrap();
                if snapshot.operation_id == stopped && is_terminal(&snapshot.state) {
                    break snapshot;
                }
            }
        })
        .await
        .expect("the stop's published command ends");
        assert_eq!(
            (published_end.state, published_end.pid),
            (CommandState::Failed, None)
        );
        assert!(!host.is_launched(stopped));
        let running = loop {
            let snapshot = user_snapshot(&registry, backgrounded).await;
            if snapshot.pid.is_some() {
                break snapshot;
            }
            tokio::task::yield_now().await;
        };
        assert_eq!(running.state, CommandState::Background);
        assert_eq!(running.pid, Some(4242));
        assert!(host.stops(backgrounded).is_empty());
    }

    #[tokio::test]
    async fn a_command_whose_operation_is_cancelled_while_its_host_starts_it_never_starts() {
        let host = ScriptedHost::default();
        let registry = host.registry();
        let workdir = Workdir::new();
        let operation_id = Uuid::new_v4();
        let cancel = CancellationToken::new();
        let starting = host.starting.lock().await;
        let execution = tokio::spawn({
            let registry = registry.clone();
            let input = workdir.input("touch probe");
            let cwd = workdir.path.clone();
            let output = workdir.output();
            let cancel = cancel.clone();
            async move {
                registry
                    .execute(&cwd, input, &output, operation_id, Uuid::new_v4(), cancel)
                    .await
            }
        });
        while registry.state(operation_id).await.is_none() {
            tokio::task::yield_now().await;
        }
        cancel.cancel();
        drop(starting);
        let result = execution.await.unwrap().unwrap();
        assert!(result.is_error);
        assert!(
            result.content.contains("aborted before completion"),
            "{}",
            result.content
        );
        assert!(!host.is_launched(operation_id));
        let ended = registry
            .snapshots()
            .await
            .into_iter()
            .find(|snapshot| snapshot.operation_id == operation_id)
            .unwrap();
        assert_eq!(
            (ended.state, ended.reason.as_deref(), ended.pid),
            (CommandState::Failed, Some(CANCELLED_COMMAND_REASON), None)
        );
        // A stop that follows the cancel, as a user's stop of a `!` command does, finds it ended.
        let stopped = registry.stop(operation_id).await.unwrap();
        assert_eq!((stopped.state, stopped.pid), (CommandState::Failed, None));
    }

    #[tokio::test]
    async fn a_start_whose_answer_was_lost_stays_listed_with_an_unknown_outcome() {
        let host = ScriptedHost::default();
        host.loses_answers
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let registry = host.registry();
        let workdir = Workdir::new();
        let mut updates = registry.subscribe();
        let operation_id = Uuid::new_v4();
        let result = agent_command(
            &registry,
            &workdir,
            workdir.input("touch probe"),
            operation_id,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(result.is_error);
        assert!(result.content.contains(LOST_ANSWER), "{}", result.content);
        let ended = wait_for_terminal_agent_snapshot(&mut updates, operation_id).await;
        assert_eq!(
            (ended.state, ended.reason.as_deref(), ended.pid),
            (CommandState::Unknown, Some(LOST_ANSWER), None)
        );
        assert_eq!(
            registry.state(operation_id).await,
            Some(CommandState::Unknown)
        );
        assert_eq!(
            registry
                .finished_result(operation_id)
                .await
                .map(|finished| finished.content),
            Some(result.content)
        );
    }

    #[test]
    fn a_stopped_command_that_finished_first_still_counts_as_stopped() {
        let workdir = Workdir::new();
        let mut snapshot = ProcessSnapshot::User(UserCommandSnapshot {
            operation_id: Uuid::new_v4(),
            chat_id: Uuid::new_v4(),
            command: "true".into(),
            cwd: workdir.path.clone(),
            pid: Some(1),
            state: CommandState::Cancelling,
            was_background: false,
            backgrounded_on_timeout: false,
            exit_code: None,
            signal: None,
            reason: Some(CANCELLED_COMMAND_REASON.into()),
            output_ref: workdir.path.join("missing.output"),
            output_bytes: 0,
            updated_at_ms: None,
        });
        snapshot.finish(exited_with(0));
        assert_eq!(snapshot.state(), &CommandState::Failed);
        assert_eq!(snapshot.reason(), Some(CANCELLED_COMMAND_REASON));
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
        let workdir = Workdir::new();
        let timeout = |requested| {
            let mut input = workdir.input("true");
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
        let workdir = Workdir::new();
        let cwd = workdir.path.clone();
        let workspace = super::super::Workspace {
            folder: Some(cwd.clone()),
            additional_directories: Vec::new(),
        };
        let read_only = |command: &str| workdir.input(command).is_read_only(&cwd, &workspace);
        let accept_edits = |command: &str| {
            workdir
                .input(command)
                .is_allowed_in_accept_edits(&cwd, &workspace)
        };

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
        let workdir = Workdir::new();
        let cwd = workdir.path.clone();
        let internal = tempfile::tempdir().unwrap();
        let internal = internal.path().canonicalize().unwrap();
        let workspace = super::super::Workspace {
            folder: Some(cwd.clone()),
            additional_directories: Vec::new(),
        };
        let mut input = workdir.input(&format!("cat {}/task.output", internal.display()));
        assert!(!input.is_read_only(&cwd, &workspace));
        input.readable_directories = vec![internal.clone()];
        assert!(input.is_read_only(&cwd, &workspace));
        input.command = format!("touch {}/task.output", internal.display());
        assert!(!input.is_allowed_in_accept_edits(&cwd, &workspace));
    }

    async fn wait_for_terminal_user_snapshot(
        updates: &mut broadcast::Receiver<UserCommandSnapshot>,
        operation_id: Uuid,
    ) -> UserCommandSnapshot {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let snapshot = updates.recv().await.unwrap();
                if snapshot.operation_id == operation_id && is_terminal(&snapshot.state) {
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
        let host = ScriptedHost::default();
        let registry = host.registry();
        let workdir = Workdir::new();
        let mut updates = registry.subscribe();
        let operation_id = Uuid::new_v4();
        let execution = agent_command(
            &registry,
            &workdir,
            workdir.input("sleep 0.1"),
            operation_id,
        );
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
        execution.await.unwrap().unwrap();
        host.finish(operation_id, "", exited_with(0));
        let terminal = wait_for_terminal_agent_snapshot(&mut updates, operation_id).await;
        assert!(terminal.was_background);
    }

    #[tokio::test]
    async fn user_snapshot_keeps_explicit_background_history_through_completion() {
        let host = ScriptedHost::default();
        let registry = host.registry();
        let workdir = Workdir::new();
        let mut updates = registry.subscribe_user();
        let operation_id = Uuid::new_v4();
        let chat_id = Uuid::new_v4();
        user_command(
            &registry,
            &workdir,
            workdir.input("sleep 0.1"),
            operation_id,
            chat_id,
        )
        .await;
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
        host.finish(operation_id, "", exited_with(0));
        let terminal = wait_for_terminal_user_snapshot(&mut updates, operation_id).await;
        assert!(terminal.was_background);
    }

    #[tokio::test]
    async fn wrong_owner_endpoints_do_not_change_a_user_shell_state() {
        let host = ScriptedHost::default();
        let registry = host.registry();
        let workdir = Workdir::new();
        let operation_id = Uuid::new_v4();
        let chat_id = Uuid::new_v4();
        user_command(
            &registry,
            &workdir,
            workdir.input("sleep 30"),
            operation_id,
            chat_id,
        )
        .await;

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
        assert!(host.stops(operation_id).is_empty());

        let stopped = registry.stop_user(operation_id, chat_id).await.unwrap();
        assert_eq!(stopped.reason.as_deref(), Some("cancelled"));
    }
}

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::Duration,
};

use tick_protocol::ShellTaskId;
use uuid::Uuid;

use crate::{ChatId, LoopId};

pub const SHELL_TASK_OUTPUT_MAX_BYTES: usize = 65_536;

pub const SHELL_TASK_PROMPT_QUIET_PERIOD: Duration = Duration::from_secs(45);

/// How long a finished background Shell keeps its Monitor row once its Chat handled its end.
pub const FINISHED_SHELL_DISPLAY_MS: u64 = 30_000;

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ShellTaskOrigin {
    Agent,
    User,
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ShellTaskOwner {
    Agent { loop_id: LoopId },
    User { chat_id: ChatId },
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ShellTaskState {
    Queued,
    WaitingPermission,
    Running,
    Background,
    Cancelling,
    Completed,
    Failed,
    Unknown,
}

impl ShellTaskState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Unknown)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ShellTaskCapabilities {
    pub can_background: bool,
    pub can_stop: bool,
    pub can_read_output: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ShellTask {
    pub id: Uuid,
    /// 任务所属的 Chat。
    pub chat_id: ChatId,
    pub origin: ShellTaskOrigin,
    pub owner: ShellTaskOwner,
    /// The model's Bash call that started an Agent task; a user `!` task has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    pub command: String,
    /// What the command does: the Bash call's description, or one generated once the task moved
    /// to background without it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub cwd: PathBuf,
    pub state: ShellTaskState,
    pub was_background: bool,
    pub pid: Option<u32>,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub reason: Option<String>,
    pub output_bytes: u64,
    #[serde(default)]
    pub updated_at_ms: Option<u64>,
    /// When the task ended, in Unix milliseconds. Unlike `updated_at_ms`, later output-file or
    /// record changes never move it, so it dates the end a Monitor row reports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at_ms: Option<u64>,
    pub capabilities: ShellTaskCapabilities,
}

impl ShellTask {
    /// The short state a Monitor row and a Shell observation report: running, its exit or signal,
    /// stopped, or unknown.
    pub fn status_label(&self) -> String {
        if self.state == ShellTaskState::Unknown {
            "unknown".into()
        } else if !self.state.is_terminal() {
            "running".into()
        } else if let Some(signal) = self.signal {
            format!("signal {signal}")
        } else if let Some(exit) = self.exit_code {
            format!("exit {exit}")
        } else if self.reason.is_some() {
            "stopped".into()
        } else {
            "unknown".into()
        }
    }

    /// Whether its Chat's Monitor lists it by itself, apart from a Loop subscribing to it: a task
    /// that went to the background while it runs and, once it ended, until
    /// [`FINISHED_SHELL_DISPLAY_MS`] after the Chat handled its end at `handled_at`.
    pub fn is_listed_in_monitor(&self, handled_at: Option<u64>, now_ms: u64) -> bool {
        self.went_to_background()
            && self
                .finished_row_until(handled_at)
                .is_none_or(|until| until > now_ms)
    }

    pub fn went_to_background(&self) -> bool {
        self.was_background || self.state == ShellTaskState::Background
    }

    /// Until when a finished task keeps its Monitor row: [`FINISHED_SHELL_DISPLAY_MS`] after its
    /// Chat handled its end at `handled_at`. `None` while it runs or while its end is unread, so
    /// it stays.
    pub fn finished_row_until(&self, handled_at: Option<u64>) -> Option<u64> {
        let handled = handled_at?;
        (self.state.is_terminal() && self.ended_at().is_none_or(|ended| handled >= ended))
            .then(|| handled.saturating_add(FINISHED_SHELL_DISPLAY_MS))
    }

    /// When it ended; `None` while it runs.
    pub fn ended_at(&self) -> Option<u64> {
        self.state
            .is_terminal()
            .then(|| self.ended_at_ms.or(self.updated_at_ms))
            .flatten()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ShellTaskSnapshot {
    pub chat_id: ChatId,
    pub live_revision: u64,
    pub tasks: Vec<ShellTask>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ShellTaskOutputChunk {
    pub task_id: Uuid,
    pub offset: u64,
    pub confirmed_offset: u64,
    pub total_bytes: u64,
    pub next_offset: Option<u64>,
    pub truncated: bool,
    pub encoding: String,
    pub output: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ShellTaskAction {
    Background,
    Stop,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ShellTaskDelta {
    pub live_revision: u64,
    pub task: ShellTask,
    pub previous_state: Option<ShellTaskState>,
    pub previous_output_bytes: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ShellTaskSubscriptionEvent {
    Changed(Box<ShellTaskDelta>),
    ResyncRequired { last_live_revision: u64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShellTaskNotice {
    InteractivePrompt {
        task_id: Uuid,
        origin: ShellTaskOrigin,
        owner: ShellTaskOwner,
        quiet_for: Duration,
    },
}

pub struct ShellTaskPromptWatchdog {
    quiet_period: Duration,
    observations: HashMap<Uuid, PromptObservation>,
}

struct PromptObservation {
    output_bytes: u64,
    last_growth_at: Duration,
    reminded: bool,
}

impl Default for ShellTaskPromptWatchdog {
    fn default() -> Self {
        Self::new(SHELL_TASK_PROMPT_QUIET_PERIOD)
    }
}

impl ShellTaskPromptWatchdog {
    pub fn new(quiet_period: Duration) -> Self {
        Self {
            quiet_period,
            observations: HashMap::new(),
        }
    }

    pub fn observe(
        &mut self,
        task: &ShellTask,
        trailing_output: &str,
        now: Duration,
    ) -> Option<ShellTaskNotice> {
        if task.state != ShellTaskState::Background {
            self.observations.remove(&task.id);
            return None;
        }
        let observation = self
            .observations
            .entry(task.id)
            .or_insert(PromptObservation {
                output_bytes: task.output_bytes,
                last_growth_at: now,
                reminded: false,
            });
        if observation.output_bytes != task.output_bytes {
            observation.output_bytes = task.output_bytes;
            observation.last_growth_at = now;
            observation.reminded = false;
            return None;
        }
        let quiet_for = now.saturating_sub(observation.last_growth_at);
        if observation.reminded
            || quiet_for < self.quiet_period
            || !looks_like_interactive_prompt(trailing_output)
        {
            return None;
        }
        observation.reminded = true;
        Some(ShellTaskNotice::InteractivePrompt {
            task_id: task.id,
            origin: task.origin,
            owner: task.owner,
            quiet_for,
        })
    }

    pub fn remove(&mut self, task_id: Uuid) {
        self.observations.remove(&task_id);
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ShellTaskCenterError {
    #[error("Chat {} does not exist", chat_id.public())]
    ChatNotFound { chat_id: ChatId },
    #[error(
        "Shell task {} does not exist in Chat {}",
        ShellTaskId::from(*task_id).public(),
        chat_id.public()
    )]
    TaskNotFound { chat_id: ChatId, task_id: Uuid },
    #[error("{action:?} is unavailable for Shell task {}", ShellTaskId::from(*task_id).public())]
    ActionUnavailable {
        task_id: Uuid,
        action: ShellTaskAction,
    },
    #[error("output is unavailable for Shell task {}", ShellTaskId::from(*task_id).public())]
    OutputUnavailable { task_id: Uuid },
    #[error("invalid output read for Shell task {}: {reason}", ShellTaskId::from(*task_id).public())]
    InvalidOutputRead { task_id: Uuid, reason: String },
    #[error("cannot read output for Shell task {}: {reason}", ShellTaskId::from(*task_id).public())]
    OutputReadFailed { task_id: Uuid, reason: String },
    #[error("Shell task output response is invalid: {0}")]
    InvalidOutput(String),
    #[error("Shell task runtime failed: {0}")]
    Runtime(String),
    #[error("Shell task event stream closed")]
    EventStreamClosed,
}

pub fn recover_shell_update_time(
    updated: Option<u64>,
    state_since: Option<u64>,
    output: Option<&Path>,
) -> Option<u64> {
    let output_time = output.and_then(|path| {
        std::fs::metadata(path)
            .ok()?
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_millis()
            .try_into()
            .ok()
    });
    updated
        .into_iter()
        .chain(state_since)
        .chain(output_time)
        .max()
}

fn looks_like_interactive_prompt(output: &str) -> bool {
    let visible_line = terminal_visible_last_line(output);
    let prompt = visible_line.trim();
    if prompt.is_empty() {
        return false;
    }
    let lowercase = prompt.to_lowercase();
    let starts_with_input_request = [
        "enter ",
        "please enter ",
        "input:",
        "select ",
        "choose ",
        "choice:",
        "your choice:",
        "username:",
        "username for ",
        "login:",
        "password:",
        "password for ",
        "passphrase:",
        "passphrase for ",
        "token:",
        "pin:",
    ]
    .iter()
    .any(|prefix| lowercase.starts_with(prefix));
    let contains_credential_request = [" password for ", " passphrase for "]
        .iter()
        .any(|fragment| lowercase.contains(fragment));
    let requests_colon_terminated_input =
        prompt.ends_with(':') && (starts_with_input_request || contains_credential_request);
    prompt.ends_with(['?', '>', '$', '#'])
        || lowercase.contains("[y/n]")
        || lowercase.contains("[y/n/a]")
        || lowercase.contains("(y/n)")
        || lowercase.contains("press enter")
        || requests_colon_terminated_input
}

fn terminal_visible_last_line(output: &str) -> String {
    enum EscapeState {
        Ground,
        Escape,
        Csi,
        Osc,
        OscEscape,
    }

    let mut state = EscapeState::Ground;
    let mut line = String::new();
    for character in output.chars() {
        match state {
            EscapeState::Ground => match character {
                '\u{1b}' => state = EscapeState::Escape,
                '\n' | '\r' => line.clear(),
                '\u{8}' => {
                    line.pop();
                }
                '\t' => line.push(' '),
                character if !character.is_control() => line.push(character),
                _ => {}
            },
            EscapeState::Escape => match character {
                '[' => state = EscapeState::Csi,
                ']' => state = EscapeState::Osc,
                _ => state = EscapeState::Ground,
            },
            EscapeState::Csi => {
                if ('@'..='~').contains(&character) {
                    state = EscapeState::Ground;
                }
            }
            EscapeState::Osc => match character {
                '\u{7}' => state = EscapeState::Ground,
                '\u{1b}' => state = EscapeState::OscEscape,
                _ => {}
            },
            EscapeState::OscEscape => {
                state = if character == '\\' {
                    EscapeState::Ground
                } else {
                    EscapeState::Osc
                };
            }
        }
    }
    line
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use uuid::Uuid;

    use super::*;

    #[test]
    fn prompt_watchdog_notifies_once_after_quiet_period() {
        let task = background_task(12);
        let mut watchdog = ShellTaskPromptWatchdog::default();
        assert_eq!(watchdog.observe(&task, "Password: ", Duration::ZERO), None);
        assert_eq!(
            watchdog.observe(
                &task,
                "\u{1b}[33mPassword:\u{1b}[0m ",
                Duration::from_secs(44)
            ),
            None
        );
        assert!(matches!(
            watchdog.observe(
                &task,
                "\u{1b}[33mPassword:\u{1b}[0m ",
                Duration::from_secs(45)
            ),
            Some(ShellTaskNotice::InteractivePrompt { task_id, .. }) if task_id == task.id
        ));
        assert_eq!(
            watchdog.observe(&task, "Password: ", Duration::from_secs(90)),
            None
        );
    }

    #[test]
    fn output_growth_resets_prompt_watchdog_without_notifying() {
        let mut task = background_task(12);
        let mut watchdog = ShellTaskPromptWatchdog::default();
        assert_eq!(watchdog.observe(&task, "$ ", Duration::ZERO), None);
        task.output_bytes = 18;
        assert_eq!(watchdog.observe(&task, "$ ", Duration::from_secs(45)), None);
        assert_eq!(
            watchdog.observe(&task, "ordinary output", Duration::from_secs(90)),
            None
        );
        assert!(matches!(
            watchdog.observe(&task, "$ ", Duration::from_secs(91)),
            Some(ShellTaskNotice::InteractivePrompt { .. })
        ));
    }

    #[test]
    fn prompt_detection_rejects_ordinary_long_running_output() {
        for output in [
            "phase 1:",
            "password hashing progress:",
            "pinned calculation phase:",
            "comparing values: left < right",
            "ordinary output",
        ] {
            assert!(!looks_like_interactive_prompt(output), "{output}");
        }
        for output in [
            "Continue?",
            "$ ",
            "Select an option:",
            "[sudo] password for user:",
            "Proceed [y/N]",
            "Press Enter to continue",
        ] {
            assert!(looks_like_interactive_prompt(output), "{output}");
        }
    }

    fn background_task(output_bytes: u64) -> ShellTask {
        let chat_id = Uuid::new_v4();
        ShellTask {
            updated_at_ms: None,
            ended_at_ms: None,
            id: Uuid::new_v4(),
            chat_id: chat_id.into(),
            origin: ShellTaskOrigin::User,
            owner: ShellTaskOwner::User {
                chat_id: chat_id.into(),
            },
            call_id: None,
            command: "interactive-command".into(),
            description: None,
            cwd: PathBuf::from("/tmp"),
            state: ShellTaskState::Background,
            was_background: true,
            pid: Some(123),
            exit_code: None,
            signal: None,
            reason: None,
            output_bytes,
            capabilities: ShellTaskCapabilities {
                can_background: false,
                can_stop: true,
                can_read_output: true,
            },
        }
    }
}

//! Claude Code–compatible command hooks: where each comes from and whether it may run.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{ChatId, LoopId, RunId};

/// The Claude Code hook events Tickloop runs; hooks for any other event are skipped with a warning.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub enum HookEvent {
    PreToolUse,
    PostToolUse,
    PostToolUseFailure,
    UserPromptSubmit,
    SessionStart,
    SessionEnd,
    Stop,
    StopFailure,
    SubagentStop,
    PreCompact,
}

impl HookEvent {
    pub const ALL: [Self; 10] = [
        Self::PreToolUse,
        Self::PostToolUse,
        Self::PostToolUseFailure,
        Self::UserPromptSubmit,
        Self::SessionStart,
        Self::SessionEnd,
        Self::Stop,
        Self::StopFailure,
        Self::SubagentStop,
        Self::PreCompact,
    ];

    /// The event's name in Claude Code's configuration and hook input.
    pub fn name(self) -> &'static str {
        match self {
            Self::PreToolUse => "PreToolUse",
            Self::PostToolUse => "PostToolUse",
            Self::PostToolUseFailure => "PostToolUseFailure",
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::SessionStart => "SessionStart",
            Self::SessionEnd => "SessionEnd",
            Self::Stop => "Stop",
            Self::StopFailure => "StopFailure",
            Self::SubagentStop => "SubagentStop",
            Self::PreCompact => "PreCompact",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|event| event.name() == name)
    }

    /// Whether the event is about one tool call, which a hook's `if` can then narrow.
    pub fn is_tool_event(self) -> bool {
        matches!(
            self,
            Self::PreToolUse | Self::PostToolUse | Self::PostToolUseFailure
        )
    }

    /// Whether a matcher selects this event's hooks: the tool name for tool events, the source for
    /// `SessionStart` and the trigger for `PreCompact`. Other events ignore any matcher.
    pub fn uses_matcher(self) -> bool {
        matches!(
            self,
            Self::PreToolUse
                | Self::PostToolUse
                | Self::PostToolUseFailure
                | Self::SessionStart
                | Self::PreCompact
        )
    }
}

/// Where a hook comes from. Hooks are deduplicated, and their trust is recorded, per source.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HookSource {
    /// The user's own `settings.json`, at the top level or in the Folder's partition.
    User,
    /// An installed, enabled plugin, by its `<plugin>@<marketplace>` ID.
    Plugin { id: String },
    /// The Folder's `.claude/settings.json` or `.claude/settings.local.json`.
    Repository,
}

/// Whether a hook may run. A user hook needs no confirmation: it is trusted unless disabled.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HookTrust {
    /// The user confirmed this exact content.
    Trusted,
    /// The user has not confirmed it yet.
    Pending,
    /// The user confirmed it, but its content has changed since.
    Modified,
    /// The user turned it off.
    Disabled,
}

/// The directories a plugin hook runs with.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HookPluginDirectories {
    pub root: PathBuf,
    pub data: PathBuf,
}

/// How a hook runs apart from the event that starts it, as Claude Code's `async` and
/// `asyncRewake` declare: in the background, without holding up the event, with any decision it
/// prints ignored.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HookBackground {
    /// `async`: its `systemMessage` and `additionalContext` reach the model's next request.
    Async,
    /// `asyncRewake`: exit status 2 notifies the Loop with its output, waking it when idle.
    AsyncRewake {
        /// Claude Code's `rewakeMessage`: what the model reads before the output.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
        /// Claude Code's `rewakeSummary`: the one line the user sees.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
    },
}

/// What stands in for a sensitive value a view must not show.
const REDACTED_VALUE: &str = "[redacted]";

/// One `command` hook as configured for a Folder.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ConfiguredHook {
    /// Identifies the hook within its source: its event, matcher and normalized declaration.
    pub key: String,
    pub source: HookSource,
    /// The file that declares it.
    pub file: PathBuf,
    pub event: HookEvent,
    /// `None` when none is declared or the event uses none.
    pub matcher: Option<String>,
    /// The command as declared.
    pub command: String,
    /// The command to run, with a plugin's placeholders substituted; `None` while it refers to a
    /// plugin user configuration value that is not set.
    pub resolved_command: Option<String>,
    /// The plugin's sensitive user configuration values `resolved_command` contains. The host
    /// removes them from what the hook prints before the model, a view or a record sees it, and
    /// views get the hook [`ConfiguredHook::for_views`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sensitive_values: Vec<String>,
    /// Claude Code's `if`: a Permission Rule such as `Bash(git push*)` the tool call must also
    /// match, after the matcher. A hook of an event without tool calls that has one never runs.
    #[serde(default, rename = "if", skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
    pub timeout_secs: u64,
    /// `None` for a hook its event waits for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<HookBackground>,
    pub plugin: Option<HookPluginDirectories>,
    /// What a trust record must match: the declaration with the plugin's whole content, or with
    /// the Folder's `.claude/hooks/` directory. `None` for user hooks.
    pub hash: Option<String>,
    pub trust: HookTrust,
}

impl ConfiguredHook {
    /// The hook as a view may show it: each sensitive value in the command it runs replaced
    /// with `[redacted]`, and the values themselves left out.
    pub fn for_views(mut self) -> Self {
        let mut secrets = std::mem::take(&mut self.sensitive_values);
        secrets.retain(|secret| !secret.is_empty());
        secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
        if let Some(command) = &mut self.resolved_command {
            for secret in &secrets {
                *command = command.replace(secret.as_str(), REDACTED_VALUE);
            }
        }
        self
    }

    pub fn needs_configuration(&self) -> bool {
        self.resolved_command.is_none()
    }

    /// Whether the hook may run now: trusted (a user hook unless disabled) and fully configured.
    pub fn may_run(&self) -> bool {
        self.trust == HookTrust::Trusted && !self.needs_configuration()
    }
}

/// Every hook configured for a Folder, with the problems found reading them.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct HookList {
    pub hooks: Vec<ConfiguredHook>,
    pub warnings: Vec<String>,
    /// The revision of the trust records the list was built from; a trust write checks it.
    pub revision: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum HookTrustAction {
    /// Confirms the content the user saw, by the hash it had in the list.
    Trust {
        hash: String,
    },
    Disable,
    /// Undoes [`HookTrustAction::Disable`].
    Enable,
}

/// A change to the trust record of the hook `key` from `source`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HookTrustChange {
    pub source: HookSource,
    pub key: String,
    pub action: HookTrustAction,
}

/// How one hook run ended.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HookRunOutcome {
    /// It exited 0 with output the host could read.
    Success,
    /// It exited 2, or its output blocked or denied.
    Blocking,
    /// It exited with another status, or its output could not be read: only the user is told.
    NonBlockingError,
    /// Its timeout passed and the environment ended it.
    TimedOut,
    /// The turn was interrupted or stopped while it ran.
    Cancelled,
    /// The environment could not start it.
    NotRun,
}

/// What a hook's output decided.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HookDecision {
    Allow,
    Ask,
    Deny,
    /// A `decision: "block"` outside `PreToolUse`, or an exit status 2.
    Block,
    /// An `allow` from a plugin or repository hook, which may not admit a tool call.
    IgnoredAllow,
}

/// Which end of a background hook's run a record is.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HookBackgroundPhase {
    /// It started and its event went on without it; the record's outcome is `Success`.
    Started,
    /// It ended; any decision it printed was ignored.
    Ended,
}

/// One hook run as the Chat's activity history keeps it. It never reaches the model.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HookRunRecord {
    pub id: Uuid,
    pub chat_id: ChatId,
    pub loop_id: LoopId,
    pub run_id: RunId,
    pub at_ms: u64,
    pub event: HookEvent,
    pub source: HookSource,
    /// The command as declared, without a plugin's substituted values.
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    pub duration_ms: u64,
    pub outcome: HookRunOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<HookDecision>,
    /// Why it decided, blocked or failed, shortened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The `systemMessage` it showed the user, shortened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_message: Option<String>,
    /// The `updatedInput` it gave a `PreToolUse` call, shortened; the call's own input stays in
    /// the conversation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_input: Option<String>,
    /// Its standard output and error, shortened; empty when it asked to suppress its output.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub output: String,
    /// `None` for a run its event waited for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<HookBackgroundPhase>,
}

/// Hooks an event matched that did not run because they may not run yet.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HookSkipNotice {
    pub event: HookEvent,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    /// Plugin and repository hooks the user has not confirmed, or whose content changed since.
    pub unconfirmed: usize,
    /// Plugin hooks that refer to a user configuration value that is not set.
    pub unconfigured: usize,
}

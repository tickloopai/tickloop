//! Claude Code–compatible command hooks: where each comes from and whether it may run.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

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
    pub timeout_secs: u64,
    pub plugin: Option<HookPluginDirectories>,
    /// What a trust record must match: the declaration with the plugin's whole content, or with
    /// the Folder's `.claude/hooks/` directory. `None` for user hooks.
    pub hash: Option<String>,
    pub trust: HookTrust,
}

impl ConfiguredHook {
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

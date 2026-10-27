use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    ApiKeyStatus, AutoPolicy, AutoReviewerHealth, AutoReviewerPreference, AutoRuleChange,
    AutoRuleSettings, AutoSettingsRevision, ChatId, ExecutionMode, PermissionRule,
};

pub type AdditionalDirectorySources = BTreeMap<PathBuf, BTreeSet<AdditionalDirectorySource>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdditionalDirectorySource {
    UserSettings,
    DirectorySettings,
    LaunchArgument,
    Session,
}

impl std::fmt::Display for AdditionalDirectorySource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::UserSettings => "user settings",
            Self::DirectorySettings => "directory settings",
            Self::LaunchArgument => "launch argument",
            Self::Session => "session",
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PermissionsRevision {
    local: u64,
    project_rules: String,
    auto_settings: AutoSettingsRevision,
}

impl PermissionsRevision {
    pub fn new(local: u64, project_rules: String, auto_settings: AutoSettingsRevision) -> Self {
        Self {
            local,
            project_rules,
            auto_settings,
        }
    }

    pub fn local(&self) -> u64 {
        self.local
    }

    pub fn project_rules(&self) -> &str {
        &self.project_rules
    }

    pub fn auto_settings(&self) -> &AutoSettingsRevision {
        &self.auto_settings
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PermissionsView {
    pub chat_id: ChatId,
    pub revision: PermissionsRevision,
    pub execution_mode: ExecutionMode,
    pub chat_model_label: String,
    pub preferred_reviewer: AutoReviewerPreference,
    pub auto_policy: AutoPolicy,
    pub auto_rule_settings: AutoRuleSettings,
    pub auto_settings_revision: AutoSettingsRevision,
    pub jev_key_status: ApiKeyStatus,
    pub auto_reviewer_health: AutoReviewerHealth,
    pub selected_loop_id: Option<Uuid>,
    pub rules: Vec<PermissionRuleView>,
    pub working_directory: PathBuf,
    pub additional_directories: Vec<PathBuf>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub additional_directory_sources: AdditionalDirectorySources,
    pub warnings: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PermissionRuleSource {
    ChatSession,
    ProjectUser,
    ProjectConstraint,
    ManagedConstraint,
    /// Imposed by the host on every Chat; never editable.
    Host,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PermissionRuleScope {
    Chat,
    Loop(Uuid),
    Project(PathBuf),
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PermissionRuleView {
    pub rule: PermissionRule,
    pub source: PermissionRuleSource,
    pub scope: PermissionRuleScope,
    pub editable: bool,
    pub effective_note: Option<String>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum PermissionChange {
    UpsertRule { rule: PermissionRule },
    RemoveRule { id: Uuid },
    AddDirectory { path: PathBuf },
    RemoveDirectory { path: PathBuf },
    SelectReviewer { reviewer: AutoReviewerPreference },
    ChangeAutoRules { change: AutoRuleChange },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct WorkspaceScope {
    /// `None` for a Chat without a Folder.
    folder: Option<PathBuf>,
    additional_directories: Vec<PathBuf>,
}

impl WorkspaceScope {
    /// A scope over canonical directories; `additional_directories` excludes `folder`.
    pub fn new(folder: Option<PathBuf>, additional_directories: Vec<PathBuf>) -> Self {
        Self {
            folder,
            additional_directories,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct SessionPermissionDenial {
    tool: String,
    identity_sha256: String,
    workspace: WorkspaceScope,
}

impl SessionPermissionDenial {
    pub fn new(tool: String, identity_sha256: String, workspace: WorkspaceScope) -> Self {
        Self {
            tool,
            identity_sha256,
            workspace,
        }
    }

    pub fn tool(&self) -> &str {
        &self.tool
    }

    pub fn identity_sha256(&self) -> &str {
        &self.identity_sha256
    }

    pub fn workspace(&self) -> &WorkspaceScope {
        &self.workspace
    }
}

//! The user settings file `<root>/settings.json`, corresponding to Claude Code's user settings
//! `~/.claude/settings.json`.
//!
//! Following Claude Code's `updateSettingsForSource`: every write first reads the file and merges
//! into it, changing only the keys involved in this write and keeping all other keys as they are;
//! if the file is not valid JSON, or a key on the write path has the wrong type, the write fails
//! without overwriting. Writes go to a temporary file that is then renamed, so readers never see a
//! half-written file; when the settings file is a symlink, the link target is written, following
//! Claude Code's `writeFileSyncAndFlush_DEPRECATED`. The read-modify-write happens under an
//! exclusive lock on `settings.json.lock`, so concurrent writes from several processes do not lose
//! each other's changes.
//!
//! Reading follows Claude Code's `parseSettingsFile`: a missing file or one with only whitespace
//! means no settings; when the file is invalid or unreadable, or a value is invalid, that item does
//! not take effect and the problem is returned as a warning instead of being silently dropped
//! (users need to know when persistent Deny/Ask rules stop applying).

pub mod claude_auto_import;
mod execution_mode;
pub mod instructions;
pub mod loop_skill;
pub mod package_http;
pub mod sandbox;
mod store;
pub use execution_mode::parse_execution_mode;
pub use tickloop_harness::policy::{AutoSettings, ProjectRuleChanges};
pub mod new_chat;
pub mod skills;
#[cfg(test)]
mod web_search_tests;

use std::{
    collections::HashSet,
    fs::File,
    io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use tickloop_harness::base::output_style::OutputStyle;
use tickloop_harness::base::private_fs::{
    PRIVATE_FILE_MODE, create_private_directory, replace_file_atomically,
};
use tickloop_harness::model::cards::{
    ModelCatalog, ModelSelection, model_for_persistent_selection,
    upgrade_saved_legacy_builtin_model,
};
use tickloop_harness::protocol::{
    AutoReviewerPreference, AutoRuleSettings, AutoSettingsRevision, Decision, ExecutionMode,
    ModelConfig, PermissionRule, ReasoningEffort,
};

pub mod mcp;
pub mod mcp_credentials;
pub mod plugin_config;

pub const SETTINGS_FILE_NAME: &str = "settings.json";

const MODEL_KEY: &str = "model";
const PROVIDERS_KEY: &str = "providers";
const PROVIDERS_DIRECTORY: &str = "providers";
const DEFAULT_MODEL_KEY: &str = "defaultModel";
const EFFORT_LEVEL_KEY: &str = "effortLevel";
const THINKING_ENABLED_KEY: &str = "alwaysThinkingEnabled";
const DEFAULT_MODE_KEY: &str = "defaultMode";
const CONTEXT_WINDOW_KEY: &str = "contextWindowTokens";
/// Claude Code's `CLAUDE_CODE_AUTO_COMPACT_WINDOW`: a window in tokens that caps the one the
/// automatic compaction line is measured in.
const AUTO_COMPACT_WINDOW_KEY: &str = "autoCompactWindow";
const PREFERS_REDUCED_MOTION_KEY: &str = "prefersReducedMotion";
const MONITOR_KEY: &str = "monitor";
const MONITOR_POPUP_KEY: &str = "monitorPopup";
const MONITOR_WIDTH_KEY: &str = "monitorWidth";
const PROMPT_SUGGESTION_ENABLED_KEY: &str = "promptSuggestionEnabled";
const USE_AUTO_MODE_DURING_PLAN_KEY: &str = "useAutoModeDuringPlan";
/// The execution modes Shift+Tab visits, in Claude Code's order; at least one.
const SHIFT_TAB_MODES_KEY: &str = "shiftTabModes";
pub const DEFAULT_SHIFT_TAB_MODES: [ExecutionMode; 2] = [ExecutionMode::Auto, ExecutionMode::Plan];
/// Claude Code's `outputStyle` setting.
const OUTPUT_STYLE_KEY: &str = "outputStyle";
const TASK_TOOLS_ENABLED_KEY: &str = "taskToolsEnabled";
/// The token budget a new goal gets unless it names its own.
const GOAL_TOKEN_BUDGET_KEY: &str = "goalTokenBudget";
/// The model that checks a goal's reported completion; unset, the Chat's own model checks it.
const GOAL_CHECK_MODEL_KEY: &str = "goalCheckModel";
/// Follows the name and values of Claude Code's `cleanupPeriodDays`, but only 0 has a meaning: do
/// not write the Transcript. Tickloop never deletes files automatically by age; positive values
/// save the Chat just like an unset value.
const CLEANUP_PERIOD_DAYS_KEY: &str = "cleanupPeriodDays";
/// Settings partitioned by project directory, modeled on `projects[<directory>]` in Claude Code's
/// global config.
const PROJECTS_KEY: &str = "projects";
const PERMISSIONS_KEY: &str = "permissions";
const RULES_KEY: &str = "rules";
const AUTO_APPROVAL_KEY: &str = "autoApproval";
const AUTO_SETTINGS_SCHEMA_VERSION: u64 = 1;
const TRUST_ACCEPTED_KEY: &str = "hasTrustDialogAccepted";
/// Permission bits of the original file, kept when replacing the settings file.
const PRESERVED_PERMISSION_BITS: u32 = 0o777;
const LOCK_FILE_SUFFIX: &str = ".lock";
/// Maximum number of symlink levels to resolve, the same as Linux `MAXSYMLINKS`; Claude Code
/// resolves only one level.
const MAX_SYMLINK_HOPS: usize = 40;

#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("settings changed while editing")]
    Conflict,
    #[error("cannot read settings file {path}: {source}")]
    Read { path: PathBuf, source: io::Error },
    #[error("settings file {0} is not valid JSON; fix or remove it before changing settings")]
    InvalidJson(PathBuf),
    #[error("settings file {0} must contain a JSON object")]
    NotAnObject(PathBuf),
    #[error(
        "settings file {path} must have {expected} at `{key}`; fix it before changing settings"
    )]
    InvalidEntry {
        path: PathBuf,
        key: String,
        expected: String,
    },
    #[error("cannot lock settings file {path}: {source}")]
    Lock { path: PathBuf, source: io::Error },
    #[error("cannot write settings file {path}: {source}")]
    Write { path: PathBuf, source: io::Error },
}

/// A key with the wrong type on the write path; `update` adds the file path and turns it into
/// [`SettingsError::InvalidEntry`].
struct InvalidEntry {
    key: String,
    expected: String,
}

/// A persistent permission rule in the settings file; its project is expressed by the `projects`
/// partition it sits in, so it carries no Loop.
#[derive(Deserialize, Serialize)]
struct SavedRule {
    id: Uuid,
    decision: Decision,
    tool: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    specifier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    input: Option<Value>,
}

impl From<SavedRule> for PermissionRule {
    fn from(rule: SavedRule) -> Self {
        Self {
            id: rule.id,
            decision: rule.decision,
            tool: rule.tool,
            specifier: rule.specifier,
            input: rule.input,
            loop_id: None,
            persistent: true,
        }
    }
}

/// Console caches under the Tickloop root: pasted images at `<chatId>/<N>.<ext>` and large
/// pastes at `<hash>.txt`.
pub const IMAGE_CACHE_DIRECTORY: &str = "image-cache";
pub const PASTE_CACHE_DIRECTORY: &str = "paste-cache";
pub const PASTE_FILE_EXTENSION: &str = ".txt";

/// The Tickloop state root: `TICKLOOP_CONFIG_DIR` when it is non-empty, otherwise `.tickloop` under
/// the user's home directory. The home directory comes from `std::env::home_dir()`, which falls
/// back to the account database when `$HOME` is missing or empty. Returns `None` when neither is
/// available, and the caller writes nothing to disk. Tick package programs running on their own use
/// the same rule to find their credentials.
pub fn config_root_directory() -> Option<PathBuf> {
    tick_packages::default_config_root()
}

#[derive(Clone, Debug)]
pub struct UserSettings {
    path: PathBuf,
    /// The home directory whose Claude Code instructions stand in for a missing `AGENTS.md` in
    /// the state root; without one, only the state root and the Folder supply instruction files.
    home_directory: Option<PathBuf>,
}

/// Settings content from one read, along with the problems found while reading each item.
#[derive(Clone, Debug)]
pub struct LoadedSettings {
    path: PathBuf,
    settings: Map<String, Value>,
    warnings: Vec<String>,
    read_error: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DisplayPreferences {
    pub prefers_reduced_motion: bool,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MonitorPreference {
    #[default]
    Auto,
    Hidden,
}

/// Whether Monitor may float a hosted Tick in a Herdr popup. Installing the popup plugin, with
/// `tickloop monitor popup install` or Monitor's confirmed offer, enables it; the setting itself
/// never installs anything.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MonitorPopup {
    Disabled,
    #[default]
    Enabled,
}

impl LoadedSettings {
    pub fn search_config(&mut self) -> Option<tickloop_harness::protocol::SearchConfig> {
        let Some(value) = self.settings.get("webSearch").cloned() else {
            return tickloop_harness::protocol::SearchConfig::enabled_default();
        };
        if value.is_null() {
            return None;
        }
        match serde_json::from_value::<tickloop_harness::protocol::SearchConfig>(value) {
            Ok(config) if tickloop_harness::tools::web_search::validate_config(&config).is_ok() => {
                Some(config)
            }
            _ => {
                self.warn(
                    "`webSearch` is invalid; search is disabled until it is corrected".into(),
                );
                None
            }
        }
    }

    pub fn status_lines(&mut self) -> Option<Vec<String>> {
        let value = self.settings.get("statusLines")?.clone();
        match serde_json::from_value::<Vec<String>>(value) {
            Ok(lines) => Some(lines),
            Err(error) => {
                self.warn(format!(
                    "`statusLines` must be an array of strings and is ignored ({error})"
                ));
                None
            }
        }
    }

    pub fn jev_config(&self) -> Result<tickloop_harness::model::judgment::JevConfig, String> {
        if let Some(error) = &self.read_error {
            return Err(error.clone());
        }
        match self.settings.get("jev") {
            Some(value) => serde_json::from_value(value.clone()).map_err(|_| {
                "invalid Jev settings; expected endpoint and model, with the key in Agent .env"
                    .into()
            }),
            None => Ok(tickloop_harness::model::judgment::JevConfig::default()),
        }
    }

    pub fn model_catalog(&self) -> Result<ModelCatalog, String> {
        if let Some(error) = &self.read_error {
            return Err(error.clone());
        }
        Ok(ModelCatalog::defaults())
    }

    pub fn configured_model(&mut self) -> Result<Option<ModelConfig>, String> {
        if let Some(error) = &self.read_error {
            return Err(error.clone());
        }
        let catalog = ModelCatalog::defaults();
        if has_retired_model_settings(&self.settings, parent_directory(&self.path)) {
            self.warn(BUILTIN_ONLY_NOTICE.into());
        }
        if let Some(value) = self.settings.get(DEFAULT_MODEL_KEY) {
            if let Ok(selection) = serde_json::from_value::<ModelSelection>(value.clone())
                && let Some(model) = catalog.get(&selection.provider, &selection.model)
            {
                return Ok(Some(model));
            }
            self.warn(BUILTIN_ONLY_NOTICE.into());
            return Ok(None);
        }
        if let Some(value) = self.settings.get(MODEL_KEY) {
            if let Ok(saved) = serde_json::from_value::<ModelConfig>(value.clone()) {
                let saved = upgrade_saved_legacy_builtin_model(saved);
                if let Some(model) = catalog.get(&saved.provider_id, &saved.model_id) {
                    return Ok(Some(model));
                }
            }
            self.warn(BUILTIN_ONLY_NOTICE.into());
        }
        Ok(None)
    }

    pub fn model(&mut self) -> Option<ModelConfig> {
        match self.configured_model() {
            Ok(model) => model,
            Err(error) => {
                self.warn(error);
                None
            }
        }
    }

    pub fn reasoning_effort(&mut self) -> Option<ReasoningEffort> {
        let value = self.settings.get(EFFORT_LEVEL_KEY)?.clone();
        if value == "auto" || value == "off" {
            return None;
        }
        serde_json::from_value(value)
            .map_err(|error| {
                self.warn(format!(
                    "`{EFFORT_LEVEL_KEY}` is not a supported reasoning effort and is ignored ({error})"
                ))
            })
            .ok()
    }

    pub fn goal_token_budget(&mut self) -> Option<u64> {
        let value = self.settings.get(GOAL_TOKEN_BUDGET_KEY)?;
        match value.as_u64() {
            Some(budget) if budget > 0 => Some(budget),
            _ => {
                self.warn(format!(
                    "`{GOAL_TOKEN_BUDGET_KEY}` must be a positive integer and is ignored"
                ));
                None
            }
        }
    }

    pub fn auto_compact_window(&mut self) -> Option<usize> {
        let value = self.settings.get(AUTO_COMPACT_WINDOW_KEY)?;
        match value
            .as_u64()
            .and_then(|tokens| usize::try_from(tokens).ok())
        {
            Some(tokens) if tokens > 0 => Some(tokens),
            _ => {
                self.warn(format!(
                    "`{AUTO_COMPACT_WINDOW_KEY}` must be a positive integer of tokens and is ignored"
                ));
                None
            }
        }
    }

    pub fn goal_check_model(&mut self) -> Option<ModelConfig> {
        let value = self.settings.get(GOAL_CHECK_MODEL_KEY)?.clone();
        let model = serde_json::from_value::<ModelSelection>(value)
            .ok()
            .and_then(|selection| {
                ModelCatalog::defaults().get(&selection.provider, &selection.model)
            });
        if model.is_none() {
            self.warn(format!(
                "`{GOAL_CHECK_MODEL_KEY}` must name a built-in model as provider and model; the Chat's model checks goals instead"
            ));
        }
        model
    }

    pub fn task_tools_enabled(&mut self) -> bool {
        match self.settings.get(TASK_TOOLS_ENABLED_KEY) {
            Some(Value::Bool(enabled)) => *enabled,
            None => false,
            Some(_) => {
                self.warn(format!(
                    "`{TASK_TOOLS_ENABLED_KEY}` must be a boolean and is ignored"
                ));
                false
            }
        }
    }

    pub fn thinking_enabled(&mut self) -> bool {
        if let Some(value) = self.settings.get(THINKING_ENABLED_KEY).cloned() {
            if let Some(enabled) = value.as_bool() {
                return enabled;
            }
            self.warn(format!(
                "`{THINKING_ENABLED_KEY}` must be a boolean and is ignored"
            ));
        }
        self.settings.get(EFFORT_LEVEL_KEY) != Some(&Value::String("off".into()))
    }

    pub fn context_window_tokens(&mut self) -> Option<usize> {
        None
    }

    /// The saved `defaultMode`, read like `--mode`. A value without a Tickloop mode is reported and
    /// left out, so new Chats start in the default mode rather than refusing to start.
    pub fn default_execution_mode(&mut self) -> Option<ExecutionMode> {
        let value = self.settings.get(DEFAULT_MODE_KEY)?.clone();
        let name = value
            .as_str()
            .map_or_else(|| value.to_string(), str::to_owned);
        match parse_execution_mode(&name) {
            Ok((mode, warning)) => {
                if let Some(warning) = warning {
                    self.warn(format!("`{DEFAULT_MODE_KEY}`: {warning}"));
                }
                Some(mode)
            }
            Err(error) => {
                self.warn(format!(
                    "`{DEFAULT_MODE_KEY}`: {error}; using {}",
                    ExecutionMode::default()
                ));
                None
            }
        }
    }

    pub fn is_working_directory_trusted(&mut self, project: &Path) -> bool {
        let key = project_key(project);
        let Some(projects) = self.settings.get(PROJECTS_KEY) else {
            return false;
        };
        let Some(projects) = projects.as_object() else {
            self.warn(format!(
                "`{PROJECTS_KEY}` is not an object; the working directory remains untrusted"
            ));
            return false;
        };
        let Some(entry) = projects.get(&key) else {
            return false;
        };
        let Some(entry) = entry.as_object() else {
            self.warn(format!(
                "`{PROJECTS_KEY}[{key:?}]` is not an object; the working directory remains untrusted"
            ));
            return false;
        };
        match entry.get(TRUST_ACCEPTED_KEY) {
            Some(Value::Bool(true)) => true,
            Some(_) => {
                self.warn(format!(
                    "`{PROJECTS_KEY}[{key:?}].{TRUST_ACCEPTED_KEY}` is not true; the working directory remains untrusted"
                ));
                false
            }
            None => false,
        }
    }

    pub fn monitor_preference(&self) -> MonitorPreference {
        self.settings
            .get(MONITOR_KEY)
            .cloned()
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap_or_default()
    }

    /// Monitor's column as the user last dragged it, its separator included. A View narrows it
    /// to keep the conversation readable.
    pub fn monitor_width(&mut self) -> Option<u16> {
        let value = self.settings.get(MONITOR_WIDTH_KEY)?;
        let columns = value
            .as_u64()
            .and_then(|columns| u16::try_from(columns).ok())
            .filter(|columns| *columns > 0);
        if columns.is_none() {
            self.warn(format!(
                "`{MONITOR_WIDTH_KEY}` is not a positive column count and is ignored"
            ));
        }
        columns
    }

    pub fn monitor_popup(&self) -> MonitorPopup {
        self.settings
            .get(MONITOR_POPUP_KEY)
            .cloned()
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap_or_default()
    }

    /// The modes Shift+Tab visits, in [`ExecutionMode::CYCLE_ORDER`]. An empty or invalid list
    /// falls back to Auto and Plan.
    pub fn shift_tab_modes(&mut self) -> Vec<ExecutionMode> {
        let Some(value) = self.settings.get(SHIFT_TAB_MODES_KEY) else {
            return DEFAULT_SHIFT_TAB_MODES.to_vec();
        };
        match serde_json::from_value::<Vec<ExecutionMode>>(value.clone()) {
            Ok(modes) if !modes.is_empty() => ExecutionMode::CYCLE_ORDER
                .into_iter()
                .filter(|mode| modes.contains(mode))
                .collect(),
            _ => {
                self.warn(format!(
                    "`{SHIFT_TAB_MODES_KEY}` is not a nonempty list of `auto`, `acceptEdits`, `plan` and `fullAccess`; shift+tab uses Auto and Plan"
                ));
                DEFAULT_SHIFT_TAB_MODES.to_vec()
            }
        }
    }

    pub fn prompt_suggestion_enabled(&mut self) -> bool {
        match self.settings.get(PROMPT_SUGGESTION_ENABLED_KEY) {
            Some(Value::Bool(enabled)) => *enabled,
            None => true,
            Some(_) => {
                self.warn(format!(
                    "`{PROMPT_SUGGESTION_ENABLED_KEY}` is not a boolean and is ignored"
                ));
                true
            }
        }
    }

    pub fn use_auto_mode_during_plan(&mut self) -> bool {
        match self.settings.get(USE_AUTO_MODE_DURING_PLAN_KEY) {
            Some(Value::Bool(enabled)) => *enabled,
            None => true,
            Some(_) => {
                self.warn(format!(
                    "`{USE_AUTO_MODE_DURING_PLAN_KEY}` is not a boolean and is ignored"
                ));
                true
            }
        }
    }

    pub fn display_preferences(&mut self) -> DisplayPreferences {
        let prefers_reduced_motion = self
            .settings
            .get(PREFERS_REDUCED_MOTION_KEY)
            .cloned()
            .and_then(|value| {
                value.as_bool().or_else(|| {
                    self.warn(format!(
                        "`{PREFERS_REDUCED_MOTION_KEY}` is not a boolean and is ignored"
                    ));
                    None
                })
            })
            .unwrap_or(false);
        DisplayPreferences {
            prefers_reduced_motion,
        }
    }

    pub fn output_style(&mut self) -> OutputStyle {
        let Some(value) = self.settings.get(OUTPUT_STYLE_KEY).cloned() else {
            return OutputStyle::default();
        };
        serde_json::from_value(value).unwrap_or_else(|error| {
            self.warn(format!(
                "`{OUTPUT_STYLE_KEY}` is not `standard`, `concise` or `detailed` and is ignored ({error})"
            ));
            OutputStyle::default()
        })
    }

    /// The value of `cleanupPeriodDays`; a value that is not a non-negative integer does not take
    /// effect and is recorded as a warning.
    fn cleanup_period_days(&mut self) -> Option<u64> {
        let value = self.settings.get(CLEANUP_PERIOD_DAYS_KEY)?;
        if let Some(days) = value.as_u64() {
            return Some(days);
        }
        self.warn(format!(
            "`{CLEANUP_PERIOD_DAYS_KEY}` is not a whole number of days (0 or more) and is ignored; conversations are saved until it is fixed"
        ));
        None
    }

    /// Persistent permission rules saved for `project` (the normalized working directory); invalid
    /// entries are skipped and recorded as warnings.
    pub fn permission_rules(&mut self, project: &Path) -> Vec<PermissionRule> {
        let mut problems = Vec::new();
        let rules = saved_project_rules(&self.settings, project, &mut problems);
        for problem in problems {
            self.warn(problem);
        }
        rules
    }

    pub fn additional_directories(
        &mut self,
        folder: &Path,
    ) -> tickloop_harness::protocol::AdditionalDirectorySources {
        use tickloop_harness::protocol::AdditionalDirectorySource;
        let mut directories = Default::default();
        self.read_additional_directories(
            folder,
            self.settings.get(PERMISSIONS_KEY).cloned(),
            PERMISSIONS_KEY,
            AdditionalDirectorySource::UserSettings,
            &mut directories,
        );
        let key = project_key(folder);
        let project = self
            .settings
            .get(PROJECTS_KEY)
            .and_then(|projects| projects.get(&key));
        let permissions = project
            .and_then(|project| project.get(PERMISSIONS_KEY))
            .cloned();
        self.read_additional_directories(
            folder,
            permissions,
            &format!("{PROJECTS_KEY}[{key:?}].{PERMISSIONS_KEY}"),
            AdditionalDirectorySource::DirectorySettings,
            &mut directories,
        );
        directories
    }

    fn read_additional_directories(
        &mut self,
        folder: &Path,
        permissions: Option<Value>,
        location: &str,
        source: tickloop_harness::protocol::AdditionalDirectorySource,
        directories: &mut tickloop_harness::protocol::AdditionalDirectorySources,
    ) {
        let Some(permissions) = permissions else {
            return;
        };
        let Some(permissions) = permissions.as_object() else {
            // Project permission rules already report a malformed shared permissions object.
            if source == tickloop_harness::protocol::AdditionalDirectorySource::UserSettings {
                self.warn(format!(
                    "`{location}` is not an object; additional directories skipped"
                ));
            }
            return;
        };
        let Some(value) = permissions.get("additionalDirectories") else {
            return;
        };
        let location = format!("{location}.additionalDirectories");
        let Some(paths) = value.as_array() else {
            self.warn(format!("`{location}` is not an array; skipped"));
            return;
        };
        for value in paths {
            let Some(path) = value.as_str().filter(|path| !path.is_empty()) else {
                self.warn(format!(
                    "`{location}` contains an invalid directory path; skipped"
                ));
                continue;
            };
            let path = folder.join(path);
            match tickloop_harness::tools::permissions::canonical_directory(path.clone()) {
                Ok(directory) => {
                    directories.entry(directory).or_default().insert(source);
                }
                Err(reason) => self.warn(format!(
                    "additional directory {}: {reason}; skipped",
                    path.display()
                )),
            }
        }
    }

    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    pub fn into_warnings(self) -> Vec<String> {
        self.warnings
    }

    pub fn warn(&mut self, problem: String) {
        let warning = format!("settings file {}: {problem}", self.path.display());
        if !self.warnings.contains(&warning) {
            self.warnings.push(warning);
        }
    }
}

fn saved_project_rules(
    settings: &Map<String, Value>,
    project: &Path,
    problems: &mut Vec<String>,
) -> Vec<PermissionRule> {
    const IGNORED: &str = "persistent permission rules of this project are ignored";
    let Some(projects) = settings.get(PROJECTS_KEY) else {
        return Vec::new();
    };
    let Some(projects) = projects.as_object() else {
        problems.push(format!("`{PROJECTS_KEY}` is not an object; {IGNORED}"));
        return Vec::new();
    };
    let key = project_key(project);
    let Some(entry) = projects.get(&key) else {
        return Vec::new();
    };
    let location = format!("{PROJECTS_KEY}[{key:?}]");
    let Some(entry) = entry.as_object() else {
        problems.push(format!("`{location}` is not an object; {IGNORED}"));
        return Vec::new();
    };
    let Some(permissions) = entry.get(PERMISSIONS_KEY) else {
        return Vec::new();
    };
    let location = format!("{location}.{PERMISSIONS_KEY}");
    let Some(permissions) = permissions.as_object() else {
        problems.push(format!("`{location}` is not an object; {IGNORED}"));
        return Vec::new();
    };
    let Some(rules) = permissions.get(RULES_KEY) else {
        return Vec::new();
    };
    let location = format!("{location}.{RULES_KEY}");
    let Some(rules) = rules.as_array() else {
        problems.push(format!("`{location}` is not an array; {IGNORED}"));
        return Vec::new();
    };
    rules
        .iter()
        .enumerate()
        .filter_map(
            |(index, rule)| match serde_json::from_value::<SavedRule>(rule.clone()) {
                Ok(rule) if !rule.tool.is_empty() && rule.decision != Decision::Check => {
                    let rule = PermissionRule::from(rule);
                    if rule.tool.starts_with("mcp__")
                        && let Err(error) = tickloop_harness::tools::permissions::validate_rule(&rule)
                    {
                        problems.push(format!("`{location}[{index}]` is ignored: {error}"));
                        return None;
                    }
                    Some(rule)
                }
                Ok(_) => {
                    problems.push(format!(
                        "`{location}[{index}]` needs a tool and an allow, ask or deny decision; the rule is ignored"
                    ));
                    None
                }
                Err(error) => {
                    problems.push(format!(
                        "`{location}[{index}]` is not a valid permission rule and is ignored ({error})"
                    ));
                    None
                }
            },
        )
        .collect()
}

pub const BUILTIN_ONLY_NOTICE: &str = "External Provider/model definitions and model-parameter overrides are ignored; only the program's built-in catalog is supported. Unsupported selections use the built-in default. Your files and credentials are unchanged.";

fn has_retired_model_settings(settings: &Map<String, Value>, root: &Path) -> bool {
    if settings.contains_key(PROVIDERS_KEY)
        || settings.contains_key(MODEL_KEY)
        || settings.contains_key(CONTEXT_WINDOW_KEY)
    {
        return true;
    }
    let directory = root.join(PROVIDERS_DIRECTORY);
    if let Ok(metadata) = std::fs::symlink_metadata(&directory)
        && !metadata.file_type().is_dir()
    {
        return true;
    }
    match std::fs::read_dir(&directory) {
        Ok(mut entries) => entries.any(|entry| {
            entry.map_or(true, |entry| {
                entry.path().extension().is_some_and(|extension| {
                    matches!(extension.to_str(), Some("yaml" | "yml" | "json"))
                })
            })
        }),
        Err(error) => error.kind() != io::ErrorKind::NotFound,
    }
}

impl UserSettings {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            home_directory: None,
        }
    }

    pub fn with_home_directory(mut self, home_directory: Option<PathBuf>) -> Self {
        self.home_directory = home_directory;
        self
    }

    /// The settings file under the Tickloop state root.
    pub fn in_root(root: &Path) -> Self {
        Self::new(root.join(SETTINGS_FILE_NAME))
    }

    pub fn file_path(&self) -> &Path {
        &self.path
    }

    pub fn root_directory(&self) -> Option<&Path> {
        self.path.parent()
    }

    pub fn model_catalog(&self) -> Result<ModelCatalog, String> {
        self.read_object().map_err(|error| error.to_string())?;
        Ok(ModelCatalog::defaults())
    }

    pub fn root(&self) -> Option<&Path> {
        self.path.parent()
    }

    pub fn home_directory(&self) -> Option<&Path> {
        self.home_directory.as_deref()
    }

    /// Reads the settings file. When the file is invalid or unreadable, the content is empty and
    /// the problem is recorded as a warning. `cleanupPeriodDays` is a process-wide setting checked
    /// on every read, so an invalid value shows up in the warnings like any other settings problem.
    pub fn load(&self) -> LoadedSettings {
        let (settings, warnings, read_error) = match self.read_object() {
            Ok(settings) => (settings, Vec::new(), None),
            Err(error) => {
                let message = error.to_string();
                (
                    Map::new(),
                    vec![format!(
                        "{message}; the saved model, persistent permission rules, context window and cleanupPeriodDays are ignored"
                    )],
                    Some(message),
                )
            }
        };
        let mut loaded = LoadedSettings {
            path: self.path.clone(),
            settings,
            warnings,
            read_error,
        };
        loaded.cleanup_period_days();
        if loaded.read_error.is_none()
            && let Err(error) = loaded.sandbox()
        {
            loaded.warn(error);
        }
        if has_retired_model_settings(&loaded.settings, parent_directory(&loaded.path)) {
            loaded.warn(BUILTIN_ONLY_NOTICE.into());
        }
        loaded
    }

    /// When `cleanupPeriodDays` is 0, the Transcript is not written (following Claude Code's
    /// `shouldSkipPersistence`).
    pub fn skips_transcripts(&self) -> bool {
        self.load().cleanup_period_days() == Some(0)
    }

    pub fn model(&self) -> Option<ModelConfig> {
        self.load().model()
    }

    pub fn reasoning_effort(&self) -> Option<ReasoningEffort> {
        self.load().reasoning_effort()
    }

    pub fn thinking_enabled(&self) -> bool {
        self.load().thinking_enabled()
    }

    pub fn goal_token_budget(&self) -> Option<u64> {
        self.load().goal_token_budget()
    }

    pub fn goal_check_model(&self) -> Option<ModelConfig> {
        self.load().goal_check_model()
    }

    pub fn auto_compact_window(&self) -> Option<usize> {
        self.load().auto_compact_window()
    }

    pub fn context_window_tokens(&self) -> Option<usize> {
        self.load().context_window_tokens()
    }

    pub fn is_working_directory_trusted(&self, project: &Path) -> bool {
        let canonical_project = project
            .canonicalize()
            .unwrap_or_else(|_| project.to_path_buf());
        self.load().is_working_directory_trusted(&canonical_project)
    }

    pub fn display_preferences(&self) -> DisplayPreferences {
        self.load().display_preferences()
    }

    pub fn permission_rules(&self, project: &Path) -> Vec<PermissionRule> {
        self.load().permission_rules(project)
    }

    pub fn output_style(&self) -> OutputStyle {
        self.load().output_style()
    }

    pub fn save_output_style(&self, style: OutputStyle) -> Result<(), SettingsError> {
        self.update(|settings| {
            settings.insert(
                OUTPUT_STYLE_KEY.into(),
                serde_json::to_value(style).expect("output style serializes to JSON"),
            );
            Ok(())
        })
    }

    pub fn save_default_execution_mode(&self, mode: ExecutionMode) -> Result<(), SettingsError> {
        self.update(|settings| {
            settings.insert(
                DEFAULT_MODE_KEY.into(),
                serde_json::to_value(mode).expect("execution mode serializes to JSON"),
            );
            Ok(())
        })
    }

    pub fn trust_working_directory(&self, project: &Path) -> Result<(), SettingsError> {
        let project = project
            .canonicalize()
            .map_err(|source| SettingsError::Read {
                path: project.to_path_buf(),
                source,
            })?;
        self.update(|settings| {
            let projects = object_entry(settings, PROJECTS_KEY, PROJECTS_KEY.into())?;
            let key = project_key(&project);
            let entry = object_entry(projects, &key, format!("{PROJECTS_KEY}[{key:?}]"))?;
            entry.insert(TRUST_ACCEPTED_KEY.into(), Value::Bool(true));
            Ok(())
        })
    }

    /// Saves the model selection; when `clear_context_window` is true, also removes the context
    /// window setting (the window belonged to the previous model).
    pub fn save_model(
        &self,
        model: &ModelConfig,
        clear_context_window: bool,
    ) -> Result<(), SettingsError> {
        self.save_model_with_selection(model, clear_context_window, None)
    }

    pub fn save_model_with_thinking(
        &self,
        model: &ModelConfig,
        clear_context_window: bool,
        effort: Option<ReasoningEffort>,
    ) -> Result<(), SettingsError> {
        self.save_model_with_selection(model, clear_context_window, Some(effort))
    }

    fn save_model_with_selection(
        &self,
        model: &ModelConfig,
        clear_context_window: bool,
        thinking_choice: Option<Option<ReasoningEffort>>,
    ) -> Result<(), SettingsError> {
        let selected = model_for_persistent_selection(model.clone());
        let model = ModelCatalog::defaults()
            .get(&selected.provider_id, &selected.model_id)
            .ok_or_else(|| SettingsError::InvalidEntry {
                path: self.path.clone(),
                key: DEFAULT_MODEL_KEY.into(),
                expected: "a built-in Provider/model selection".into(),
            })?;
        if let Some(effort) = thinking_choice {
            tickloop_harness::model::validate_reasoning_effort(&model, effort).map_err(|_| {
                SettingsError::InvalidEntry {
                    path: self.path.clone(),
                    key: EFFORT_LEVEL_KEY.into(),
                    expected: "an effort supported by the built-in model".into(),
                }
            })?;
        }
        self.update(|settings| {
            settings.insert(
                DEFAULT_MODEL_KEY.into(),
                serde_json::to_value(ModelSelection {
                    provider: model.provider_id.clone(),
                    model: model.model_id.clone(),
                })
                .expect("selection serializes"),
            );
            if clear_context_window {
                settings.remove(CONTEXT_WINDOW_KEY);
            }
            if let Some(effort) = thinking_choice {
                match effort {
                    Some(effort) => {
                        settings.insert(
                            EFFORT_LEVEL_KEY.into(),
                            serde_json::to_value(effort).expect("effort serializes"),
                        );
                    }
                    None => {
                        settings.remove(EFFORT_LEVEL_KEY);
                    }
                }
                settings.insert(
                    THINKING_ENABLED_KEY.into(),
                    Value::Bool(effort != Some(ReasoningEffort::Off)),
                );
            }
            Ok(())
        })
    }

    pub fn save_reasoning_effort(
        &self,
        effort: Option<ReasoningEffort>,
    ) -> Result<(), SettingsError> {
        self.update(|settings| {
            if effort == Some(ReasoningEffort::On) {
                settings.insert(THINKING_ENABLED_KEY.into(), Value::Bool(true));
            }
            if settings.get(EFFORT_LEVEL_KEY) == Some(&Value::String("off".into()))
                && !settings.contains_key(THINKING_ENABLED_KEY)
            {
                settings.insert(THINKING_ENABLED_KEY.into(), Value::Bool(false));
            }
            if let Some(effort) = effort {
                settings.insert(
                    EFFORT_LEVEL_KEY.into(),
                    serde_json::to_value(effort).expect("reasoning effort serializes to JSON"),
                );
            } else {
                settings.remove(EFFORT_LEVEL_KEY);
            }
            Ok(())
        })
    }

    pub fn save_thinking_enabled(&self, enabled: bool) -> Result<(), SettingsError> {
        self.save_thinking_enabled_with_effort(enabled, None)
    }

    pub fn save_thinking_enabled_with_effort(
        &self,
        enabled: bool,
        effort: Option<ReasoningEffort>,
    ) -> Result<(), SettingsError> {
        self.update(|settings| {
            settings.insert(THINKING_ENABLED_KEY.into(), Value::Bool(enabled));
            if let Some(effort) = effort {
                settings.insert(
                    EFFORT_LEVEL_KEY.into(),
                    serde_json::to_value(effort).expect("reasoning effort serializes to JSON"),
                );
            } else if settings.get(EFFORT_LEVEL_KEY) == Some(&Value::String("off".into())) {
                settings.remove(EFFORT_LEVEL_KEY);
            }
            Ok(())
        })
    }

    pub fn save_context_window(&self, _tokens: usize) -> Result<(), SettingsError> {
        Err(SettingsError::InvalidEntry {
            path: self.path.clone(),
            key: CONTEXT_WINDOW_KEY.into(),
            expected: "the context window supplied by the built-in program catalog".into(),
        })
    }

    pub fn save_monitor_preference(
        &self,
        preference: MonitorPreference,
    ) -> Result<(), SettingsError> {
        self.update(|settings| {
            settings.insert(
                MONITOR_KEY.into(),
                serde_json::to_value(preference).expect("Monitor preference serializes to JSON"),
            );
            Ok(())
        })
    }

    pub fn save_monitor_width(&self, columns: u16) -> Result<(), SettingsError> {
        self.update(|settings| {
            settings.insert(MONITOR_WIDTH_KEY.into(), columns.into());
            Ok(())
        })
    }

    pub fn save_monitor_popup(&self, popup: MonitorPopup) -> Result<(), SettingsError> {
        self.update(|settings| {
            settings.insert(
                MONITOR_POPUP_KEY.into(),
                serde_json::to_value(popup).expect("Monitor popup setting serializes to JSON"),
            );
            Ok(())
        })
    }

    pub fn save_shift_tab_modes(&self, modes: &[ExecutionMode]) -> Result<(), SettingsError> {
        let ordered = ExecutionMode::CYCLE_ORDER
            .into_iter()
            .filter(|mode| modes.contains(mode))
            .collect::<Vec<_>>();
        debug_assert!(!ordered.is_empty(), "Shift+Tab keeps at least one mode");
        self.update(|settings| {
            settings.insert(
                SHIFT_TAB_MODES_KEY.into(),
                serde_json::to_value(ordered).expect("execution modes serialize to JSON"),
            );
            Ok(())
        })
    }

    pub fn save_task_tools_enabled(&self, enabled: bool) -> Result<(), SettingsError> {
        self.update(|settings| {
            settings.insert(TASK_TOOLS_ENABLED_KEY.into(), enabled.into());
            Ok(())
        })
    }

    pub fn save_search_config(
        &self,
        config: Option<&tickloop_harness::protocol::SearchConfig>,
    ) -> Result<(), SettingsError> {
        self.update(|settings| {
            if let Some(config) = config {
                tickloop_harness::tools::web_search::validate_config(config).map_err(|error| {
                    InvalidEntry {
                        key: "webSearch".into(),
                        expected: error.to_string(),
                    }
                })?;
            }
            settings.insert(
                "webSearch".into(),
                serde_json::to_value(config).expect("search settings serialize"),
            );
            Ok(())
        })
    }

    pub fn save_prompt_suggestion_enabled(&self, enabled: bool) -> Result<(), SettingsError> {
        self.update(|settings| {
            settings.insert(PROMPT_SUGGESTION_ENABLED_KEY.into(), enabled.into());
            Ok(())
        })
    }

    pub fn save_display_preferences(
        &self,
        preferences: DisplayPreferences,
    ) -> Result<(), SettingsError> {
        self.update(|settings| {
            settings.insert(
                PREFERS_REDUCED_MOTION_KEY.into(),
                preferences.prefers_reduced_motion.into(),
            );
            Ok(())
        })
    }

    /// Writes a change to the persistent rules of a project partition, leaving other projects and
    /// keys unchanged; nothing is written if any key on the path has the wrong type.
    pub fn update_permission_rules<'a>(
        &self,
        changes: impl IntoIterator<Item = (&'a Path, &'a ProjectRuleChanges)>,
    ) -> Result<(), SettingsError> {
        self.update(|settings| apply_permission_rule_changes(settings, changes))
    }

    /// Returns the project rules and a revision of their normalized saved JSON.
    /// The revision is checked again inside the settings file lock before an edit.
    pub fn project_rules_with_revision(
        &self,
        project: &Path,
    ) -> Result<(Vec<PermissionRule>, Vec<String>, String), SettingsError> {
        let settings = match self.read_object() {
            Ok(settings) => settings,
            Err(error) => return Ok((Vec::new(), vec![error.to_string()], String::new())),
        };
        let mut problems = Vec::new();
        let rules = saved_project_rules(&settings, project, &mut problems);
        let warnings = problems
            .into_iter()
            .map(|problem| format!("settings file {}: {problem}", self.path.display()))
            .collect();
        Ok((rules, warnings, project_rules_revision(&settings, project)))
    }

    pub fn auto_settings_with_revision(
        &self,
    ) -> Result<(AutoSettings, AutoSettingsRevision), SettingsError> {
        let settings = self.read_object()?;
        let auto = parse_auto_settings(&settings).map_err(|entry| SettingsError::InvalidEntry {
            path: self.path.clone(),
            key: entry.key,
            expected: entry.expected,
        })?;
        Ok((auto, auto_settings_revision(&settings)))
    }

    pub fn update_auto_reviewer_cas(
        &self,
        expected: &AutoSettingsRevision,
        reviewer: AutoReviewerPreference,
    ) -> Result<AutoSettingsRevision, SettingsError> {
        self.update_auto_field_cas(
            expected,
            "reviewer",
            serde_json::to_value(reviewer).expect("reviewer serializes"),
        )
    }

    pub fn update_auto_rules_cas(
        &self,
        expected: &AutoSettingsRevision,
        rules: &AutoRuleSettings,
    ) -> Result<AutoSettingsRevision, SettingsError> {
        rules
            .validate()
            .map_err(|reason| SettingsError::InvalidEntry {
                path: self.path.clone(),
                key: format!("{AUTO_APPROVAL_KEY}.rules"),
                expected: reason,
            })?;
        self.update_auto_field_cas(
            expected,
            "rules",
            serde_json::to_value(rules).expect("Auto rules serialize"),
        )
    }

    fn update_auto_field_cas(
        &self,
        expected: &AutoSettingsRevision,
        field: &str,
        value: Value,
    ) -> Result<AutoSettingsRevision, SettingsError> {
        let write_error = |source| SettingsError::Write {
            path: self.path.clone(),
            source,
        };
        create_private_directory(parent_directory(&self.path)).map_err(write_error)?;
        let lock_path = self.lock_path();
        let _lock = lock_exclusively(&lock_path).map_err(|source| SettingsError::Lock {
            path: lock_path.clone(),
            source,
        })?;
        let mut settings = self.read_object()?;
        if &auto_settings_revision(&settings) != expected {
            return Err(SettingsError::Conflict);
        }
        parse_auto_settings(&settings).map_err(|entry| SettingsError::InvalidEntry {
            path: self.path.clone(),
            key: entry.key,
            expected: entry.expected,
        })?;
        let auto = object_entry(&mut settings, AUTO_APPROVAL_KEY, AUTO_APPROVAL_KEY.into())
            .map_err(|entry| SettingsError::InvalidEntry {
                path: self.path.clone(),
                key: entry.key,
                expected: entry.expected,
            })?;
        auto.insert("schemaVersion".into(), AUTO_SETTINGS_SCHEMA_VERSION.into());
        auto.insert(field.into(), value);
        let revision = auto_settings_revision(&settings);
        let mut contents = serde_json::to_vec_pretty(&Value::Object(settings))
            .expect("settings serialize to JSON");
        contents.push(b'\n');
        self.replace_file(&contents).map_err(write_error)?;
        Ok(revision)
    }

    pub fn update_permission_rules_cas<'a>(
        &self,
        project: &Path,
        expected_revision: &str,
        changes: impl IntoIterator<Item = (&'a Path, &'a ProjectRuleChanges)>,
    ) -> Result<(), SettingsError> {
        let write_error = |source| SettingsError::Write {
            path: self.path.clone(),
            source,
        };
        create_private_directory(parent_directory(&self.path)).map_err(write_error)?;
        let lock_path = self.lock_path();
        let _lock = lock_exclusively(&lock_path).map_err(|source| SettingsError::Lock {
            path: lock_path.clone(),
            source,
        })?;
        let mut settings = self.read_object()?;
        if project_rules_revision(&settings, project) != expected_revision {
            return Err(SettingsError::Conflict);
        }
        let mut problems = Vec::new();
        saved_project_rules(&settings, project, &mut problems);
        if !problems.is_empty() {
            return Err(SettingsError::InvalidEntry {
                path: self.path.clone(),
                key: format!(
                    "{PROJECTS_KEY}[{:?}].{PERMISSIONS_KEY}.{RULES_KEY}",
                    project_key(project)
                ),
                expected: "valid project permission rules".into(),
            });
        }
        apply_permission_rule_changes(&mut settings, changes).map_err(|entry| {
            SettingsError::InvalidEntry {
                path: self.path.clone(),
                key: entry.key,
                expected: entry.expected,
            }
        })?;
        let mut contents = serde_json::to_vec_pretty(&Value::Object(settings))
            .expect("settings serialize to JSON");
        contents.push(b'\n');
        self.replace_file(&contents).map_err(write_error)
    }

    /// Reads the whole settings object; an empty object when the file is missing or contains only
    /// whitespace.
    fn read_object(&self) -> Result<Map<String, Value>, SettingsError> {
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Map::new()),
            Err(source) => {
                return Err(SettingsError::Read {
                    path: self.path.clone(),
                    source,
                });
            }
        };
        if bytes.trim_ascii().is_empty() {
            return Ok(Map::new());
        }
        match serde_json::from_slice::<Value>(&bytes) {
            Ok(Value::Object(settings)) => Ok(settings),
            Ok(_) => Err(SettingsError::NotAnObject(self.path.clone())),
            Err(_) => Err(SettingsError::InvalidJson(self.path.clone())),
        }
    }

    /// Reads, modifies and atomically replaces the settings file under an exclusive lock on
    /// `settings.json.lock`.
    fn update(
        &self,
        edit: impl FnOnce(&mut Map<String, Value>) -> Result<(), InvalidEntry>,
    ) -> Result<(), SettingsError> {
        let write_error = |source| SettingsError::Write {
            path: self.path.clone(),
            source,
        };
        create_private_directory(parent_directory(&self.path)).map_err(write_error)?;
        let lock_path = self.lock_path();
        let _lock = lock_exclusively(&lock_path).map_err(|source| SettingsError::Lock {
            path: lock_path.clone(),
            source,
        })?;
        let mut settings = self.read_object()?;
        edit(&mut settings).map_err(|entry| SettingsError::InvalidEntry {
            path: self.path.clone(),
            key: entry.key,
            expected: entry.expected,
        })?;
        let mut contents = serde_json::to_vec_pretty(&Value::Object(settings))
            .expect("settings serialize to JSON");
        contents.push(b'\n');
        self.replace_file(&contents).map_err(write_error)
    }

    fn lock_path(&self) -> PathBuf {
        let mut path = self.path.clone().into_os_string();
        path.push(LOCK_FILE_SUFFIX);
        PathBuf::from(path)
    }

    /// Following Claude Code's `writeFileSyncAndFlush_DEPRECATED`: when the settings file is a
    /// symlink, writes the link target; the temporary file and the rename both happen in the
    /// target's directory, and the link itself is kept.
    fn replace_file(&self, contents: &[u8]) -> io::Result<()> {
        let target = resolve_symlinks(&self.path);
        let mode = std::fs::metadata(&target)
            .map(|metadata| {
                std::os::unix::fs::PermissionsExt::mode(&metadata.permissions())
                    & PRESERVED_PERMISSION_BITS
            })
            .unwrap_or(PRIVATE_FILE_MODE);
        replace_file_atomically(&target, contents, mode)
    }
}

/// Holds an exclusive advisory lock (`flock`) on `path` while alive. The lock is released when the
/// file is closed, or by the kernel if the process crashes, so no lock is left behind that would
/// need age-based expiry; the lock file itself is kept, because deleting it would let waiters lock
/// a file already detached from the path.
fn lock_exclusively(path: &Path) -> io::Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, PRIVATE_FILE_MODE);
    let file = options.open(path)?;
    file.lock()?;
    Ok(file)
}

fn project_rules_revision(settings: &Map<String, Value>, project: &Path) -> String {
    let rules = settings
        .get(PROJECTS_KEY)
        .and_then(Value::as_object)
        .and_then(|projects| projects.get(&project_key(project)))
        .and_then(Value::as_object)
        .and_then(|project| project.get(PERMISSIONS_KEY))
        .and_then(Value::as_object)
        .and_then(|permissions| permissions.get(RULES_KEY));
    let digest = Sha256::digest(serde_json::to_vec(&rules).expect("settings value serializes"));
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The revision of settings that hold no Auto approval entry, such as when no settings exist.
pub fn empty_auto_settings_revision() -> AutoSettingsRevision {
    auto_settings_revision(&Map::new())
}

fn auto_settings_revision(settings: &Map<String, Value>) -> AutoSettingsRevision {
    let digest = Sha256::digest(
        serde_json::to_vec(&settings.get(AUTO_APPROVAL_KEY)).expect("settings value serializes"),
    );
    AutoSettingsRevision::new(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn parse_auto_settings(settings: &Map<String, Value>) -> Result<AutoSettings, InvalidEntry> {
    let Some(value) = settings.get(AUTO_APPROVAL_KEY) else {
        return Ok(AutoSettings::default());
    };
    let Some(auto) = value.as_object() else {
        return Err(InvalidEntry {
            key: AUTO_APPROVAL_KEY.into(),
            expected: "an object with a reviewer".into(),
        });
    };
    let schema_version = match auto.get("schemaVersion") {
        None => AUTO_SETTINGS_SCHEMA_VERSION,
        Some(value) => value.as_u64().ok_or_else(|| InvalidEntry {
            key: format!("{AUTO_APPROVAL_KEY}.schemaVersion"),
            expected: "a supported schema version".into(),
        })?,
    };
    if schema_version != AUTO_SETTINGS_SCHEMA_VERSION {
        return Err(InvalidEntry {
            key: format!("{AUTO_APPROVAL_KEY}.schemaVersion"),
            expected: format!("schema version {AUTO_SETTINGS_SCHEMA_VERSION}"),
        });
    }
    let reviewer = match auto.get("reviewer") {
        None => AutoReviewerPreference::Llm,
        Some(value) => serde_json::from_value(value.clone()).map_err(|_| InvalidEntry {
            key: format!("{AUTO_APPROVAL_KEY}.reviewer"),
            expected: "llm or jev".into(),
        })?,
    };
    let rules = match auto.get("rules") {
        None => AutoRuleSettings::default(),
        Some(value) => {
            let rules: AutoRuleSettings =
                serde_json::from_value(value.clone()).map_err(|_| InvalidEntry {
                    key: format!("{AUTO_APPROVAL_KEY}.rules"),
                    expected: "valid Auto rule groups and Environment".into(),
                })?;
            rules.validate().map_err(|reason| InvalidEntry {
                key: format!("{AUTO_APPROVAL_KEY}.rules"),
                expected: reason,
            })?;
            rules
        }
    };
    Ok(AutoSettings {
        schema_version,
        reviewer,
        rules,
    })
}

fn apply_permission_rule_changes<'a>(
    settings: &mut Map<String, Value>,
    changes: impl IntoIterator<Item = (&'a Path, &'a ProjectRuleChanges)>,
) -> Result<(), InvalidEntry> {
    for (project, change) in changes {
        let key = project_key(project);
        let projects = object_entry(settings, PROJECTS_KEY, PROJECTS_KEY.into())?;
        let location = format!("{PROJECTS_KEY}[{key:?}]");
        let project = object_entry(projects, &key, location.clone())?;
        let location = format!("{location}.{PERMISSIONS_KEY}");
        let permissions = object_entry(project, PERMISSIONS_KEY, location.clone())?;
        let Some(rules) = permissions
            .entry(RULES_KEY)
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
        else {
            return Err(InvalidEntry {
                key: format!("{location}.{RULES_KEY}"),
                expected: "an array".into(),
            });
        };
        let replaced = change
            .removed
            .iter()
            .chain(change.added.iter().map(|rule| &rule.id))
            .collect::<HashSet<_>>();
        rules.retain(|rule| {
            rule.get("id")
                .and_then(Value::as_str)
                .and_then(|id| id.parse::<Uuid>().ok())
                .is_none_or(|id| !replaced.contains(&id))
        });
        rules.extend(change.added.iter().map(|rule| {
            serde_json::to_value(SavedRule {
                id: rule.id,
                decision: rule.decision,
                tool: rule.tool.clone(),
                specifier: rule.specifier.clone(),
                input: rule.input.clone(),
            })
            .expect("permission rule serializes to JSON")
        }));
    }
    Ok(())
}

fn resolve_symlinks(path: &Path) -> PathBuf {
    let mut target = path.to_path_buf();
    for _ in 0..MAX_SYMLINK_HOPS {
        let Ok(link) = std::fs::read_link(&target) else {
            break;
        };
        target = parent_directory(&target).join(link);
    }
    target
}

fn parent_directory(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

fn project_key(project: &Path) -> String {
    project.to_string_lossy().into_owned()
}

/// Gets the object for `key` in an object; inserts an empty object when missing, and errors when
/// the value is not an object (the user's content is never overwritten).
fn object_entry<'a>(
    object: &'a mut Map<String, Value>,
    key: &str,
    location: String,
) -> Result<&'a mut Map<String, Value>, InvalidEntry> {
    object
        .entry(key)
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or(InvalidEntry {
            key: location,
            expected: "an object".into(),
        })
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, Barrier, mpsc},
        time::Duration,
    };

    use super::*;
    use tickloop_harness::protocol::CustomAutoRule;

    fn allow(tool: &str) -> PermissionRule {
        PermissionRule {
            id: Uuid::new_v4(),
            decision: Decision::Allow,
            tool: tool.into(),
            specifier: None,
            input: None,
            loop_id: None,
            persistent: true,
        }
    }

    fn change(removed: &[Uuid], added: &[PermissionRule]) -> ProjectRuleChanges {
        ProjectRuleChanges {
            removed: removed.iter().copied().collect(),
            added: added.to_vec(),
        }
    }

    #[test]
    fn selecting_an_unchanged_builtin_model_does_not_create_provider_yaml() {
        let root = tempfile::tempdir().unwrap();
        let settings = UserSettings::in_root(root.path());
        let selected = ModelCatalog::defaults()
            .get("deepseek-api", "deepseek-flash")
            .unwrap();
        settings.save_model(&selected, false).unwrap();
        assert!(!root.path().join("providers/deepseek-api.yaml").exists());
        let saved: Value =
            serde_json::from_slice(&std::fs::read(root.path().join(SETTINGS_FILE_NAME)).unwrap())
                .unwrap();
        assert!(saved.get(MODEL_KEY).is_none());
        assert_eq!(saved[DEFAULT_MODEL_KEY]["provider"], "deepseek-api");
    }

    #[test]
    fn task_tools_default_off_and_persist_only_the_boolean_preference() {
        let directory = tempfile::tempdir().unwrap();
        let settings = UserSettings::in_root(directory.path());
        assert!(!settings.load().task_tools_enabled());
        std::fs::write(&settings.path, r#"{"taskToolsEnabled":"yes","other":42}"#).unwrap();
        let mut loaded = settings.load();
        assert!(!loaded.task_tools_enabled());
        assert!(!loaded.into_warnings().is_empty());
        settings.save_task_tools_enabled(true).unwrap();
        assert!(settings.load().task_tools_enabled());
        let saved: Value = serde_json::from_slice(&std::fs::read(&settings.path).unwrap()).unwrap();
        assert_eq!(saved["other"], 42);
        settings.save_task_tools_enabled(false).unwrap();
        assert!(!settings.load().task_tools_enabled());
    }

    #[test]
    fn legacy_off_effort_migrates_to_thinking_toggle() {
        let directory = tempfile::tempdir().unwrap();
        let settings = UserSettings::in_root(directory.path());
        std::fs::write(&settings.path, r#"{"effortLevel":"off"}"#).unwrap();

        assert_eq!(settings.reasoning_effort(), None);
        assert!(!settings.thinking_enabled());
        settings
            .save_reasoning_effort(Some(ReasoningEffort::High))
            .unwrap();
        assert!(!settings.thinking_enabled());
        settings.save_thinking_enabled(true).unwrap();
        assert!(settings.thinking_enabled());
        assert_eq!(settings.reasoning_effort(), Some(ReasoningEffort::High));
        let saved: Value = serde_json::from_slice(&std::fs::read(&settings.path).unwrap()).unwrap();
        assert_eq!(saved[THINKING_ENABLED_KEY], true);
        assert_eq!(saved[EFFORT_LEVEL_KEY], "high");
    }

    /// While another process holds `settings.json.lock`, a write waits and only does its
    /// read-modify-write after release, so it never overwrites based on stale content.
    #[test]
    fn a_held_settings_lock_delays_the_write_until_release() {
        let directory = tempfile::tempdir().unwrap();
        let settings = UserSettings::in_root(directory.path());
        let project = directory.path().to_path_buf();
        let held = lock_exclusively(&settings.lock_path()).unwrap();
        let (finished_sender, finished) = mpsc::channel();
        let writer = {
            let settings = settings.clone();
            let project = project.clone();
            std::thread::spawn(move || {
                let rule = allow("Read");
                settings
                    .update_permission_rules([(project.as_path(), &change(&[], &[rule]))])
                    .unwrap();
                finished_sender.send(()).unwrap();
            })
        };
        assert!(
            finished.recv_timeout(Duration::from_millis(300)).is_err(),
            "the write must wait for the lock"
        );
        assert!(!settings.path.exists());
        std::fs::write(&settings.path, "{\"cleanupPeriodDays\": 7}").unwrap();
        drop(held);
        finished.recv_timeout(Duration::from_secs(5)).unwrap();
        writer.join().unwrap();
        let saved: Value = serde_json::from_slice(&std::fs::read(&settings.path).unwrap()).unwrap();
        assert_eq!(saved["cleanupPeriodDays"], 7);
        assert_eq!(settings.permission_rules(&project).len(), 1);
    }

    /// Several threads interleave adding and removing rules in the same partition: every rule each
    /// thread last left in place is present, and no removed rule comes back.
    #[test]
    fn interleaved_writers_keep_every_change() {
        const WRITERS: usize = 4;
        const ROUNDS: usize = 25;
        let directory = tempfile::tempdir().unwrap();
        let settings = UserSettings::in_root(directory.path());
        let project = directory.path().to_path_buf();
        let start = Arc::new(Barrier::new(WRITERS));
        let writers = (0..WRITERS)
            .map(|writer| {
                let settings = settings.clone();
                let project = project.clone();
                let start = start.clone();
                std::thread::spawn(move || {
                    start.wait();
                    for _ in 0..ROUNDS {
                        let transient = allow(&format!("transient-{writer}"));
                        settings
                            .update_permission_rules([(
                                project.as_path(),
                                &change(&[], std::slice::from_ref(&transient)),
                            )])
                            .unwrap();
                        settings
                            .update_permission_rules([(
                                project.as_path(),
                                &change(&[transient.id], &[]),
                            )])
                            .unwrap();
                    }
                    let kept = allow(&format!("kept-{writer}"));
                    settings
                        .update_permission_rules([(
                            project.as_path(),
                            &change(&[], std::slice::from_ref(&kept)),
                        )])
                        .unwrap();
                    kept.id
                })
            })
            .collect::<Vec<_>>();
        let mut kept = writers
            .into_iter()
            .map(|writer| writer.join().unwrap())
            .collect::<Vec<_>>();
        let mut saved = settings
            .permission_rules(&project)
            .into_iter()
            .map(|rule| rule.id)
            .collect::<Vec<_>>();
        kept.sort();
        saved.sort();
        assert_eq!(saved, kept);
    }

    #[test]
    fn nested_keys_of_the_wrong_type_refuse_the_write_and_report_a_warning() {
        let directory = tempfile::tempdir().unwrap();
        let settings = UserSettings::in_root(directory.path());
        let project = directory.path().join("project");
        let original = serde_json::to_string(&serde_json::json!({
            "projects": {project.to_string_lossy(): {"permissions": {"rules": {"not": "an array"}}}},
        }))
        .unwrap();
        std::fs::write(&settings.path, &original).unwrap();
        let error = settings
            .update_permission_rules([(project.as_path(), &change(&[], &[allow("Glob")]))])
            .unwrap_err();
        assert!(
            matches!(error, SettingsError::InvalidEntry { .. }),
            "{error}"
        );
        assert_eq!(std::fs::read_to_string(&settings.path).unwrap(), original);
        let mut loaded = settings.load();
        assert!(loaded.permission_rules(&project).is_empty());
        assert_eq!(loaded.warnings().len(), 1);
        assert!(loaded.warnings()[0].contains("is not an array"));
    }

    #[test]
    fn malformed_saved_rules_cannot_silently_erase_defaults() {
        let mut settings = AutoRuleSettings::default();
        settings.soft_deny.include_builtins = false;
        settings.soft_deny.custom.push(CustomAutoRule {
            id: "not-a-stable-id".into(),
            text: "Some restriction".into(),
        });
        assert!(settings.validate().is_err());
        settings.soft_deny.custom[0].id = Uuid::new_v4().to_string();
        settings.soft_deny.custom[0].text = "   ".into();
        assert!(settings.validate().is_err());
    }
}

#[cfg(feature = "test-support")]
pub mod local_agent;

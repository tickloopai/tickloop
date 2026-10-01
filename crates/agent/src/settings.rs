//! 用户设置文件 `<root>/settings.json`，对应 Claude Code 的用户设置 `~/.claude/settings.json`。
//!
//! 照 Claude Code `updateSettingsForSource`：每次写入先读取文件并合并，只改动本次涉及的键，
//! 其他键原样保留；文件不是合法 JSON、或写入路径上的键类型不对时报错且不覆盖。写入时先写
//! 临时文件再重命名，读取方不会看到写了一半的文件；设置文件是符号链接时照 Claude Code
//! `writeFileSyncAndFlush_DEPRECATED` 写入链接目标。读-改-写在 `settings.json.lock` 的独占锁内
//! 完成，多个进程并发写入不会丢失对方的改动。
//!
//! 读取时照 Claude Code `parseSettingsFile`：文件不存在或只有空白按没有设置处理；文件无效、
//! 不可读或某个值无效时该项不生效，并把问题作为警告返回，不静默丢弃（持久 Deny／Ask 规则
//! 失效是需要用户知道的）。

use std::{
    collections::{BTreeMap, HashSet},
    fs::File,
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::model::{
    ModelConfig, ReasoningEffort,
    cards::{
        ModelCatalog, ModelPatch, ModelSelection, ProviderPatch, migrate_legacy_provider_patch,
        model_for_persistent_selection, upgrade_saved_legacy_builtin_model, valid_provider_id,
    },
};
use crate::output_style::OutputStyle;
use crate::permissions::{Decision, ExecutionMode, PermissionRule};
use crate::private_fs::{PRIVATE_FILE_MODE, create_private_directory, replace_file_atomically};
use crate::runtime::auto_rules::AutoRuleSettings;

pub const SETTINGS_FILE_NAME: &str = "settings.json";

const MODEL_KEY: &str = "model";
const PROVIDERS_KEY: &str = "providers";
const PROVIDERS_DIRECTORY: &str = "providers";
const DEFAULT_MODEL_KEY: &str = "defaultModel";
const EFFORT_LEVEL_KEY: &str = "effortLevel";
const THINKING_ENABLED_KEY: &str = "alwaysThinkingEnabled";
const DEFAULT_MODE_KEY: &str = "defaultMode";
const CONTEXT_WINDOW_KEY: &str = "contextWindowTokens";
const GLYPH_SET_KEY: &str = "glyphSet";
const PREFERS_REDUCED_MOTION_KEY: &str = "prefersReducedMotion";
const TICK_LIST_KEY: &str = "tickList";
const STATUS_LINE_POSITION_KEY: &str = "statusLinePosition";
/// Claude Code's `outputStyle` setting.
const OUTPUT_STYLE_KEY: &str = "outputStyle";
/// 照 Claude Code `cleanupPeriodDays`：保留对话记录等文件的天数；0 表示不写 Transcript。
const CLEANUP_PERIOD_DAYS_KEY: &str = "cleanupPeriodDays";
/// 照 Claude Code `DEFAULT_CLEANUP_PERIOD_DAYS`。
pub const DEFAULT_CLEANUP_PERIOD_DAYS: u64 = 30;
/// 按项目目录分区的设置，参照 Claude Code 全局配置中的 `projects[<目录>]`。
const PROJECTS_KEY: &str = "projects";
const PERMISSIONS_KEY: &str = "permissions";
const RULES_KEY: &str = "rules";
const AUTO_APPROVAL_KEY: &str = "autoApproval";
const AUTO_SETTINGS_SCHEMA_VERSION: u64 = 1;
const TRUST_ACCEPTED_KEY: &str = "hasTrustDialogAccepted";
/// 替换设置文件时沿用的原文件权限位。
const PRESERVED_PERMISSION_BITS: u32 = 0o777;
const LOCK_FILE_SUFFIX: &str = ".lock";
/// 解析符号链接的最大层数，与 Linux `MAXSYMLINKS` 相同；Claude Code 只解析一层。
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
    #[error("invalid provider YAML {path}: {reason}")]
    InvalidProviderYaml { path: PathBuf, reason: String },
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AutoReviewerPreference {
    #[default]
    Llm,
    Jev,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AutoSettingsRevision(String);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AutoSettings {
    pub schema_version: u64,
    pub reviewer: AutoReviewerPreference,
    pub rules: AutoRuleSettings,
}

impl Default for AutoSettings {
    fn default() -> Self {
        Self {
            schema_version: AUTO_SETTINGS_SCHEMA_VERSION,
            reviewer: AutoReviewerPreference::Llm,
            rules: AutoRuleSettings::default(),
        }
    }
}

/// 写入路径上类型不对的键；`update` 补上文件路径换成 [`SettingsError::InvalidEntry`]。
struct InvalidEntry {
    key: String,
    expected: String,
}

/// 设置文件中的持久权限规则；所属项目由它所在的 `projects` 分区表达，因此不带 Loop。
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

/// 一个项目分区中持久规则的改动：先删除 id 在 `removed` 或 `added` 中的规则，再追加 `added`，
/// 因此内容改变的规则（同 id）被替换。
#[derive(Clone, Debug, Default)]
pub(crate) struct ProjectRuleChanges {
    pub(crate) removed: HashSet<Uuid>,
    pub(crate) added: Vec<PermissionRule>,
}

#[derive(Clone, Debug)]
pub struct UserSettings {
    path: PathBuf,
    transient_providers: Arc<Mutex<BTreeMap<String, ProviderPatch>>>,
}

/// 一次读取的设置内容，以及读取各项时发现的问题。
#[derive(Debug)]
pub struct LoadedSettings {
    path: PathBuf,
    settings: Map<String, Value>,
    warnings: Vec<String>,
    read_error: Option<String>,
}

/// 后台清理使用的清理天数，见 [`UserSettings::cleanup_period_for_cleanup`]。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CleanupPeriod {
    Days(u64),
    /// 设置有问题且显式配置了清理天数，本次不清理。
    SkipInvalidSettings,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum GlyphSet {
    #[default]
    Unicode,
    NerdFont,
}

impl GlyphSet {
    pub const ALL: [Self; 2] = [Self::Unicode, Self::NerdFont];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Unicode => "Unicode",
            Self::NerdFont => "Nerd Font",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum StatusLinePosition {
    Left,
    #[default]
    Right,
}

impl StatusLinePosition {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Left => "Left",
            Self::Right => "Right",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DisplayPreferences {
    pub glyph_set: GlyphSet,
    pub prefers_reduced_motion: bool,
    pub status_line_position: StatusLinePosition,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TickListPreference {
    #[default]
    Auto,
    Hidden,
}

impl LoadedSettings {
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

    pub fn jev_config(&self) -> Result<crate::judgment::JevConfig, String> {
        if let Some(error) = &self.read_error {
            return Err(error.clone());
        }
        match self.settings.get("jev") {
            Some(value) => serde_json::from_value(value.clone()).map_err(|_| {
                "invalid Jev settings; expected endpoint and model, with the key in Agent .env"
                    .into()
            }),
            None => Ok(crate::judgment::JevConfig::default()),
        }
    }

    pub fn model_catalog(&self) -> Result<ModelCatalog, String> {
        if let Some(error) = &self.read_error {
            return Err(error.clone());
        }
        catalog_from_settings(&self.settings, parent_directory(&self.path))
    }

    pub fn configured_model(&mut self) -> Result<Option<ModelConfig>, String> {
        let catalog = self.model_catalog()?;
        let default_model = self
            .settings
            .get(DEFAULT_MODEL_KEY)
            .cloned()
            .map(|value| {
                let selection: ModelSelection = serde_json::from_value(value)
                    .map_err(|error| format!("`{DEFAULT_MODEL_KEY}`: {error}"))?;
                catalog
                    .get(&selection.provider, &selection.model)
                    .ok_or_else(|| {
                        format!(
                            "`{DEFAULT_MODEL_KEY}`: unknown provider/model {}/{}",
                            selection.provider, selection.model
                        )
                    })
            })
            .transpose()?;
        if let Some(value) = self.settings.get(MODEL_KEY).cloned() {
            let saved: ModelConfig =
                serde_json::from_value(value).map_err(|error| format!("`{MODEL_KEY}`: {error}"))?;
            let was_legacy_profile = saved.card.is_none();
            let migrated = upgrade_saved_legacy_builtin_model(saved);
            if was_legacy_profile
                && migrated.card.is_some()
                && !migrated.provider_id.is_empty()
                && let Some(current) = catalog.get(&migrated.provider_id, &migrated.model_id)
            {
                return Ok(Some(current));
            }
            crate::model::validate_model_config(&migrated)
                .map_err(|error| format!("`{MODEL_KEY}`: {error}"))?;
            return Ok(Some(migrated));
        }
        Ok(default_model)
    }

    /// `/model` 保存的模型配置，格式与 `--config` 文件相同。
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
        let value = self.settings.get(CONTEXT_WINDOW_KEY)?.clone();
        serde_json::from_value(value)
            .map_err(|error| {
                self.warn(format!(
                    "`{CONTEXT_WINDOW_KEY}` is not a token count and is ignored ({error})"
                ))
            })
            .ok()
    }

    pub fn default_execution_mode(&mut self) -> Option<ExecutionMode> {
        let value = self.settings.get(DEFAULT_MODE_KEY)?.clone();
        if let Ok(mode) = serde_json::from_value(value.clone()) {
            return Some(mode);
        }
        let legacy = value.as_str();
        let mode = match legacy {
            Some("default" | "manual") => ExecutionMode::AcceptEdits,
            Some("dontAsk" | "bypass") => ExecutionMode::Plan,
            _ => ExecutionMode::Plan,
        };
        self.warn(format!(
            "`{DEFAULT_MODE_KEY}` has unsupported value {value}; using {mode} conservatively"
        ));
        Some(mode)
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

    pub fn tick_list_preference(&self) -> TickListPreference {
        self.settings
            .get(TICK_LIST_KEY)
            .cloned()
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap_or_default()
    }

    pub fn display_preferences(&mut self) -> DisplayPreferences {
        let glyph_set = self
            .settings
            .get(GLYPH_SET_KEY)
            .cloned()
            .map(|value| {
                serde_json::from_value(value).unwrap_or_else(|error| {
                    self.warn(format!(
                        "`{GLYPH_SET_KEY}` is not `unicode` or `nerd-font` and is ignored ({error})"
                    ));
                    GlyphSet::default()
                })
            })
            .unwrap_or_default();
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
        let status_line_position = self
            .settings
            .get(STATUS_LINE_POSITION_KEY)
            .cloned()
            .map(|value| {
                serde_json::from_value(value).unwrap_or_else(|error| {
                    self.warn(format!(
                        "`{STATUS_LINE_POSITION_KEY}` is not `left` or `right` and is ignored ({error})"
                    ));
                    StatusLinePosition::default()
                })
            })
            .unwrap_or_default();
        DisplayPreferences {
            glyph_set,
            prefers_reduced_motion,
            status_line_position,
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

    /// 保留对话记录等文件的天数；不是非负整数时不生效并记为警告。
    pub fn cleanup_period_days(&mut self) -> Option<u64> {
        let value = self.settings.get(CLEANUP_PERIOD_DAYS_KEY)?;
        if let Some(days) = value.as_u64() {
            return Some(days);
        }
        self.warn(format!(
            "`{CLEANUP_PERIOD_DAYS_KEY}` is not a whole number of days (0 or more) and is ignored; old conversations are not cleaned up until it is fixed"
        ));
        None
    }

    /// 照 Claude Code `getSettingsWithAllErrors`：检查文件中所有已知设置，包括每个项目分区的规则，
    /// 返回是否发现问题。
    fn has_any_problem(&mut self) -> bool {
        self.model();
        self.default_execution_mode();
        self.context_window_tokens();
        self.cleanup_period_days();
        self.display_preferences();
        let projects = self
            .settings
            .get(PROJECTS_KEY)
            .and_then(Value::as_object)
            .map(|projects| projects.keys().cloned().collect::<Vec<_>>());
        match projects {
            Some(projects) => {
                for project in projects {
                    self.permission_rules(Path::new(&project));
                }
            }
            None if self.settings.contains_key(PROJECTS_KEY) => {
                self.permission_rules(Path::new(""));
            }
            None => {}
        }
        !self.warnings.is_empty()
    }

    /// 在 `project`（规范化的工作目录）中保存的持久权限规则；无效的条目被跳过并记为警告。
    pub fn permission_rules(&mut self, project: &Path) -> Vec<PermissionRule> {
        let mut problems = Vec::new();
        let rules = saved_project_rules(&self.settings, project, &mut problems);
        for problem in problems {
            self.warn(problem);
        }
        rules
    }

    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    pub fn into_warnings(self) -> Vec<String> {
        self.warnings
    }

    pub(crate) fn warn(&mut self, problem: String) {
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
                    Some(PermissionRule::from(rule))
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

fn legacy_provider_patches(settings: &Map<String, Value>) -> Result<Vec<ProviderPatch>, String> {
    settings
        .get(PROVIDERS_KEY)
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|error| format!("`{PROVIDERS_KEY}`: {error}"))
        .map(|patches: Option<Vec<ProviderPatch>>| {
            let mut patches = patches.unwrap_or_default();
            for patch in &mut patches {
                migrate_legacy_provider_patch(patch);
            }
            patches
        })
}

fn remove_legacy_model_override(
    settings: &mut Map<String, Value>,
    provider_id: &str,
    model_id: &str,
) -> Result<bool, String> {
    if !settings.contains_key(PROVIDERS_KEY) {
        return Ok(false);
    }
    let mut configured = legacy_provider_patches(settings)?;
    let mut removed = false;
    if let Some(provider) = configured
        .iter_mut()
        .find(|provider| provider.id == provider_id)
        && let Some(models) = &mut provider.models
    {
        let previous_count = models.len();
        models.retain(|model| model.id != model_id);
        removed = models.len() != previous_count;
    }
    if removed {
        settings.insert(
            PROVIDERS_KEY.into(),
            serde_json::to_value(configured).expect("providers serialize"),
        );
    }
    Ok(removed)
}

fn remove_legacy_provider_override(
    settings: &mut Map<String, Value>,
    provider_id: &str,
) -> Result<bool, String> {
    if !settings.contains_key(PROVIDERS_KEY) {
        return Ok(false);
    }
    let mut configured = legacy_provider_patches(settings)?;
    let previous_count = configured.len();
    configured.retain(|provider| provider.id != provider_id);
    if configured.len() == previous_count {
        return Ok(false);
    }
    settings.insert(
        PROVIDERS_KEY.into(),
        serde_json::to_value(configured).expect("providers serialize"),
    );
    Ok(true)
}

fn provider_directory(root: &Path) -> PathBuf {
    root.join(PROVIDERS_DIRECTORY)
}

fn provider_path(root: &Path, provider_id: &str) -> Result<PathBuf, String> {
    if !valid_provider_id(provider_id) {
        return Err(format!(
            "providers[{provider_id}]: id must be a safe lowercase slug"
        ));
    }
    Ok(provider_directory(root).join(format!("{provider_id}.yaml")))
}

fn provider_patch_is_empty(patch: &ProviderPatch) -> bool {
    if patch.connection.is_some()
        || patch
            .model_aliases
            .as_ref()
            .is_some_and(|aliases| !aliases.is_empty())
    {
        return false;
    }
    patch.default_model.is_none()
        && patch.display_name.is_none()
        && patch.auth.is_none()
        && patch.api_key_env.is_none()
        && patch
            .endpoints
            .as_ref()
            .is_none_or(|value| value.is_empty())
        && patch.models.as_ref().is_none_or(|value| value.is_empty())
}

fn read_provider_patch(path: &Path, expected_id: &str) -> Result<ProviderPatch, String> {
    let metadata =
        std::fs::symlink_metadata(path).map_err(|error| format!("{}: {error}", path.display()))?;
    if !metadata.file_type().is_file() {
        return Err(format!(
            "{}: provider YAML must be a regular file",
            path.display()
        ));
    }
    let contents =
        std::fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let patch: ProviderPatch = serde_yaml_ng::from_str(&contents)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    if patch.id != expected_id {
        return Err(format!(
            "{}: file name must match provider id `{}`",
            path.display(),
            patch.id
        ));
    }
    validate_patch_model_ids(&patch).map_err(|reason| format!("{}: {reason}", path.display()))?;
    Ok(patch)
}

fn validate_patch_model_ids(patch: &ProviderPatch) -> Result<(), String> {
    let mut ids = HashSet::new();
    for model in patch.models.iter().flatten() {
        if !ids.insert(&model.id) {
            return Err(format!(
                "providers[{}].models[{}]: duplicate model id",
                patch.id, model.id
            ));
        }
    }
    Ok(())
}

fn directory_provider_patches(root: &Path) -> Result<Vec<ProviderPatch>, String> {
    let directory = provider_directory(root);
    match std::fs::symlink_metadata(&directory) {
        Ok(metadata) if !metadata.file_type().is_dir() => {
            return Err(format!(
                "{}: providers path must be a real directory",
                directory.display()
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("{}: {error}", directory.display())),
        Ok(_) => {}
    }
    let entries = match std::fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) => return Err(format!("{}: {error}", directory.display())),
    };
    let mut patches = Vec::new();
    let mut ids = HashSet::new();
    for entry in entries {
        let entry = entry.map_err(|error| format!("{}: {error}", directory.display()))?;
        let path = entry.path();
        if path.extension().is_none_or(|extension| extension != "yaml") {
            continue;
        }
        let id = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .ok_or_else(|| format!("{}: invalid provider file name", path.display()))?;
        if !valid_provider_id(id) {
            return Err(format!("{}: invalid provider file name", path.display()));
        }
        let patch = read_provider_patch(&path, id)?;
        if !ids.insert(patch.id.clone()) {
            return Err(format!(
                "{}: duplicate provider id `{}`",
                path.display(),
                patch.id
            ));
        }
        patches.push(patch);
    }
    patches.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(patches)
}

fn combined_provider_patches(
    settings: &Map<String, Value>,
    directory_patches: Vec<ProviderPatch>,
) -> Result<Vec<ProviderPatch>, String> {
    let mut legacy = legacy_provider_patches(settings)?;
    let mut legacy_ids = HashSet::new();
    for patch in &legacy {
        validate_patch_model_ids(patch)?;
        if !legacy_ids.insert(&patch.id) {
            return Err(format!(
                "`{PROVIDERS_KEY}`: duplicate provider id `{}`",
                patch.id
            ));
        }
    }
    for patch in directory_patches {
        if let Some(existing) = legacy.iter_mut().find(|legacy| legacy.id == patch.id) {
            existing.merge_from(patch)?;
        } else {
            legacy.push(patch);
        }
    }
    Ok(legacy)
}

fn catalog_from_settings(
    settings: &Map<String, Value>,
    root: &Path,
) -> Result<ModelCatalog, String> {
    let patches = combined_provider_patches(settings, directory_provider_patches(root)?)?;
    ModelCatalog::defaults().with_providers(patches)
}

fn validate_saved_selection(
    settings: &Map<String, Value>,
    catalog: &ModelCatalog,
) -> Result<(), String> {
    let Some(value) = settings.get(DEFAULT_MODEL_KEY) else {
        return Ok(());
    };
    let selection: ModelSelection = serde_json::from_value(value.clone())
        .map_err(|error| format!("`{DEFAULT_MODEL_KEY}`: {error}"))?;
    if catalog.get(&selection.provider, &selection.model).is_none() {
        return Err(format!(
            "`{DEFAULT_MODEL_KEY}`: selected model {}/{} would be removed; select another model first",
            selection.provider, selection.model
        ));
    }
    Ok(())
}

impl UserSettings {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            transient_providers: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    /// Tick Loop 状态根目录下的设置文件。
    pub fn in_root(root: &Path) -> Self {
        Self::new(root.join(SETTINGS_FILE_NAME))
    }

    pub(crate) fn root_directory(&self) -> Option<&Path> {
        self.path.parent()
    }

    pub fn model_catalog(&self) -> Result<ModelCatalog, String> {
        self.persisted_model_catalog()?
            .with_providers(self.transient_provider_patches())
    }

    pub fn persisted_model_catalog(&self) -> Result<ModelCatalog, String> {
        let object = self.read_object().map_err(|error| error.to_string())?;
        catalog_from_settings(&object, parent_directory(&self.path))
    }

    pub fn set_transient_provider(&self, provider: ProviderPatch) -> Option<ProviderPatch> {
        self.transient_providers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(provider.id.clone(), provider)
    }

    pub fn restore_transient_provider(&self, provider_id: &str, previous: Option<ProviderPatch>) {
        let mut providers = self
            .transient_providers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(previous) = previous {
            providers.insert(provider_id.into(), previous);
        } else {
            providers.remove(provider_id);
        }
    }

    fn transient_provider_patches(&self) -> Vec<ProviderPatch> {
        self.transient_providers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .cloned()
            .collect()
    }

    pub(crate) fn root(&self) -> Option<&Path> {
        self.path.parent()
    }

    /// 读取设置文件。文件无效或不可读时内容为空，问题记为警告。清理天数是进程级设置，
    /// 每次读取都检查它，让无效的值和其他设置问题一样出现在警告中。
    pub fn load(&self) -> LoadedSettings {
        let (settings, warnings, read_error) = match self.read_object() {
            Ok(settings) => (settings, Vec::new(), None),
            Err(error) => {
                let message = error.to_string();
                (
                    Map::new(),
                    vec![format!(
                        "{message}; the saved model, persistent permission rules, context window and cleanup period are ignored"
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
        loaded
    }

    /// 清理天数为 0 时不写 Transcript（照 Claude Code `shouldSkipPersistence`）。
    pub fn skips_transcripts(&self) -> bool {
        self.load().cleanup_period_days() == Some(0)
    }

    /// 照 Claude Code `cleanupOldMessageFilesInBackground` 的前置检查：设置有任何问题、且文件中显式
    /// 配置了清理天数时跳过清理，不回退到默认天数，以免按用户没打算的期限删除文件；否则使用配置的
    /// 天数或默认 30 天。
    ///
    /// Claude Code `rawSettingsContainsKey` 在文件不是合法 JSON 或不可读时判断不出这个键，于是按默认
    /// 天数清理。这里更保守：文件不可读时跳过；不是合法 JSON 时只要文本中出现这个键名就跳过。
    pub fn cleanup_period_for_cleanup(&self) -> CleanupPeriod {
        match self.read_object() {
            Ok(settings) => {
                let mut loaded = LoadedSettings {
                    path: self.path.clone(),
                    settings,
                    warnings: Vec::new(),
                    read_error: None,
                };
                let explicit = loaded.settings.contains_key(CLEANUP_PERIOD_DAYS_KEY);
                if loaded.has_any_problem() && explicit {
                    return CleanupPeriod::SkipInvalidSettings;
                }
                CleanupPeriod::Days(
                    loaded
                        .cleanup_period_days()
                        .unwrap_or(DEFAULT_CLEANUP_PERIOD_DAYS),
                )
            }
            Err(SettingsError::InvalidJson(_) | SettingsError::NotAnObject(_)) => {
                let quoted_key = format!("\"{CLEANUP_PERIOD_DAYS_KEY}\"");
                let mentions_key = std::fs::read(&self.path).is_ok_and(|bytes| {
                    bytes
                        .windows(quoted_key.len())
                        .any(|window| window == quoted_key.as_bytes())
                });
                if mentions_key {
                    CleanupPeriod::SkipInvalidSettings
                } else {
                    CleanupPeriod::Days(DEFAULT_CLEANUP_PERIOD_DAYS)
                }
            }
            Err(_) => CleanupPeriod::SkipInvalidSettings,
        }
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

    pub fn context_window_tokens(&self) -> Option<usize> {
        self.load().context_window_tokens()
    }

    pub fn default_execution_mode(&self) -> Option<ExecutionMode> {
        self.load().default_execution_mode()
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

    /// 保存模型选择；`clear_context_window` 为真时同时删除上下文窗口设置（窗口属于之前的模型）。
    pub(crate) fn save_model(
        &self,
        model: &ModelConfig,
        clear_context_window: bool,
    ) -> Result<(), SettingsError> {
        self.save_model_with_selection(model, clear_context_window, None)
    }

    pub(crate) fn save_model_with_thinking(
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
        let model = model_for_persistent_selection(model.clone());
        crate::model::validate_model_config(&model).map_err(|_| SettingsError::InvalidEntry {
            path: self.path.clone(),
            key: MODEL_KEY.into(),
            expected: "a valid model configuration".into(),
        })?;
        if let Some(effort) = thinking_choice {
            crate::model::validate_reasoning_effort(&model, effort).map_err(|_| {
                SettingsError::InvalidEntry {
                    path: self.path.clone(),
                    key: EFFORT_LEVEL_KEY.into(),
                    expected: "an effort supported by the selected model".into(),
                }
            })?;
        }
        if let Some(card) = &model.card {
            let selected_model_patch = ProviderPatch {
                model_aliases: None,
                connection: model.connection.clone(),
                id: model.provider_id.clone(),
                default_model: None,
                display_name: Some(model.provider_name.clone()),
                auth: Some(model.auth_source),
                api_key_env: Some(model.api_key_env.clone()),
                endpoints: Some(std::collections::BTreeMap::from([(
                    card.adapter,
                    model.base_url.clone(),
                )])),
                models: Some(vec![ModelPatch {
                    id: card.id.clone(),
                    service_model_id: Some(card.service_model_id.clone()),
                    display_name: Some(card.display_name.clone()),
                    adapter: Some(card.adapter),
                    max_output_tokens: Some(card.max_output_tokens),
                    context_window_tokens: Some(card.context_window_tokens),
                    thinking: Some(card.thinking),
                    adaptive_by_default: Some(card.adaptive_by_default),
                    effort_levels: Some(card.effort_levels.clone()),
                    input_modalities: Some(card.input_modalities.clone()),
                    thinking_budgets: card.thinking_budgets,
                }]),
            };
            let path = self.provider_path(&model.provider_id)?;
            let _lock = self.lock_provider(&path)?;
            let settings = self.read_object()?;
            let current_catalog = catalog_from_settings(&settings, parent_directory(&self.path))
                .map_err(|reason| SettingsError::InvalidProviderYaml {
                    path: path.clone(),
                    reason,
                })?;
            let transient_provider =
                self.transient_provider_patches()
                    .into_iter()
                    .find(|candidate| {
                        candidate.id == model.provider_id
                            && current_catalog
                                .clone()
                                .with_providers(vec![candidate.clone()])
                                .ok()
                                .and_then(|catalog| {
                                    catalog.get(&model.provider_id, &model.model_id)
                                })
                                .as_ref()
                                == Some(&model)
                    });
            let has_transient_provider = transient_provider.is_some();
            let provider = transient_provider.unwrap_or(selected_model_patch);
            if !has_transient_provider
                && current_catalog
                    .get(&model.provider_id, &model.model_id)
                    .as_ref()
                    == Some(&model)
            {
                return self.save_model_selection(&model, clear_context_window, thinking_choice);
            }
            let original_yaml = match std::fs::read(&path) {
                Ok(contents) => Some(contents),
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                Err(source) => return Err(SettingsError::Read { path, source }),
            };
            let catalog = self.save_provider_locked(&path, provider)?;
            if catalog.get(&model.provider_id, &model.model_id).as_ref() != Some(&model) {
                self.restore_provider_bytes(&path, original_yaml)?;
                return Err(SettingsError::InvalidProviderYaml {
                    path,
                    reason: "saved Provider definition does not match the selected model".into(),
                });
            }
            if let Err(error) =
                self.save_model_selection(&model, clear_context_window, thinking_choice)
            {
                self.restore_provider_bytes(&path, original_yaml)?;
                return Err(error);
            }
            return Ok(());
        }
        Err(SettingsError::InvalidEntry {
            path: self.path.clone(),
            key: MODEL_KEY.into(),
            expected: "a model with a supported Provider and Model Card".into(),
        })
    }

    fn save_model_selection(
        &self,
        model: &ModelConfig,
        clear_context_window: bool,
        thinking_choice: Option<Option<ReasoningEffort>>,
    ) -> Result<(), SettingsError> {
        self.update(|settings| {
            settings.remove(MODEL_KEY);
            settings.insert(
                DEFAULT_MODEL_KEY.into(),
                serde_json::to_value(ModelSelection {
                    provider: model.provider_id.clone(),
                    model: model.model_id.clone(),
                })
                .expect("model selection serializes"),
            );
            if clear_context_window {
                settings.remove(CONTEXT_WINDOW_KEY);
            }
            if let Some(effort) = thinking_choice {
                match effort {
                    Some(effort) => {
                        settings.insert(
                            EFFORT_LEVEL_KEY.into(),
                            serde_json::to_value(effort).expect("reasoning effort serializes"),
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

    pub fn save_provider(&self, provider: ProviderPatch) -> Result<(), SettingsError> {
        let path = self.provider_path(&provider.id)?;
        let _lock = self.lock_provider(&path)?;
        self.save_provider_locked(&path, provider).map(|_| ())
    }

    fn save_provider_locked(
        &self,
        path: &Path,
        provider: ProviderPatch,
    ) -> Result<ModelCatalog, SettingsError> {
        let mut saved = if std::fs::symlink_metadata(path).is_ok() {
            read_provider_patch(path, &provider.id).map_err(|reason| {
                SettingsError::InvalidProviderYaml {
                    path: path.to_path_buf(),
                    reason,
                }
            })?
        } else {
            let settings = self.read_object()?;
            legacy_provider_patches(&settings)
                .map_err(|reason| SettingsError::InvalidProviderYaml {
                    path: self.path.clone(),
                    reason,
                })?
                .into_iter()
                .find(|legacy| legacy.id == provider.id)
                .unwrap_or_else(|| ProviderPatch {
                    model_aliases: None,
                    connection: None,
                    id: provider.id.clone(),
                    default_model: None,
                    display_name: None,
                    auth: None,
                    api_key_env: None,
                    endpoints: None,
                    models: None,
                })
        };
        saved
            .merge_from(provider)
            .map_err(|reason| SettingsError::InvalidProviderYaml {
                path: path.to_path_buf(),
                reason,
            })?;
        let settings = self.read_object()?;
        let mut directory =
            directory_provider_patches(parent_directory(&self.path)).map_err(|reason| {
                SettingsError::InvalidProviderYaml {
                    path: path.to_path_buf(),
                    reason,
                }
            })?;
        if let Some(existing) = directory
            .iter_mut()
            .find(|existing| existing.id == saved.id)
        {
            *existing = saved.clone();
        } else {
            directory.push(saved.clone());
        }
        let catalog = combined_provider_patches(&settings, directory)
            .and_then(|patches| ModelCatalog::defaults().with_providers(patches))
            .map_err(|reason| SettingsError::InvalidProviderYaml {
                path: path.to_path_buf(),
                reason,
            })?;
        self.write_provider_patch(path, &saved)?;
        Ok(catalog)
    }

    fn restore_provider_bytes(
        &self,
        path: &Path,
        original_yaml: Option<Vec<u8>>,
    ) -> Result<(), SettingsError> {
        match original_yaml {
            Some(contents) => {
                replace_file_atomically(path, &contents, PRIVATE_FILE_MODE).map_err(|source| {
                    SettingsError::Write {
                        path: path.to_path_buf(),
                        source,
                    }
                })
            }
            None => self.remove_provider_file(path),
        }
    }

    pub fn restore_provider_default(&self, provider_id: &str) -> Result<(), SettingsError> {
        let path = self.provider_path(provider_id)?;
        let _lock = self.lock_provider(&path)?;
        let original_yaml = match std::fs::read(&path) {
            Ok(contents) => Some(contents),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(source) => return Err(SettingsError::Read { path, source }),
        };
        let mut preview_settings = self.read_object()?;
        let remove_legacy = remove_legacy_provider_override(&mut preview_settings, provider_id)
            .map_err(|reason| SettingsError::InvalidProviderYaml {
                path: self.path.clone(),
                reason,
            })?;
        let mut directory =
            directory_provider_patches(parent_directory(&self.path)).map_err(|reason| {
                SettingsError::InvalidProviderYaml {
                    path: path.clone(),
                    reason,
                }
            })?;
        directory.retain(|existing| existing.id != provider_id);
        let prospective = combined_provider_patches(&preview_settings, directory)
            .and_then(|patches| ModelCatalog::defaults().with_providers(patches))
            .map_err(|reason| SettingsError::InvalidProviderYaml {
                path: path.clone(),
                reason,
            })?;
        validate_saved_selection(&preview_settings, &prospective).map_err(|reason| {
            SettingsError::InvalidProviderYaml {
                path: path.clone(),
                reason,
            }
        })?;
        self.remove_provider_file(&path)?;
        if remove_legacy
            && let Err(error) = self.update(|settings| {
                remove_legacy_provider_override(settings, provider_id).map_err(|reason| {
                    InvalidEntry {
                        key: PROVIDERS_KEY.into(),
                        expected: reason,
                    }
                })?;
                Ok(())
            })
        {
            if let Some(contents) = original_yaml
                && let Err(source) = replace_file_atomically(&path, &contents, PRIVATE_FILE_MODE)
            {
                return Err(SettingsError::Write { path, source });
            }
            return Err(error);
        }
        Ok(())
    }

    pub fn restore_model_default(
        &self,
        provider_id: &str,
        model_id: &str,
    ) -> Result<(), SettingsError> {
        let path = self.provider_path(provider_id)?;
        let _lock = self.lock_provider(&path)?;
        let original_yaml = match std::fs::read(&path) {
            Ok(contents) => Some(contents),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(source) => {
                return Err(SettingsError::Read {
                    path: path.clone(),
                    source,
                });
            }
        };
        let current =
            self.model_catalog()
                .map_err(|reason| SettingsError::InvalidProviderYaml {
                    path: path.clone(),
                    reason,
                })?;
        let canonical = current
            .canonical_model_id(provider_id, model_id)
            .unwrap_or(model_id);
        let mut equivalent_ids =
            std::collections::BTreeSet::from([model_id.to_owned(), canonical.to_owned()]);
        if let Some(provider) = current
            .providers
            .iter()
            .find(|provider| provider.id == provider_id)
        {
            equivalent_ids.extend(
                provider
                    .model_aliases
                    .iter()
                    .filter(|(_, target)| target.as_str() == canonical)
                    .map(|(alias, _)| alias.clone()),
            );
        }
        let mut patch = if original_yaml.is_some() {
            Some(read_provider_patch(&path, provider_id).map_err(|reason| {
                SettingsError::InvalidProviderYaml {
                    path: path.clone(),
                    reason,
                }
            })?)
        } else {
            None
        };
        if let Some(models) = patch.as_mut().and_then(|patch| patch.models.as_mut()) {
            models.retain(|model| !equivalent_ids.contains(&model.id));
        }
        let mut preview_settings = self.read_object()?;
        let mut remove_legacy = false;
        for id in &equivalent_ids {
            remove_legacy |= remove_legacy_model_override(&mut preview_settings, provider_id, id)
                .map_err(|reason| SettingsError::InvalidProviderYaml {
                path: self.path.clone(),
                reason,
            })?;
        }
        let mut directory =
            directory_provider_patches(parent_directory(&self.path)).map_err(|reason| {
                SettingsError::InvalidProviderYaml {
                    path: path.clone(),
                    reason,
                }
            })?;
        directory.retain(|existing| existing.id != provider_id);
        if let Some(patch) = &patch
            && !provider_patch_is_empty(patch)
        {
            directory.push(patch.clone());
        }
        let prospective = combined_provider_patches(&preview_settings, directory)
            .and_then(|patches| ModelCatalog::defaults().with_providers(patches))
            .map_err(|reason| SettingsError::InvalidProviderYaml {
                path: path.clone(),
                reason,
            })?;
        validate_saved_selection(&preview_settings, &prospective).map_err(|reason| {
            SettingsError::InvalidProviderYaml {
                path: path.clone(),
                reason,
            }
        })?;
        if let Some(patch) = &patch {
            if provider_patch_is_empty(patch) {
                self.remove_provider_file(&path)?;
            } else {
                self.write_provider_patch(&path, patch)?;
            }
        }
        if remove_legacy
            && let Err(error) = self.update(|settings| {
                for id in &equivalent_ids {
                    remove_legacy_model_override(settings, provider_id, id).map_err(|reason| {
                        InvalidEntry {
                            key: PROVIDERS_KEY.into(),
                            expected: reason,
                        }
                    })?;
                }
                Ok(())
            })
        {
            let rollback = match original_yaml {
                Some(contents) => replace_file_atomically(&path, &contents, PRIVATE_FILE_MODE),
                None => match std::fs::remove_file(&path) {
                    Ok(()) => Ok(()),
                    Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(()),
                    Err(source) => Err(source),
                },
            };
            if let Err(source) = rollback {
                return Err(SettingsError::Write { path, source });
            }
            return Err(error);
        }
        Ok(())
    }

    fn provider_path(&self, provider_id: &str) -> Result<PathBuf, SettingsError> {
        provider_path(parent_directory(&self.path), provider_id).map_err(|reason| {
            SettingsError::InvalidProviderYaml {
                path: provider_directory(parent_directory(&self.path)),
                reason,
            }
        })
    }

    fn lock_provider(&self, path: &Path) -> Result<File, SettingsError> {
        create_private_directory(parent_directory(path)).map_err(|source| {
            SettingsError::Write {
                path: path.to_path_buf(),
                source,
            }
        })?;
        let metadata = std::fs::symlink_metadata(parent_directory(path)).map_err(|source| {
            SettingsError::Read {
                path: parent_directory(path).to_path_buf(),
                source,
            }
        })?;
        if !metadata.file_type().is_dir() {
            return Err(SettingsError::InvalidProviderYaml {
                path: parent_directory(path).to_path_buf(),
                reason: "providers path must be a real directory".into(),
            });
        }
        let mut lock_name = path.as_os_str().to_os_string();
        lock_name.push(LOCK_FILE_SUFFIX);
        let lock_path = PathBuf::from(lock_name);
        lock_exclusively(&lock_path).map_err(|source| SettingsError::Lock {
            path: lock_path,
            source,
        })
    }

    fn write_provider_patch(
        &self,
        path: &Path,
        patch: &ProviderPatch,
    ) -> Result<(), SettingsError> {
        let mut contents =
            serde_yaml_ng::to_string(patch).expect("provider patch serializes to YAML");
        if !contents.ends_with('\n') {
            contents.push('\n');
        }
        replace_file_atomically(path, contents.as_bytes(), PRIVATE_FILE_MODE).map_err(|source| {
            SettingsError::Write {
                path: path.to_path_buf(),
                source,
            }
        })
    }

    fn remove_provider_file(&self, path: &Path) -> Result<(), SettingsError> {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(SettingsError::Write {
                path: path.to_path_buf(),
                source,
            }),
        }
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

    pub(crate) fn save_thinking_enabled_with_effort(
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

    pub(crate) fn save_context_window(&self, tokens: usize) -> Result<(), SettingsError> {
        self.update(|settings| {
            settings.insert(CONTEXT_WINDOW_KEY.into(), tokens.into());
            Ok(())
        })
    }

    pub fn save_tick_list_preference(
        &self,
        preference: TickListPreference,
    ) -> Result<(), SettingsError> {
        self.update(|settings| {
            settings.insert(
                TICK_LIST_KEY.into(),
                serde_json::to_value(preference).expect("tick list preference serializes to JSON"),
            );
            Ok(())
        })
    }

    pub fn save_display_preferences(
        &self,
        preferences: DisplayPreferences,
    ) -> Result<(), SettingsError> {
        self.update(|settings| {
            settings.insert(
                GLYPH_SET_KEY.into(),
                serde_json::to_value(preferences.glyph_set).expect("glyph set serializes to JSON"),
            );
            settings.insert(
                PREFERS_REDUCED_MOTION_KEY.into(),
                preferences.prefers_reduced_motion.into(),
            );
            settings.insert(
                STATUS_LINE_POSITION_KEY.into(),
                serde_json::to_value(preferences.status_line_position)
                    .expect("status line position serializes to JSON"),
            );
            Ok(())
        })
    }

    /// 按项目分区写入持久规则的改动，其他项目与键不变；路径上任何键类型不对时整体不写。
    pub(crate) fn update_permission_rules<'a>(
        &self,
        changes: impl IntoIterator<Item = (&'a Path, &'a ProjectRuleChanges)>,
    ) -> Result<(), SettingsError> {
        self.update(|settings| apply_permission_rule_changes(settings, changes))
    }

    /// Returns the project rules and a revision of their normalized saved JSON.
    /// The revision is checked again inside the settings file lock before an edit.
    pub(crate) fn project_rules_with_revision(
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

    pub(crate) fn auto_settings_with_revision(
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

    pub(crate) fn update_auto_reviewer_cas(
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

    pub(crate) fn update_auto_rules_cas(
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

    pub(crate) fn update_permission_rules_cas<'a>(
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

    /// 读取整个设置对象；文件不存在或只有空白时为空对象。
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

    /// 在 `settings.json.lock` 的独占锁内读取、修改并原子替换设置文件。
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

    /// 照 Claude Code `writeFileSyncAndFlush_DEPRECATED`：设置文件是符号链接时写入链接目标，
    /// 临时文件与重命名都在目标所在目录进行，链接本身保留。
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

/// 持有期间独占 `path` 上的建议锁（`flock`）。锁随文件关闭释放，进程崩溃时由内核释放，
/// 不会留下需要按时间判断过期的锁；锁文件本身保留，删除它会让等待者锁在已脱离路径的文件上。
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

impl AutoSettingsRevision {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for AutoSettingsRevision {
    fn default() -> Self {
        auto_settings_revision(&Map::new())
    }
}

fn auto_settings_revision(settings: &Map<String, Value>) -> AutoSettingsRevision {
    let digest = Sha256::digest(
        serde_json::to_vec(&settings.get(AUTO_APPROVAL_KEY)).expect("settings value serializes"),
    );
    AutoSettingsRevision(digest.iter().map(|byte| format!("{byte:02x}")).collect())
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

/// 取对象中 `key` 对应的对象；不存在时插入空对象，不是对象时报错（不覆盖用户的内容）。
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

    #[test]
    fn saving_an_old_alias_card_canonicalizes_identity_without_replacing_overrides() {
        let root = tempfile::tempdir().unwrap();
        let settings = UserSettings::in_root(root.path());
        let mut saved = ModelCatalog::defaults()
            .get("openai-api", "gpt-6.1-sol")
            .unwrap();
        saved.model_id = "gpt-6.1-sol-responses".into();
        saved.card.as_mut().unwrap().id = saved.model_id.clone();
        saved.base_url = "https://retained.example/legacy-messages".into();
        saved.protocol = crate::model::Protocol::Messages;
        saved.card.as_mut().unwrap().adapter = crate::model::cards::Adapter::Messages;
        saved.model = "private-service-version".into();
        saved.card.as_mut().unwrap().service_model_id = saved.model.clone();
        saved.max_output_tokens = 2048;
        saved.card.as_mut().unwrap().max_output_tokens = 2048;
        saved.card.as_mut().unwrap().context_window_tokens = Some(8192);
        let original_source = saved.private_history_source();
        settings.save_model(&saved, false).unwrap();
        saved.model_id = "gpt-6.1-sol".into();
        saved.card.as_mut().unwrap().id = saved.model_id.clone();
        let restored = settings.load().configured_model().unwrap().unwrap();
        assert_eq!(restored, saved);
        assert_eq!(restored.private_history_source(), original_source);
        let json: Value =
            serde_json::from_slice(&std::fs::read(root.path().join(SETTINGS_FILE_NAME)).unwrap())
                .unwrap();
        assert_eq!(json[DEFAULT_MODEL_KEY]["model"], "gpt-6.1-sol");
    }

    #[test]
    fn explicit_same_id_model_definition_survives_restart_and_yaml_edits() {
        let root = tempfile::tempdir().unwrap();
        let settings = UserSettings::in_root(root.path());
        let mut explicit = ModelCatalog::defaults()
            .get("deepseek-api", "deepseek-flash-messages")
            .unwrap();
        explicit.base_url = "https://custom.example/messages".into();
        explicit.max_output_tokens = 8192;
        explicit.card.as_mut().unwrap().max_output_tokens = 8192;
        settings.save_model(&explicit, false).unwrap();
        let saved: Value =
            serde_json::from_slice(&std::fs::read(root.path().join(SETTINGS_FILE_NAME)).unwrap())
                .unwrap();
        assert!(saved.get(MODEL_KEY).is_none());
        assert_eq!(saved[DEFAULT_MODEL_KEY]["provider"], "deepseek-api");
        assert_eq!(saved[DEFAULT_MODEL_KEY]["model"], "deepseek-flash-messages");
        let provider_path = root.path().join("providers/deepseek-api.yaml");
        assert!(provider_path.is_file());
        let restarted = UserSettings::in_root(root.path())
            .load()
            .configured_model()
            .unwrap()
            .unwrap();
        assert_eq!(restarted, explicit);

        let mut provider: ProviderPatch =
            serde_yaml_ng::from_slice(&std::fs::read(&provider_path).unwrap()).unwrap();
        provider.models.as_mut().unwrap()[0].max_output_tokens = Some(16384);
        std::fs::write(&provider_path, serde_yaml_ng::to_string(&provider).unwrap()).unwrap();
        let edited = UserSettings::in_root(root.path())
            .load()
            .configured_model()
            .unwrap()
            .unwrap();
        assert_eq!(edited.max_output_tokens, 16384);
        assert_eq!(edited.base_url, explicit.base_url);
    }

    #[test]
    fn selecting_an_unchanged_builtin_model_does_not_create_provider_yaml() {
        let root = tempfile::tempdir().unwrap();
        let settings = UserSettings::in_root(root.path());
        let selected = ModelCatalog::defaults()
            .get("deepseek-api", "deepseek-flash-messages")
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
    fn failed_selection_write_rolls_back_the_new_provider_definition() {
        let root = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("/dev/null", root.path().join(SETTINGS_FILE_NAME)).unwrap();
        let settings = UserSettings::in_root(root.path());
        let mut selected = ModelCatalog::defaults()
            .get("deepseek-api", "deepseek-flash-messages")
            .unwrap();
        selected.base_url = "https://custom.example/messages".into();
        assert!(matches!(
            settings.save_model(&selected, false),
            Err(SettingsError::Write { .. })
        ));
        assert!(!root.path().join("providers/deepseek-api.yaml").exists());
    }

    #[test]
    fn transient_multimodel_provider_is_saved_in_full_when_a_sibling_is_selected() {
        let root = tempfile::tempdir().unwrap();
        let settings = UserSettings::in_root(root.path());
        let yaml = "id: external-provider\ndefault_model: first\ndisplay_name: External provider\nauth: api_key_env\napi_key_env: EXTERNAL_KEY\nendpoints:\n  messages: https://gateway.example/messages\nmodels:\n  - id: first\n    service_model_id: first-service\n    display_name: First\n    adapter: messages\n  - id: second\n    service_model_id: second-service\n    display_name: Second\n    adapter: messages\n";
        let (_, provider) = ModelCatalog::resolved_provider_yaml(yaml).unwrap();
        settings.set_transient_provider(provider);
        let shared = settings.clone();
        let second = shared
            .model_catalog()
            .unwrap()
            .get("external-provider", "second")
            .unwrap();
        assert!(
            !root
                .path()
                .join("providers/external-provider.yaml")
                .exists()
        );
        settings.save_model(&second, false).unwrap();

        let saved: ProviderPatch = serde_yaml_ng::from_slice(
            &std::fs::read(root.path().join("providers/external-provider.yaml")).unwrap(),
        )
        .unwrap();
        assert_eq!(saved.default_model.as_deref(), Some("first"));
        assert_eq!(saved.models.unwrap().len(), 2);
        let selection: Value =
            serde_json::from_slice(&std::fs::read(root.path().join(SETTINGS_FILE_NAME)).unwrap())
                .unwrap();
        assert!(selection.get(MODEL_KEY).is_none());
        assert_eq!(selection[DEFAULT_MODEL_KEY]["model"], "second");
        let restarted = UserSettings::in_root(root.path())
            .load()
            .configured_model()
            .unwrap()
            .unwrap();
        assert_eq!(restarted.model, "second-service");
    }

    #[test]
    fn matching_selected_card_still_saves_new_transient_siblings() {
        let root = tempfile::tempdir().unwrap();
        let settings = UserSettings::in_root(root.path());
        let yaml = "id: external-provider\ndefault_model: first\ndisplay_name: External provider\nauth: api_key_env\napi_key_env: EXTERNAL_KEY\nendpoints:\n  messages: https://gateway.example/messages\nmodels:\n  - id: first\n    service_model_id: first-service\n    display_name: First\n    adapter: messages\n  - id: second\n    service_model_id: second-service\n    display_name: Second\n    adapter: messages\n";
        let (first, complete) = ModelCatalog::resolved_provider_yaml(yaml).unwrap();
        let mut first_only = complete.clone();
        first_only
            .models
            .as_mut()
            .unwrap()
            .retain(|model| model.id == "first");
        settings.save_provider(first_only).unwrap();
        settings.set_transient_provider(complete);
        settings.save_model(&first, false).unwrap();
        let saved: ProviderPatch = serde_yaml_ng::from_slice(
            &std::fs::read(root.path().join("providers/external-provider.yaml")).unwrap(),
        )
        .unwrap();
        assert_eq!(saved.models.unwrap().len(), 2);
    }

    #[test]
    fn partial_explicit_yaml_keeps_saved_endpoint_and_inherited_defaults_live() {
        let root = tempfile::tempdir().unwrap();
        let settings = UserSettings::in_root(root.path());
        let existing: ProviderPatch = serde_yaml_ng::from_str(
            "id: deepseek-api\nendpoints:\n  deepseek_messages: https://gateway.example/anthropic\n",
        )
        .unwrap();
        settings.save_provider(existing).unwrap();
        let partial = "id: deepseek-api\ndefault_model: deepseek-flash-messages\nmodels:\n  - id: deepseek-flash-messages\n    max_output_tokens: 8192\n";
        let (selected, authored) = ModelCatalog::resolved_provider_yaml_with_catalog(
            partial,
            settings.persisted_model_catalog().unwrap(),
        )
        .unwrap();
        assert_eq!(selected.base_url, "https://gateway.example/anthropic");
        settings.set_transient_provider(authored);
        settings.save_model(&selected, false).unwrap();

        let saved: ProviderPatch = serde_yaml_ng::from_slice(
            &std::fs::read(root.path().join("providers/deepseek-api.yaml")).unwrap(),
        )
        .unwrap();
        assert_eq!(saved.display_name, None);
        assert_eq!(saved.auth, None);
        assert_eq!(saved.models.as_ref().unwrap().len(), 1);
        let selected_patch = &saved.models.as_ref().unwrap()[0];
        assert_eq!(selected_patch.max_output_tokens, Some(8192));
        assert_eq!(selected_patch.service_model_id, None);
        assert_eq!(selected_patch.context_window_tokens, None);
        let mut next_defaults = ModelCatalog::defaults();
        let sibling = next_defaults
            .providers
            .iter_mut()
            .find(|provider| provider.id == "deepseek-api")
            .unwrap()
            .models
            .iter_mut()
            .find(|model| model.id == "deepseek-flash-responses")
            .unwrap();
        sibling.max_output_tokens = 12345;
        let next = next_defaults.with_providers(vec![saved]).unwrap();
        assert_eq!(
            next.get("deepseek-api", "deepseek-flash-responses")
                .unwrap()
                .max_output_tokens,
            12345
        );
    }

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

    /// 另一个进程持有 `settings.json.lock` 时写入等待，释放后才读-改-写，不会基于旧内容覆盖。
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

    /// 多个线程交错增删同一分区的规则：每个线程最后留下的规则都在，删除的规则都不复活。
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
}

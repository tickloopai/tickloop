//! User preferences and project permissions, independent of any storage format.
use crate::AutoSettings;
use crate::ProjectRuleChanges;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Mutex,
};
use tickloop_harness_base::output_style::OutputStyle;
use tickloop_harness_model::judgment::JevConfig;
use tickloop_harness_protocol::{
    AutoReviewerPreference, AutoRuleSettings, AutoSettingsRevision, ExecutionMode, ModelConfig,
    PermissionRule, ReasoningEffort, SearchConfig,
};

pub type PermissionChanges = BTreeMap<PathBuf, ProjectRuleChanges>;

#[derive(Clone, Debug, Default)]
pub struct ProjectSettings {
    pub rules: Vec<PermissionRule>,
    pub warnings: Vec<String>,
    pub revision: String,
    pub trusted: bool,
}

#[derive(Clone, Debug)]
pub struct SettingsSnapshot {
    pub additional_directories: tickloop_harness_protocol::AdditionalDirectorySources,
    pub project: ProjectSettings,
    pub model: Option<ModelConfig>,
    pub task_tools_enabled: bool,
    pub execution_mode: Option<ExecutionMode>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub thinking_enabled: bool,
    pub context_window_tokens: Option<usize>,
    /// The `autoCompactWindow` setting: caps the window the automatic compaction line is measured
    /// in.
    pub auto_compact_window: Option<usize>,
    pub search: Option<SearchConfig>,
    pub output_style: OutputStyle,
    pub goal_token_budget: Option<u64>,
    pub goal_check_model: Option<ModelConfig>,
    pub skips_transcripts: bool,
    /// Claude Code's `useAutoModeDuringPlan`: Plan admits and reviews as Auto does. On unless
    /// settings turn it off.
    pub use_auto_mode_during_plan: bool,
    pub auto: Result<(AutoSettings, AutoSettingsRevision), String>,
    pub jev: Result<JevConfig, String>,
    pub warnings: Vec<String>,
    pub warning_prefix: String,
}
impl Default for SettingsSnapshot {
    fn default() -> Self {
        Self {
            project: ProjectSettings::default(),
            additional_directories: BTreeMap::new(),
            model: None,
            task_tools_enabled: false,
            execution_mode: None,
            reasoning_effort: None,
            thinking_enabled: true,
            context_window_tokens: None,
            auto_compact_window: None,
            search: SearchConfig::enabled_default(),
            output_style: OutputStyle::default(),
            goal_token_budget: None,
            goal_check_model: None,
            skips_transcripts: false,
            use_auto_mode_during_plan: true,
            auto: Ok((AutoSettings::default(), empty_auto_settings_revision())),
            jev: Ok(JevConfig::default()),
            warnings: Vec::new(),
            warning_prefix: String::new(),
        }
    }
}

// The existing revision of an absent Auto entry: SHA-256 of JSON null.
const EMPTY_AUTO_SETTINGS_REVISION: &str =
    "74234e98afe7498fb5daf1f36ac2d78acc339464f950703b8c019892f982b90b";

pub fn empty_auto_settings_revision() -> AutoSettingsRevision {
    AutoSettingsRevision::new(EMPTY_AUTO_SETTINGS_REVISION.into())
}

/// Purpose-level changes; the adapter preserves unrelated preferences atomically.
pub enum SettingsUpdate {
    Model {
        model: Box<ModelConfig>,
        clear_context_window: bool,
        thinking: Option<Option<ReasoningEffort>>,
    },
    ReasoningEffort(Option<ReasoningEffort>),
    Thinking {
        enabled: bool,
        effort: Option<ReasoningEffort>,
    },
    TaskTools(bool),
    ExecutionMode(ExecutionMode),
    ContextWindow(usize),
    Search(Option<SearchConfig>),
    TrustDirectory(PathBuf),
    AutoReviewer {
        expected: AutoSettingsRevision,
        reviewer: AutoReviewerPreference,
    },
    AutoRules {
        expected: AutoSettingsRevision,
        rules: AutoRuleSettings,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum SettingsStoreError {
    #[error("settings changed while editing")]
    Conflict,
    #[error("{0}")]
    Storage(String),
}

/// Reads produce a coherent snapshot. Permission writes merge every named partition atomically;
/// when supplied, the expected revision is checked under the same transaction before any write.
/// Revisions are opaque and change only when the corresponding stored content changes.
pub trait SettingsStore: Send + Sync {
    fn read(&self, project: Option<&Path>) -> SettingsSnapshot;
    fn write(&self, change: SettingsUpdate) -> Result<(), SettingsStoreError>;
    fn skills(&self, folder: Option<&Path>) -> crate::skills::SkillRegistry {
        if folder.is_some() {
            crate::skills::SkillRegistry::builtin()
        } else {
            crate::skills::SkillRegistry::builtin_without_ticks()
        }
    }
    fn loop_instructions(
        &self,
        _folder: Option<&Path>,
    ) -> Result<crate::loop_skill::LoopInstructions, crate::loop_skill::LoopFileError> {
        crate::loop_skill::LoopInstructionSource::instructions(
            &crate::loop_skill::MaintenanceInstructions,
        )
    }
    /// The instruction files a conversation in `folder` loads, lowest priority first.
    fn instruction_files(
        &self,
        _folder: Option<&Path>,
    ) -> Vec<crate::instructions::InstructionFile> {
        Vec::new()
    }

    fn write_permissions(
        &self,
        changes: &PermissionChanges,
        expected: Option<(&Path, &str)>,
    ) -> Result<(), SettingsStoreError>;
}

#[derive(Default)]
pub struct MemorySettingsStore {
    state: Mutex<MemorySettings>,
}

#[derive(Default)]
struct MemorySettings {
    snapshot: SettingsSnapshot,
    projects: BTreeMap<PathBuf, ProjectSettings>,
    auto_revision: u64,
    has_thinking_selection: bool,
    previous_effort: Option<ReasoningEffort>,
}

impl MemorySettingsStore {
    pub fn new(snapshot: SettingsSnapshot) -> Self {
        Self {
            state: Mutex::new(MemorySettings {
                auto_revision: snapshot
                    .auto
                    .as_ref()
                    .ok()
                    .and_then(|(_, revision)| revision.as_str().parse().ok())
                    .unwrap_or(0),
                has_thinking_selection: true,
                previous_effort: snapshot.reasoning_effort,
                snapshot,
                ..Default::default()
            }),
        }
    }
}

impl SettingsStore for MemorySettingsStore {
    fn read(&self, project: Option<&Path>) -> SettingsSnapshot {
        let state = self.state.lock().unwrap();
        let mut snapshot = state.snapshot.clone();
        snapshot.project = project
            .and_then(|project| state.projects.get(project).cloned())
            .unwrap_or_default();
        snapshot
    }

    fn write(&self, change: SettingsUpdate) -> Result<(), SettingsStoreError> {
        let mut state = self.state.lock().unwrap();
        match change {
            SettingsUpdate::TaskTools(enabled) => state.snapshot.task_tools_enabled = enabled,
            SettingsUpdate::ExecutionMode(mode) => state.snapshot.execution_mode = Some(mode),
            SettingsUpdate::TrustDirectory(project) => {
                state.projects.entry(project).or_default().trusted = true
            }
            SettingsUpdate::Search(config) => state.snapshot.search = config,
            SettingsUpdate::ContextWindow(_) => {
                return Err(SettingsStoreError::Storage(
                    "context window is supplied by the built-in program catalog".into(),
                ));
            }
            SettingsUpdate::ReasoningEffort(effort) => {
                if effort == Some(ReasoningEffort::On) {
                    state.snapshot.thinking_enabled = true;
                    state.has_thinking_selection = true;
                } else if !state.has_thinking_selection {
                    if state.previous_effort == Some(ReasoningEffort::Off) {
                        state.snapshot.thinking_enabled = false;
                        state.has_thinking_selection = true;
                    } else {
                        state.snapshot.thinking_enabled = effort != Some(ReasoningEffort::Off);
                    }
                }
                state.previous_effort = effort;
                state.snapshot.reasoning_effort =
                    effort.filter(|effort| !matches!(effort, ReasoningEffort::Off));
            }
            SettingsUpdate::Thinking { enabled, effort } => {
                state.snapshot.thinking_enabled = enabled;
                state.has_thinking_selection = true;
                if let Some(effort) = effort {
                    state.previous_effort = Some(effort);
                    state.snapshot.reasoning_effort =
                        (effort != ReasoningEffort::Off).then_some(effort);
                } else if state.previous_effort == Some(ReasoningEffort::Off) {
                    state.previous_effort = None;
                    state.snapshot.reasoning_effort = None;
                }
            }
            SettingsUpdate::Model {
                model,
                clear_context_window,
                thinking,
            } => {
                if clear_context_window {
                    state.snapshot.context_window_tokens = None;
                }
                if let Some(effort) = thinking {
                    state.has_thinking_selection = true;
                    state.previous_effort = effort;
                    state.snapshot.thinking_enabled = effort != Some(ReasoningEffort::Off);
                    state.snapshot.reasoning_effort =
                        effort.filter(|effort| *effort != ReasoningEffort::Off);
                }
                state.snapshot.model = Some(*model);
            }
            SettingsUpdate::AutoReviewer { ref expected, .. }
            | SettingsUpdate::AutoRules { ref expected, .. } => {
                let (mut auto, revision) = state
                    .snapshot
                    .auto
                    .clone()
                    .map_err(SettingsStoreError::Storage)?;
                if &revision != expected {
                    return Err(SettingsStoreError::Conflict);
                }
                let previous = auto.clone();
                match change {
                    SettingsUpdate::AutoReviewer { reviewer, .. } => auto.reviewer = reviewer,
                    SettingsUpdate::AutoRules { rules, .. } => {
                        rules.validate().map_err(SettingsStoreError::Storage)?;
                        auto.rules = rules;
                    }
                    _ => unreachable!(),
                }
                if auto != previous {
                    state.auto_revision += 1;
                    state.snapshot.auto = Ok((
                        auto,
                        AutoSettingsRevision::new(state.auto_revision.to_string()),
                    ));
                }
            }
        }
        Ok(())
    }

    fn write_permissions(
        &self,
        changes: &PermissionChanges,
        expected: Option<(&Path, &str)>,
    ) -> Result<(), SettingsStoreError> {
        let mut state = self.state.lock().unwrap();
        let projects = &mut state.projects;
        if let Some((project, revision)) = expected
            && projects
                .get(project)
                .map_or("", |settings| settings.revision.as_str())
                != revision
        {
            return Err(SettingsStoreError::Conflict);
        }
        for (project, change) in changes {
            let is_new = !projects.contains_key(project);
            let settings = projects.entry(project.clone()).or_default();
            let previous =
                serde_json::to_value(&settings.rules).expect("permission rules serialize");
            settings.rules.retain(|rule| {
                !change.removed.contains(&rule.id)
                    && !change.added.iter().any(|added| added.id == rule.id)
            });
            settings.rules.extend(change.added.clone());
            if is_new
                || serde_json::to_value(&settings.rules).expect("permission rules serialize")
                    != previous
            {
                let next = settings.revision.parse::<u64>().unwrap_or(0) + 1;
                settings.revision = next.to_string();
            }
        }
        Ok(())
    }
}

impl SettingsSnapshot {
    pub fn warn(&mut self, problem: String) {
        let warning = format!("{}{problem}", self.warning_prefix);
        if !self.warnings.contains(&warning) {
            self.warnings.push(warning);
        }
    }
}

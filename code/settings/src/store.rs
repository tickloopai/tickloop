use crate::{
    SettingsError, UserSettings,
    skills::{SkillDiscovery, SkillRegistry},
};
use std::path::Path;
use tickloop_harness::policy::settings::{
    PermissionChanges, ProjectSettings, SettingsSnapshot, SettingsStore, SettingsStoreError,
    SettingsUpdate,
};
use tickloop_harness::protocol::hooks::{HookList, HookTrustChange};

impl From<SettingsError> for SettingsStoreError {
    fn from(error: SettingsError) -> Self {
        match error {
            SettingsError::Conflict => Self::Conflict,
            error => Self::Storage(error.to_string()),
        }
    }
}

impl SettingsStore for UserSettings {
    fn read(&self, project: Option<&Path>) -> SettingsSnapshot {
        let mut saved = self.load();
        let project_settings = project
            .map(|project| {
                let mut project_saved = saved.clone();
                let mut problems = Vec::new();
                let rules = crate::saved_project_rules(&saved.settings, project, &mut problems);
                let warnings = if let Some(error) = &saved.read_error {
                    vec![error.clone()]
                } else {
                    problems
                        .into_iter()
                        .map(|problem| format!("settings file {}: {problem}", saved.path.display()))
                        .collect()
                };
                let revision = if saved.read_error.is_some() {
                    String::new()
                } else {
                    crate::project_rules_revision(&saved.settings, project)
                };
                ProjectSettings {
                    rules,
                    warnings,
                    revision,
                    trusted: project_saved.is_working_directory_trusted(project),
                }
            })
            .unwrap_or_default();
        let auto = match &saved.read_error {
            Some(error) => Err(error.clone()),
            None => crate::parse_auto_settings(&saved.settings)
                .map(|auto| (auto, crate::auto_settings_revision(&saved.settings)))
                .map_err(|entry| {
                    SettingsError::InvalidEntry {
                        path: saved.path.clone(),
                        key: entry.key,
                        expected: entry.expected,
                    }
                    .to_string()
                }),
        };
        let has_read_error = saved.read_error.is_some();
        let mut snapshot = SettingsSnapshot {
            project: project_settings,
            additional_directories: project
                .map(|folder| saved.additional_directories(folder))
                .unwrap_or_default(),
            model: saved.clone().model(),
            task_tools_enabled: saved.task_tools_enabled(),
            execution_mode: saved.default_execution_mode(),
            reasoning_effort: saved.reasoning_effort(),
            thinking_enabled: saved.thinking_enabled(),
            context_window_tokens: saved.context_window_tokens(),
            auto_compact_window: saved.auto_compact_window(),
            search: saved.search_config(),
            output_style: saved.output_style(),
            goal_token_budget: saved.clone().goal_token_budget(),
            goal_check_model: saved.clone().goal_check_model(),
            skips_transcripts: saved.cleanup_period_days() == Some(0),
            use_auto_mode_during_plan: saved.use_auto_mode_during_plan(),
            auto,
            jev: saved.jev_config(),
            warning_prefix: format!("settings file {}: ", saved.path.display()),
            warnings: saved.into_warnings(),
        };
        if !has_read_error {
            snapshot.warnings.extend(snapshot.project.warnings.clone());
        }
        snapshot
    }

    fn write(&self, change: SettingsUpdate) -> Result<(), SettingsStoreError> {
        match change {
            SettingsUpdate::Model {
                model,
                clear_context_window,
                thinking,
            } => match thinking {
                Some(effort) => self.save_model_with_thinking(&model, clear_context_window, effort),
                None => self.save_model(&model, clear_context_window),
            },
            SettingsUpdate::ReasoningEffort(effort) => self.save_reasoning_effort(effort),
            SettingsUpdate::Thinking { enabled, effort } => {
                self.save_thinking_enabled_with_effort(enabled, effort)
            }
            SettingsUpdate::TaskTools(enabled) => self.save_task_tools_enabled(enabled),
            SettingsUpdate::ExecutionMode(mode) => self.save_default_execution_mode(mode),
            SettingsUpdate::ContextWindow(tokens) => self.save_context_window(tokens),
            SettingsUpdate::Search(config) => self.save_search_config(config.as_ref()),
            SettingsUpdate::TrustDirectory(project) => self.trust_working_directory(&project),
            SettingsUpdate::AutoReviewer { expected, reviewer } => self
                .update_auto_reviewer_cas(&expected, reviewer)
                .map(|_| ()),
            SettingsUpdate::AutoRules { expected, rules } => {
                self.update_auto_rules_cas(&expected, &rules).map(|_| ())
            }
        }
        .map_err(Into::into)
    }

    fn write_permissions(
        &self,
        changes: &PermissionChanges,
        expected: Option<(&Path, &str)>,
    ) -> Result<(), SettingsStoreError> {
        let changes = changes
            .iter()
            .map(|(project, change)| (project.as_path(), change));
        match expected {
            Some((project, revision)) => {
                self.update_permission_rules_cas(project, revision, changes)
            }
            None => self.update_permission_rules(changes),
        }
        .map_err(Into::into)
    }

    fn skills(&self, folder: Option<&Path>) -> SkillRegistry {
        match folder {
            Some(folder) => SkillRegistry::discover(folder, self.root()),
            None => {
                let mut registry = SkillRegistry::builtin_without_ticks();
                registry.set_loop_instructions(std::sync::Arc::new(
                    crate::loop_skill::LoopFiles::new(None, self.root()),
                ));
                registry
            }
        }
    }

    fn loop_instructions(
        &self,
        folder: Option<&Path>,
    ) -> Result<
        tickloop_harness::policy::loop_skill::LoopInstructions,
        tickloop_harness::policy::loop_skill::LoopFileError,
    > {
        crate::loop_skill::LoopFiles::new(folder, self.root()).instructions()
    }

    fn instruction_files(
        &self,
        folder: Option<&Path>,
    ) -> Vec<tickloop_harness::policy::instructions::InstructionFile> {
        crate::instructions::instruction_files(folder, self.root(), self.home_directory())
    }

    fn hooks(&self, folder: Option<&Path>) -> HookList {
        crate::hooks::configured_hooks(self, folder)
    }

    fn write_hook_trust(
        &self,
        folder: Option<&Path>,
        changes: &[HookTrustChange],
        expected_revision: Option<&str>,
    ) -> Result<(), SettingsStoreError> {
        crate::hooks::write_trust(self, folder, changes, expected_revision)
    }
}

//! Configuration chosen on the New Agent screen before its first submit.
//!
//! The draft never reaches the Agent daemon. The first submit creates the Agent from `launch`, then
//! `ConsoleExecution` previews choices and saves user defaults locally, then replays staged
//! choices through the same Configure commands a visible Agent uses. The daemon validates them
//! again against the created Agent. Only fields and commands
//! that protocol 1.1 already carries are used: a daemon that does not understand a staged change
//! cannot exist, so none can be silently dropped.

use std::path::PathBuf;

use uuid::Uuid;

use tickloop_agent_model::{validate_model_config, validate_reasoning_effort};

use super::completion::CHILD_AUTHORIZATION_FIXED;
use tickloop_agent_protocol::{
    AccessError, ChatSnapshot, ConfigurationChange, ExecutionMode, LaunchSpec, ModelConfig,
    PermissionChange, PermissionRule, ReasoningEffort,
};

/// Which configuration a settings menu or command shows and changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ConfigTarget {
    /// The visible Agent, root or child; its daemon validates scope and ceilings.
    Agent(Uuid),
    /// The New Agent screen, before its first submit creates the Agent.
    NewDraft,
}

impl ConfigTarget {
    pub(super) fn visible(selected_chat: Option<Uuid>) -> Self {
        selected_chat.map_or(Self::NewDraft, Self::Agent)
    }

    pub(super) fn agent(self) -> Option<Uuid> {
        match self {
            Self::Agent(chat) => Some(chat),
            Self::NewDraft => None,
        }
    }
}

/// The configuration settings menus read, for a visible Agent or the New Agent draft.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct AgentConfiguration {
    pub(super) model: ModelConfig,
    pub(super) pending_model: Option<ModelConfig>,
    pub(super) reasoning_effort: Option<ReasoningEffort>,
    pub(super) thinking_enabled: bool,
    pub(super) execution_mode: ExecutionMode,
    pub(super) additional_directories: Vec<PathBuf>,
    /// The daemon's delegation fact for this Agent, never inferred from the visible tree.
    pub(super) delegated: bool,
}

impl AgentConfiguration {
    pub(super) fn of(snapshot: &ChatSnapshot) -> Self {
        Self {
            model: snapshot.model.clone(),
            pending_model: snapshot.pending_model.clone(),
            reasoning_effort: snapshot.reasoning_effort,
            thinking_enabled: snapshot.thinking_enabled,
            execution_mode: snapshot.policy.mode,
            additional_directories: snapshot.additional_directories.clone(),
            delegated: snapshot.delegation.is_some(),
        }
    }

    /// Why `change` cannot apply to this Agent, known before anything is sent. The daemon still
    /// validates every change it receives; model, effort, Thinking, context window and search stay
    /// available to a child.
    pub(super) fn unavailable_change(&self, change: &ConfigurationChange) -> Option<&'static str> {
        let root_only = matches!(
            change,
            ConfigurationChange::ExecutionMode { .. }
                | ConfigurationChange::Directories { .. }
                | ConfigurationChange::PermissionChange { .. }
                | ConfigurationChange::GenerateAutoEnvironment { .. }
                | ConfigurationChange::ReplaceRules { .. }
                | ConfigurationChange::ProjectConstraints { .. }
                | ConfigurationChange::ManagedConstraints { .. }
        );
        (self.delegated && root_only).then_some(CHILD_AUTHORIZATION_FIXED)
    }
}

/// What the first input needs: the Create launch, then changes in replay order.
pub(super) struct FirstCreate {
    pub(super) launch: LaunchSpec,
    pub(super) execution_mode: Option<ExecutionMode>,
    pub(super) additional_directories: Vec<PathBuf>,
    pub(super) rule_changes: Vec<PermissionChange>,
    /// Model, Thinking, effort and search changes, in the order they were made.
    pub(super) changes: Vec<ConfigurationChange>,
}

/// One staged choice the first input replays on its created Agent.
#[allow(clippy::large_enum_variant)]
pub(super) enum ReplayStep {
    ExecutionMode(ExecutionMode),
    Directories(Vec<PathBuf>),
    Rule(PermissionChange),
    Change(ConfigurationChange),
}

impl FirstCreate {
    /// The next choice to replay: execution mode first, so the first input never runs under a
    /// wider mode than the one chosen, then directories, rules and the remaining changes in the
    /// order they were made.
    pub(super) fn next_step(&self) -> Option<ReplayStep> {
        if let Some(mode) = self.execution_mode {
            return Some(ReplayStep::ExecutionMode(mode));
        }
        if !self.additional_directories.is_empty() {
            return Some(ReplayStep::Directories(self.additional_directories.clone()));
        }
        if let Some(change) = self.rule_changes.first() {
            return Some(ReplayStep::Rule(change.clone()));
        }
        self.changes.first().cloned().map(ReplayStep::Change)
    }

    /// Drops the step `next_step` returned once the daemon proves it applied.
    pub(super) fn complete_step(&mut self) {
        if self.execution_mode.take().is_some() {
            return;
        }
        if !self.additional_directories.is_empty() {
            self.additional_directories.clear();
            return;
        }
        if !self.rule_changes.is_empty() {
            self.rule_changes.remove(0);
            return;
        }
        if !self.changes.is_empty() {
            self.changes.remove(0);
        }
    }
}

#[derive(Clone, Default)]
pub(super) struct NewAgentDraft {
    changes: Vec<ConfigurationChange>,
    additional_directories: Option<Vec<PathBuf>>,
    rules: Vec<PermissionRule>,
}

impl NewAgentDraft {
    /// `base` is what a new Agent gets from the launch and saved settings; staged choices
    /// override it the way the runtime setters would.
    pub(super) fn configuration(&self, mut base: AgentConfiguration) -> AgentConfiguration {
        for change in &self.changes {
            apply_preview(&mut base, change);
        }
        if let Some(directories) = &self.additional_directories {
            base.additional_directories = directories.clone();
        }
        base
    }

    /// Records `change` for the first Create after checking it the way the runtime will.
    /// `current` is the configuration the draft shows now. A rejected change leaves the draft
    /// unchanged.
    pub(super) fn stage(
        &mut self,
        current: &AgentConfiguration,
        change: ConfigurationChange,
    ) -> Result<(), AccessError> {
        let invalid = |error: &dyn std::fmt::Display| AccessError::invalid(error.to_string());
        match &change {
            ConfigurationChange::Model { config, thinking } => {
                validate_model_config(config).map_err(|error| invalid(&error))?;
                if let Some(effort) = thinking {
                    validate_reasoning_effort(config, *effort).map_err(|error| invalid(&error))?;
                }
            }
            ConfigurationChange::ReasoningEffort { effort } => {
                validate_reasoning_effort(&current.model, *effort)
                    .map_err(|error| invalid(&error))?;
            }
            ConfigurationChange::ThinkingEnabled { enabled } => {
                validate_reasoning_effort(
                    &current.model,
                    (!enabled).then_some(ReasoningEffort::Off),
                )
                .map_err(|error| invalid(&error))?;
            }
            ConfigurationChange::Directories { additional, .. } => {
                self.additional_directories = Some(additional.clone());
                return Ok(());
            }
            ConfigurationChange::ExecutionMode { .. } | ConfigurationChange::Search { .. } => {}
            _ => {
                return Err(AccessError::invalid(
                    "this setting applies after the first input creates the Agent",
                ));
            }
        }
        // Model, effort and Thinking choices depend on each other, so every validated choice is
        // kept in the order it was made and replayed the same way.
        self.changes.push(change);
        Ok(())
    }

    /// Stages a session or persistent rule; removing works on rules staged in this draft.
    pub(super) fn stage_rule_change(
        &mut self,
        change: PermissionChange,
    ) -> Result<(), AccessError> {
        match change {
            PermissionChange::UpsertRule { rule } => {
                self.rules.retain(|staged| staged.id != rule.id);
                self.rules.push(rule);
                Ok(())
            }
            PermissionChange::RemoveRule { id } => {
                let before = self.rules.len();
                self.rules.retain(|staged| staged.id != id);
                if self.rules.len() == before {
                    return Err(AccessError::invalid(
                        "rule does not exist in this draft; saved rules can be removed after the first input",
                    ));
                }
                Ok(())
            }
            _ => Err(AccessError::invalid(
                "this permission setting applies after the first input creates the Agent",
            )),
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.changes.is_empty() && self.additional_directories.is_none() && self.rules.is_empty()
    }

    pub(super) fn staged_rules(&self) -> &[PermissionRule] {
        &self.rules
    }

    /// Splits the draft into what the first Create sends, starting from the View's launch.
    /// A staged model is replayed rather than launched, so the runtime clears the previous
    /// model's saved context window exactly as it does for a visible Agent.
    pub(super) fn into_first_create(self, mut launch: LaunchSpec, workdir: PathBuf) -> FirstCreate {
        launch.workdir = workdir;
        launch.exact_chat = None;
        let mut execution_mode = None;
        let mut changes = Vec::new();
        for change in self.changes {
            match change {
                ConfigurationChange::ExecutionMode { mode } => execution_mode = Some(mode),
                change => changes.push(change),
            }
        }
        FirstCreate {
            launch,
            execution_mode,
            additional_directories: self.additional_directories.unwrap_or_default(),
            rule_changes: self
                .rules
                .into_iter()
                .map(|rule| PermissionChange::UpsertRule { rule })
                .collect(),
            changes,
        }
    }
}

/// Mirrors the runtime setters' effect on the fields menus show; the created Agent's own values
/// remain authoritative.
fn apply_preview(configuration: &mut AgentConfiguration, change: &ConfigurationChange) {
    match change {
        ConfigurationChange::Model { config, thinking } => {
            configuration.model =
                tickloop_agent_model::cards::model_for_persistent_selection(config.clone());
            match thinking {
                Some(effort) => {
                    configuration.reasoning_effort =
                        effort.filter(|effort| *effort != ReasoningEffort::Off);
                    configuration.thinking_enabled = *effort != Some(ReasoningEffort::Off);
                }
                None => {
                    if configuration.reasoning_effort.is_some_and(|effort| {
                        validate_reasoning_effort(&configuration.model, Some(effort)).is_err()
                    }) {
                        configuration.reasoning_effort = None;
                    }
                }
            }
        }
        ConfigurationChange::ReasoningEffort { effort } => {
            configuration.reasoning_effort = *effort;
            if *effort == Some(ReasoningEffort::On) {
                configuration.thinking_enabled = true;
            }
        }
        ConfigurationChange::ThinkingEnabled { enabled } => {
            configuration.thinking_enabled = *enabled;
            if *enabled
                && validate_reasoning_effort(&configuration.model, Some(ReasoningEffort::On))
                    .is_ok()
            {
                configuration.reasoning_effort = Some(ReasoningEffort::On);
            }
        }
        ConfigurationChange::ExecutionMode { mode } => configuration.execution_mode = *mode,
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tickloop_agent_model::cards::ModelCatalog;

    fn model(provider: &str, id: &str) -> ModelConfig {
        ModelCatalog::defaults()
            .get(provider, id)
            .expect("bundled model")
    }

    fn base() -> AgentConfiguration {
        AgentConfiguration {
            model: model("deepseek-api", "deepseek-flash-chat-completions"),
            pending_model: None,
            reasoning_effort: None,
            thinking_enabled: true,
            execution_mode: ExecutionMode::Auto,
            additional_directories: Vec::new(),
            delegated: false,
        }
    }

    #[test]
    fn the_last_execution_mode_choice_wins_and_other_choices_keep_their_order() {
        let mut draft = NewAgentDraft::default();
        let current = base();
        draft
            .stage(
                &current,
                ConfigurationChange::ExecutionMode {
                    mode: ExecutionMode::Plan,
                },
            )
            .unwrap();
        draft
            .stage(
                &current,
                ConfigurationChange::ThinkingEnabled { enabled: false },
            )
            .unwrap();
        draft
            .stage(
                &current,
                ConfigurationChange::ExecutionMode {
                    mode: ExecutionMode::Auto,
                },
            )
            .unwrap();
        let first = draft.into_first_create(
            LaunchSpec::embedded(PathBuf::from("/")),
            PathBuf::from("/work"),
        );
        assert_eq!(first.execution_mode, Some(ExecutionMode::Auto));
        assert!(matches!(
            first.changes.as_slice(),
            [ConfigurationChange::ThinkingEnabled { enabled: false }]
        ));
        assert_eq!(first.launch.workdir, PathBuf::from("/work"));
    }

    #[test]
    fn a_setting_that_needs_a_created_agent_is_refused_without_changing_the_draft() {
        let mut draft = NewAgentDraft::default();
        assert!(
            draft
                .stage(&base(), ConfigurationChange::ContextWindow { tokens: 1 })
                .is_err()
        );
        assert_eq!(draft.configuration(base()), base());
    }

    #[test]
    fn removing_an_unstaged_rule_is_refused() {
        let mut draft = NewAgentDraft::default();
        assert!(
            draft
                .stage_rule_change(PermissionChange::RemoveRule { id: Uuid::new_v4() })
                .is_err()
        );
        assert!(draft.staged_rules().is_empty());
    }
}

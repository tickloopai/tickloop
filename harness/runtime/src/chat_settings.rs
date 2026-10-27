//! Connects process-local Chat defaults with user settings.
//!
//! Model selection, persistent permission rules, and context limits live in user settings.
//! Execution mode, session rules, additional directories, and pending model changes remain
//! process-local. Persistent rules are partitioned by the Chat working directory.
//!
//! Settings writes usually release the `chats` lock, then synchronize loaded Chats after the
//! write. Model selection holds its Chat lock through the write so a completing request cannot
//! invalidate a successful selection. `settings_writes` serializes both paths.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
};

use uuid::Uuid;

use super::{Agent, AgentError, Chat, ChatBookkeeping};
use tickloop_harness_base::output_style::OutputStyle;
use tickloop_harness_model::{ModelClient, validate_reasoning_effort};
use tickloop_harness_policy::ProjectRuleChanges;
use tickloop_harness_policy::new_chat::new_chat_settings;
use tickloop_harness_policy::settings::{
    SettingsSnapshot, SettingsStore, SettingsStoreError, SettingsUpdate,
};
use tickloop_harness_protocol::{
    ChatId, ChatSnapshot, Decision, ModelConfig, PermissionRule, ReasoningEffort,
};

/// Persistent rule changes partitioned by canonical project working directory.
pub(super) type RuleChanges = BTreeMap<PathBuf, ProjectRuleChanges>;

pub(super) struct SelectedModel {
    pub(super) config: ModelConfig,
    pub(super) client: Arc<dyn ModelClient>,
}

impl Agent {
    /// Problems found the last time user settings were read for this Chat (create, resume, Branch,
    /// working directory change), such as persistent rules not taking effect because the settings
    /// file is not valid JSON.
    pub fn user_settings_warnings(&self, chat_id: ChatId) -> Vec<String> {
        self.inner
            .settings_warnings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&chat_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Applies the current model and project settings to a new or resumed Chat.
    /// Returns its model client and Output Style.
    pub(super) fn apply_chat_defaults(
        &self,
        snapshot: &mut ChatSnapshot,
        bookkeeping: &mut ChatBookkeeping,
    ) -> (Arc<dyn ModelClient>, OutputStyle) {
        let client = {
            let selected = self
                .inner
                .selected_model
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            snapshot.model = selected.config.clone();
            selected.client.clone()
        };
        let output_style = self.apply_new_chat_settings(snapshot, bookkeeping);
        (client, output_style)
    }

    /// Fills in the host's rules as they are now: a policy never records them, so every policy
    /// built or loaded gets them here.
    pub(super) fn apply_host_rules(
        &self,
        policy: &mut tickloop_harness_protocol::PermissionPolicy,
    ) {
        policy.host_rules = self.inner.host_rules.clone();
    }

    pub(super) fn apply_new_chat_settings(
        &self,
        snapshot: &mut ChatSnapshot,
        bookkeeping: &mut ChatBookkeeping,
    ) -> OutputStyle {
        let mut saved = self
            .inner
            .settings
            .as_ref()
            .map(|settings| settings.read(snapshot.folder.as_deref()));
        match &self.inner.search {
            super::SearchDefaults::Fixed(config) => snapshot.search_config = config.clone(),
            super::SearchDefaults::UserSettings => {
                if let Some(saved) = &mut saved {
                    snapshot.search_config = saved.search.clone();
                } else if snapshot.delegation.is_none() {
                    snapshot.search_config =
                        tickloop_harness_protocol::SearchConfig::enabled_default();
                }
            }
        }
        let defaults = new_chat_settings(saved.as_mut(), &self.inner.host_options, &snapshot.model);
        snapshot.policy.mode = defaults.execution_mode;
        self.apply_host_rules(&mut snapshot.policy);
        snapshot.task_tools_enabled = saved.as_ref().map_or_else(
            || {
                self.inner
                    .task_tools_enabled
                    .load(std::sync::atomic::Ordering::SeqCst)
            },
            |_| defaults.task_tools_enabled,
        );
        let Some(saved) = saved else {
            return OutputStyle::default();
        };
        snapshot.context_window_tokens = defaults.context_window_tokens;
        snapshot.reasoning_effort = defaults.reasoning_effort;
        snapshot.thinking_enabled = defaults.thinking_enabled;
        for (directory, sources) in &saved.additional_directories {
            if !snapshot.additional_directories.contains(directory) {
                snapshot.additional_directories.push(directory.clone());
            }
            bookkeeping
                .additional_directory_sources
                .entry(directory.clone())
                .or_default()
                .extend(sources);
        }
        let output_style = saved.output_style;
        self.replace_persistent_rules(snapshot, bookkeeping, saved);
        output_style
    }

    /// Reads user settings on a blocking thread; returns `None` for Agents that do not read or
    /// write settings.
    pub(super) async fn load_user_settings(
        &self,
        project: Option<PathBuf>,
    ) -> Result<Option<SettingsSnapshot>, AgentError> {
        let Some(settings) = self.inner.settings.clone() else {
            return Ok(None);
        };
        tokio::task::spawn_blocking(move || settings.read(project.as_deref()))
            .await
            .map(Some)
            .map_err(|error| AgentError::Storage(error.to_string()))
    }

    /// Replaces the Chat's persistent rules with the rules of its working directory's partition in
    /// settings, keeping session rules, and records read warnings.
    pub(super) fn replace_persistent_rules(
        &self,
        snapshot: &mut ChatSnapshot,
        bookkeeping: &mut ChatBookkeeping,
        saved: SettingsSnapshot,
    ) {
        // A Chat without a Folder belongs to no project, which persistent rules partition by.
        let Some(folder) = snapshot.folder.clone() else {
            return;
        };
        if snapshot.delegation.is_some() {
            return;
        }
        let rules = saved.project.rules;
        snapshot.policy.rules.retain(|rule| !rule.persistent);
        bookkeeping.persistent_rule_projects =
            rules.iter().map(|rule| (rule.id, folder.clone())).collect();
        snapshot.policy.rules.extend(rules);
        self.inner
            .settings_warnings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(snapshot.id, saved.warnings);
    }

    /// Chats created or resumed afterwards use this model.
    pub(super) fn select_model(&self, config: ModelConfig, client: Arc<dyn ModelClient>) {
        *self
            .inner
            .selected_model
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = SelectedModel { config, client };
    }

    pub(super) async fn save_model_setting(
        &self,
        config: ModelConfig,
        clear_context_window: bool,
    ) -> Result<(), AgentError> {
        self.write_user_settings(move |settings| {
            settings.write(SettingsUpdate::Model {
                model: Box::new(config),
                clear_context_window,
                thinking: None,
            })
        })
        .await
    }

    pub(super) async fn save_model_and_thinking_setting(
        &self,
        config: ModelConfig,
        clear_context_window: bool,
        effort: Option<ReasoningEffort>,
    ) -> Result<(), AgentError> {
        self.write_user_settings(move |settings| {
            settings.write(SettingsUpdate::Model {
                model: Box::new(config),
                clear_context_window,
                thinking: Some(effort),
            })
        })
        .await
    }

    pub async fn set_reasoning_effort(
        &self,
        chat_id: ChatId,
        effort: Option<ReasoningEffort>,
    ) -> Result<(), AgentError> {
        let _settings_gate = self.inner.settings_writes.lock().await;
        {
            let chats = self.inner.chats.lock().await;
            let chat = chats.get(&chat_id).ok_or(AgentError::NotFound)?;
            validate_reasoning_effort(&chat.snapshot.model, effort)
                .map_err(|error| AgentError::Invalid(error.to_string()))?;
        }
        self.write_user_settings(move |settings| {
            settings.write(SettingsUpdate::ReasoningEffort(effort))
        })
        .await?;
        let mut chats = self.inner.chats.lock().await;
        let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
        validate_reasoning_effort(&chat.snapshot.model, effort)
            .map_err(|error| AgentError::Invalid(error.to_string()))?;
        chat.snapshot.reasoning_effort = effort;
        if effort == Some(ReasoningEffort::On) {
            chat.snapshot.thinking_enabled = true;
        }
        Ok(())
    }

    pub async fn set_task_tools_enabled(
        &self,
        chat_id: ChatId,
        enabled: bool,
    ) -> Result<(), AgentError> {
        let _settings_gate = self.inner.settings_writes.lock().await;
        if !self.inner.chats.lock().await.contains_key(&chat_id) {
            return Err(AgentError::NotFound);
        }
        self.write_user_settings(move |settings| {
            settings.write(SettingsUpdate::TaskTools(enabled))
        })
        .await?;
        self.apply_task_tools_setting(enabled).await;
        Ok(())
    }

    pub async fn task_tools_setting(&self, chat_id: ChatId) -> Result<bool, AgentError> {
        if !self.inner.chats.lock().await.contains_key(&chat_id) {
            return Err(AgentError::NotFound);
        }
        self.refresh_task_tools_setting().await?;
        Ok(self
            .inner
            .task_tools_enabled
            .load(std::sync::atomic::Ordering::SeqCst))
    }

    pub(super) async fn refresh_task_tools_setting(&self) -> Result<(), AgentError> {
        let _settings_gate = self.inner.settings_writes.lock().await;
        if let Some(settings) = self.load_user_settings(None).await? {
            self.apply_task_tools_setting(settings.task_tools_enabled)
                .await;
        }
        Ok(())
    }

    async fn apply_task_tools_setting(&self, enabled: bool) {
        self.inner
            .task_tools_enabled
            .store(enabled, std::sync::atomic::Ordering::SeqCst);
        let mut chats = self.inner.chats.lock().await;
        for chat in chats.values_mut() {
            chat.snapshot.task_tools_enabled = enabled;
        }
    }

    pub async fn set_thinking_enabled(
        &self,
        chat_id: ChatId,
        enabled: bool,
    ) -> Result<(), AgentError> {
        let _settings_gate = self.inner.settings_writes.lock().await;
        let enable_effort = {
            let chats = self.inner.chats.lock().await;
            let chat = chats.get(&chat_id).ok_or(AgentError::NotFound)?;
            validate_reasoning_effort(
                &chat.snapshot.model,
                (!enabled).then_some(ReasoningEffort::Off),
            )
            .map_err(|error| AgentError::Invalid(error.to_string()))?;
            (enabled
                && validate_reasoning_effort(&chat.snapshot.model, Some(ReasoningEffort::On))
                    .is_ok())
            .then_some(ReasoningEffort::On)
        };
        self.write_user_settings(move |settings| {
            settings.write(SettingsUpdate::Thinking {
                enabled,
                effort: enable_effort,
            })
        })
        .await?;
        let mut chats = self.inner.chats.lock().await;
        let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
        validate_reasoning_effort(
            &chat.snapshot.model,
            (!enabled).then_some(ReasoningEffort::Off),
        )
        .map_err(|error| AgentError::Invalid(error.to_string()))?;
        chat.snapshot.thinking_enabled = enabled;
        if let Some(effort) = enable_effort {
            chat.snapshot.reasoning_effort = Some(effort);
        }
        Ok(())
    }

    pub(super) async fn save_context_window_setting(
        &self,
        tokens: usize,
    ) -> Result<(), AgentError> {
        self.write_user_settings(move |settings| {
            settings.write(SettingsUpdate::ContextWindow(tokens))
        })
        .await
    }

    pub async fn save_default_execution_mode(
        &self,
        mode: tickloop_harness_protocol::ExecutionMode,
    ) -> Result<(), AgentError> {
        self.write_user_settings(move |settings| {
            settings.write(SettingsUpdate::ExecutionMode(mode))
        })
        .await
    }

    /// Writes persistent rule changes to their respective project partitions in user settings.
    pub(super) async fn save_persistent_rule_changes(
        &self,
        changes: &RuleChanges,
    ) -> Result<(), AgentError> {
        if changes.is_empty() {
            return Ok(());
        }
        let changes = changes.clone();
        self.write_user_settings(move |settings| settings.write_permissions(&changes, None))
            .await
    }

    /// Settings writes include `sync_all`, so they run on a blocking thread.
    pub(super) async fn write_user_settings(
        &self,
        write: impl FnOnce(&dyn SettingsStore) -> Result<(), SettingsStoreError> + Send + 'static,
    ) -> Result<(), AgentError> {
        let Some(settings) = self.inner.settings.clone() else {
            return Ok(());
        };
        tokio::task::spawn_blocking(move || write(settings.as_ref()))
            .await
            .map_err(|error| AgentError::Storage(error.to_string()))?
            .map_err(|error| AgentError::Storage(error.to_string()))
    }

    /// Syncs persistent rule changes already written to settings into other Chats loaded by this
    /// process whose working directory belongs to that partition (except `skip`). Agents that do
    /// not read or write settings have no project partitions, so a persistent rule belongs only to
    /// the Chat that added it.
    pub(super) fn sync_persistent_rules(
        &self,
        chats: &mut HashMap<ChatId, Chat>,
        changes: &RuleChanges,
        skip: Option<ChatId>,
    ) {
        if self.inner.settings.is_none() {
            return;
        }
        for (chat_id, chat) in chats.iter_mut() {
            if Some(*chat_id) == skip || chat.snapshot.delegation.is_some() {
                continue;
            }
            if let Some(project) = chat.snapshot.folder.clone()
                && let Some(change) = changes.get(&project)
            {
                apply_rule_changes(chat, &project, change);
            }
        }
    }

    pub(super) async fn refresh_project_rules(&self, chat_id: ChatId) -> Result<(), AgentError> {
        let is_delegated = self
            .inner
            .chats
            .lock()
            .await
            .get(&chat_id)
            .ok_or(AgentError::NotFound)?
            .snapshot
            .delegation
            .is_some();
        let Some(settings) = self.inner.settings.clone() else {
            return Ok(());
        };
        let _settings_gate = self.inner.settings_writes.lock().await;
        let Some(folder) = self
            .inner
            .chats
            .lock()
            .await
            .get(&chat_id)
            .ok_or(AgentError::NotFound)?
            .snapshot
            .folder
            .clone()
        else {
            return Ok(());
        };
        let project = folder.clone();
        let project_settings =
            tokio::task::spawn_blocking(move || settings.read(Some(&project)).project)
                .await
                .map_err(|error| AgentError::Storage(error.to_string()))?;
        let mut rules = project_settings.rules;
        let warnings = project_settings.warnings;
        if is_delegated {
            rules.retain(|rule| matches!(rule.decision, Decision::Ask | Decision::Deny));
        }
        if !warnings.is_empty() {
            return Err(AgentError::Storage(format!(
                "project permission rules are unavailable: {}",
                warnings.join("; ")
            )));
        }
        let mut chats = self.inner.chats.lock().await;
        let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
        if chat.snapshot.folder.as_ref() != Some(&folder) {
            return Err(AgentError::Busy);
        }
        let current = chat
            .snapshot
            .policy
            .rules
            .iter()
            .filter(|rule| rule.persistent)
            .collect::<Vec<_>>();
        if serde_json::to_value(&current).expect("rules serialize")
            == serde_json::to_value(&rules).expect("rules serialize")
        {
            return Ok(());
        }
        chat.snapshot.policy.rules.retain(|rule| !rule.persistent);
        chat.bookkeeping.persistent_rule_projects =
            rules.iter().map(|rule| (rule.id, folder.clone())).collect();
        chat.snapshot.policy.rules.extend(rules);
        chat.snapshot.policy.revision += 1;
        self.save(&chat.snapshot);
        Ok(())
    }

    /// Following Claude Code's "don't ask again": persistent rules added for a permission request
    /// are written to this project's partition in user settings and added to the requesting Chat
    /// and to same-project Chats loaded by this process. The caller holds `settings_writes`.
    pub(super) async fn add_persistent_rule_for_request(
        &self,
        chat_id: ChatId,
        request_id: Uuid,
        allow: bool,
        specifier: Option<String>,
    ) -> Result<(), AgentError> {
        let (project, rule) = {
            let chats = self.inner.chats.lock().await;
            let chat = chats.get(&chat_id).ok_or(AgentError::NotFound)?;
            // Validate against the answer-time conditions first, so stale requests are not written
            // to settings; the answer phase validates again.
            let request = super::answerable_permission_request(chat, request_id, allow)?;
            if !self.inner.approvals.lock().await.contains_key(&request_id) {
                return Err(AgentError::Invalid(
                    "permission was already answered".into(),
                ));
            }
            let mcp_rule = match (&request.mcp_server, &specifier) {
                (Some(server), Some(target)) => {
                    if target != &request.name && target != &format!("mcp__{server}") {
                        return Err(AgentError::Invalid(
                            "MCP permission rule must name this tool or its server".into(),
                        ));
                    }
                    Some(target.clone())
                }
                _ => None,
            };
            let specifier = if mcp_rule.is_some() { None } else { specifier };
            // A tool whose rules name one aspect of the call, such as the WebFetch domain, never
            // saves an exact-input rule.
            let specifier = specifier.or_else(|| {
                tickloop_harness_tools::permissions::permission_rule_content(
                    &request.name,
                    &request.input,
                )
            });
            if let Some(specifier) = &specifier {
                let candidates = tickloop_harness_tools::permissions::permission_rule_candidates(
                    &request.name,
                    &request.input,
                );
                if !candidates
                    .iter()
                    .any(|candidate| candidate.specifier == *specifier)
                {
                    return Err(AgentError::Invalid(
                        "permission rule is not one of the displayed candidates".into(),
                    ));
                }
            }
            let input = (specifier.is_none() && mcp_rule.is_none()).then_some(request.input);
            let rule = PermissionRule {
                id: tickloop_ids::generate(),
                decision: if allow {
                    Decision::Allow
                } else {
                    Decision::Deny
                },
                tool: mcp_rule.unwrap_or(request.name),
                specifier,
                input,
                loop_id: None,
                persistent: true,
            };
            (super::require_folder(&chat.snapshot)?.to_path_buf(), rule)
        };
        let changes = RuleChanges::from([(
            project.clone(),
            ProjectRuleChanges {
                removed: HashSet::new(),
                added: vec![rule],
            },
        )]);
        self.save_persistent_rule_changes(&changes).await?;
        // The rule is already in settings: even if the request expired or was cancelled during the
        // write, loaded Chats stay consistent with settings.
        let mut chats = self.inner.chats.lock().await;
        self.sync_persistent_rules(&mut chats, &changes, Some(chat_id));
        if let Some(chat) = chats.get_mut(&chat_id) {
            apply_rule_changes(chat, &project, &changes[&project]);
        }
        Ok(())
    }
}

/// Persistent rule changes per project partition when the Chat's rule set changes from the current
/// one to `rules`. Rules are compared by full content: a rule whose content changed (same id) is
/// both deleted (old) and added (new). Deletions and modifications are written back to the
/// partition recorded for the rule; new rules go to the working directory's partition.
pub(super) fn persistent_rule_changes(
    snapshot: &ChatSnapshot,
    bookkeeping: &ChatBookkeeping,
    rules: &[PermissionRule],
) -> Result<RuleChanges, AgentError> {
    if rules
        .iter()
        .any(|rule| rule.persistent && rule.loop_id.is_some())
    {
        return Err(AgentError::Invalid(
            "persistent rules apply to every Chat in the project and cannot name a Loop".into(),
        ));
    }
    let previous = snapshot
        .policy
        .rules
        .iter()
        .filter(|rule| rule.persistent)
        .collect::<Vec<_>>();
    let current = rules
        .iter()
        .filter(|rule| rule.persistent)
        .collect::<Vec<_>>();
    let Some(folder) = &snapshot.folder else {
        return if current.is_empty() {
            Ok(RuleChanges::new())
        } else {
            Err(AgentError::Invalid(format!(
                "{}, so it has no project for persistent rules",
                super::FOLDERLESS_CHAT
            )))
        };
    };
    let project_of = |id: &Uuid| {
        bookkeeping
            .persistent_rule_projects
            .get(id)
            .cloned()
            .unwrap_or_else(|| folder.clone())
    };
    let mut changes = RuleChanges::new();
    for rule in &previous {
        if !current.iter().any(|kept| same_rule(kept, rule)) {
            changes
                .entry(project_of(&rule.id))
                .or_default()
                .removed
                .insert(rule.id);
        }
    }
    for rule in &current {
        if !previous.iter().any(|existing| same_rule(existing, rule)) {
            changes
                .entry(project_of(&rule.id))
                .or_default()
                .added
                .push((*rule).clone());
        }
    }
    Ok(changes)
}

/// After the caller has replaced the whole rule set, updates the record of which partition each
/// persistent rule belongs to.
pub(super) fn record_rule_projects(bookkeeping: &mut ChatBookkeeping, changes: &RuleChanges) {
    for (project, change) in changes {
        for id in &change.removed {
            bookkeeping.persistent_rule_projects.remove(id);
        }
        for rule in &change.added {
            bookkeeping
                .persistent_rule_projects
                .insert(rule.id, project.clone());
        }
    }
}

/// Applies one partition's persistent rule changes to the Chat's rule set; bumps the rule version
/// when anything changed.
fn apply_rule_changes(chat: &mut Chat, project: &Path, change: &ProjectRuleChanges) {
    let replaced = change
        .removed
        .iter()
        .chain(change.added.iter().map(|rule| &rule.id))
        .collect::<HashSet<_>>();
    let policy = &mut chat.snapshot.policy;
    let count = policy.rules.len();
    policy
        .rules
        .retain(|rule| !(rule.persistent && replaced.contains(&rule.id)));
    let is_changed = policy.rules.len() != count || !change.added.is_empty();
    for id in replaced {
        chat.bookkeeping.persistent_rule_projects.remove(id);
    }
    for rule in &change.added {
        policy.rules.push(rule.clone());
        chat.bookkeeping
            .persistent_rule_projects
            .insert(rule.id, project.to_path_buf());
    }
    if is_changed {
        policy.revision += 1;
    }
}

fn same_rule(left: &PermissionRule, right: &PermissionRule) -> bool {
    left.id == right.id
        && left.decision == right.decision
        && left.tool == right.tool
        && left.specifier == right.specifier
        && left.input == right.input
        && left.loop_id == right.loop_id
}

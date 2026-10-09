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

use super::{Agent, AgentError, Chat, ChatId, ChatSnapshot};
use crate::model::{ModelClient, ModelConfig, ReasoningEffort, validate_reasoning_effort};
use crate::output_style::OutputStyle;
use crate::permissions::{Decision, PermissionRule};
use crate::settings::{LoadedSettings, ProjectRuleChanges, SettingsError, UserSettings};

/// Persistent rule changes partitioned by canonical project working directory.
pub(super) type RuleChanges = BTreeMap<PathBuf, ProjectRuleChanges>;

pub(super) struct SelectedModel {
    pub(super) config: ModelConfig,
    pub(super) client: Arc<dyn ModelClient>,
}

/// Defaults for a new root Chat from user settings and host options. Without saved settings,
/// only execution mode follows host options; other fields keep the empty Chat defaults.
/// The New Agent draft uses the same rules to preview its first creation.
pub(crate) struct NewChatSettings {
    pub(crate) execution_mode: crate::permissions::ExecutionMode,
    pub(crate) reasoning_effort: Option<ReasoningEffort>,
    pub(crate) thinking_enabled: bool,
    pub(crate) context_window_tokens: Option<usize>,
}

pub(crate) fn new_chat_settings(
    saved: Option<&mut LoadedSettings>,
    host_options: &super::AgentHostOptions,
    model: &ModelConfig,
) -> NewChatSettings {
    let host_mode = host_options.execution_mode_override;
    let Some(saved) = saved else {
        return NewChatSettings {
            execution_mode: host_mode.unwrap_or(crate::permissions::ExecutionMode::Auto),
            reasoning_effort: None,
            thinking_enabled: true,
            context_window_tokens: None,
        };
    };
    let execution_mode = host_mode
        .or_else(|| saved.default_execution_mode())
        .unwrap_or(crate::permissions::ExecutionMode::Auto);
    let context_window_tokens = if model.maximum_context_window_tokens().is_some() {
        None
    } else {
        saved
            .context_window_tokens()
            .filter(|tokens| super::context::is_usable_context_window(*tokens, model))
    };
    let reasoning_effort = match saved.reasoning_effort() {
        Some(effort) if validate_reasoning_effort(model, Some(effort)).is_ok() => Some(effort),
        Some(effort) => {
            saved.warn(format!(
                "`effortLevel` {} is unsupported by {}; using auto",
                effort.label(),
                model.model
            ));
            None
        }
        None => None,
    };
    NewChatSettings {
        execution_mode,
        reasoning_effort,
        thinking_enabled: saved.thinking_enabled(),
        context_window_tokens,
    }
}

impl Agent {
    /// 最近一次为该 Chat 读取用户设置（新建、恢复、Branch、切换工作目录）时发现的问题，
    /// 例如设置文件不是合法 JSON 导致持久规则没有生效。
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
        let output_style = self.apply_new_chat_settings(snapshot);
        (client, output_style)
    }

    pub(super) fn apply_new_chat_settings(&self, snapshot: &mut ChatSnapshot) -> OutputStyle {
        let mut saved = self.inner.settings.as_ref().map(UserSettings::load);
        let defaults = new_chat_settings(saved.as_mut(), &self.inner.host_options, &snapshot.model);
        snapshot.policy.mode = defaults.execution_mode;
        let Some(mut saved) = saved else {
            return OutputStyle::default();
        };
        snapshot.context_window_tokens = defaults.context_window_tokens;
        snapshot.reasoning_effort = defaults.reasoning_effort;
        snapshot.thinking_enabled = defaults.thinking_enabled;
        let output_style = saved.output_style();
        self.replace_persistent_rules(snapshot, saved);
        output_style
    }

    /// 在阻塞线程中读取用户设置；不读写设置的 Agent 返回 `None`。
    pub(super) async fn load_user_settings(&self) -> Result<Option<LoadedSettings>, AgentError> {
        let Some(settings) = self.inner.settings.clone() else {
            return Ok(None);
        };
        tokio::task::spawn_blocking(move || settings.load())
            .await
            .map(Some)
            .map_err(|error| AgentError::Storage(error.to_string()))
    }

    /// 把 Chat 的持久规则换成设置中其工作目录分区的规则，会话规则不变，并记录读取警告。
    pub(super) fn replace_persistent_rules(
        &self,
        snapshot: &mut ChatSnapshot,
        mut saved: LoadedSettings,
    ) {
        if snapshot.delegation.is_some() {
            return;
        }
        let rules = saved.permission_rules(&snapshot.workdir);
        snapshot.policy.rules.retain(|rule| !rule.persistent);
        snapshot.persistent_rule_projects = rules
            .iter()
            .map(|rule| (rule.id, snapshot.workdir.clone()))
            .collect();
        snapshot.policy.rules.extend(rules);
        self.inner
            .settings_warnings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(snapshot.id, saved.into_warnings());
    }

    /// 之后新建或恢复的 Chat 使用这个模型。
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
        self.write_user_settings(move |settings| settings.save_model(&config, clear_context_window))
            .await
    }

    pub(super) async fn save_model_and_thinking_setting(
        &self,
        config: ModelConfig,
        clear_context_window: bool,
        effort: Option<ReasoningEffort>,
    ) -> Result<(), AgentError> {
        self.write_user_settings(move |settings| {
            settings.save_model_with_thinking(&config, clear_context_window, effort)
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
        self.write_user_settings(move |settings| settings.save_reasoning_effort(effort))
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
            settings.save_thinking_enabled_with_effort(enabled, enable_effort)
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
        self.write_user_settings(move |settings| settings.save_context_window(tokens))
            .await
    }

    pub async fn save_default_execution_mode(
        &self,
        mode: crate::permissions::ExecutionMode,
    ) -> Result<(), AgentError> {
        self.write_user_settings(move |settings| settings.save_default_execution_mode(mode))
            .await
    }

    /// 把持久规则的改动写入用户设置中各自的项目分区。
    pub(super) async fn save_persistent_rule_changes(
        &self,
        changes: &RuleChanges,
    ) -> Result<(), AgentError> {
        if changes.is_empty() {
            return Ok(());
        }
        let changes = changes.clone();
        self.write_user_settings(move |settings| {
            settings.update_permission_rules(
                changes
                    .iter()
                    .map(|(project, change)| (project.as_path(), change)),
            )
        })
        .await
    }

    /// 设置写入包含 `sync_all`，在阻塞线程中进行。
    async fn write_user_settings(
        &self,
        write: impl FnOnce(&UserSettings) -> Result<(), SettingsError> + Send + 'static,
    ) -> Result<(), AgentError> {
        let Some(settings) = self.inner.settings.clone() else {
            return Ok(());
        };
        tokio::task::spawn_blocking(move || write(&settings))
            .await
            .map_err(|error| AgentError::Storage(error.to_string()))?
            .map_err(|error| AgentError::Storage(error.to_string()))
    }

    /// 已写入设置的持久规则改动同步到本进程已加载、工作目录属于该分区的其他 Chat（`skip` 除外）。
    /// 不读写设置的 Agent 没有项目分区，持久规则只属于添加它的 Chat。
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
            if let Some(change) = changes.get(&chat.snapshot.workdir) {
                let project = chat.snapshot.workdir.clone();
                apply_rule_changes(&mut chat.snapshot, &project, change);
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
        let workdir = {
            let chats = self.inner.chats.lock().await;
            chats
                .get(&chat_id)
                .ok_or(AgentError::NotFound)?
                .snapshot
                .workdir
                .clone()
        };
        let project = workdir.clone();
        let (mut rules, warnings, _) =
            tokio::task::spawn_blocking(move || settings.project_rules_with_revision(&project))
                .await
                .map_err(|error| AgentError::Storage(error.to_string()))?
                .map_err(|error| AgentError::Storage(error.to_string()))?;
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
        if chat.snapshot.workdir != workdir {
            return Err(AgentError::Busy);
        }
        if let Some(delegation) = &chat.snapshot.delegation {
            let frozen = delegation
                .ceiling
                .policy
                .for_delegation(delegation.parent_loop_id);
            rules.extend(
                frozen
                    .rules
                    .into_iter()
                    .filter(|rule| {
                        rule.persistent
                            && rule.decision == Decision::Allow
                            && !rules.iter().any(|current| current.id == rule.id)
                    })
                    .collect::<Vec<_>>(),
            );
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
        chat.snapshot.persistent_rule_projects = rules
            .iter()
            .map(|rule| (rule.id, workdir.clone()))
            .collect();
        chat.snapshot.policy.rules.extend(rules);
        chat.snapshot.policy.revision += 1;
        self.save(&chat.snapshot);
        Ok(())
    }

    /// 照 Claude Code"不再询问"：为权限请求添加的持久规则写入用户设置中本项目的分区，
    /// 并加入发起请求的 Chat 与本进程已加载的同项目 Chat。调用方持有 `settings_writes`。
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
            // 先按回答时的条件校验，失效的请求不写入设置；回答阶段仍会再次校验。
            let request = super::answerable_permission_request(chat, request_id, allow)?;
            if !self.inner.approvals.lock().await.contains_key(&request_id) {
                return Err(AgentError::Invalid(
                    "permission was already answered".into(),
                ));
            }
            // A tool whose rules name one aspect of the call, such as the WebFetch domain, never
            // saves an exact-input rule.
            let specifier = specifier.or_else(|| {
                crate::permissions::permission_rule_content(&request.name, &request.input)
            });
            if let Some(specifier) = &specifier {
                let candidates =
                    crate::permissions::permission_rule_candidates(&request.name, &request.input);
                if !candidates
                    .iter()
                    .any(|candidate| candidate.specifier == *specifier)
                {
                    return Err(AgentError::Invalid(
                        "permission rule is not one of the displayed candidates".into(),
                    ));
                }
            }
            let input = specifier.is_none().then_some(request.input);
            let rule = PermissionRule {
                id: Uuid::new_v4(),
                decision: if allow {
                    Decision::Allow
                } else {
                    Decision::Deny
                },
                tool: request.name,
                specifier,
                input,
                loop_id: None,
                persistent: true,
            };
            (chat.snapshot.workdir.clone(), rule)
        };
        let changes = RuleChanges::from([(
            project.clone(),
            ProjectRuleChanges {
                removed: HashSet::new(),
                added: vec![rule],
            },
        )]);
        self.save_persistent_rule_changes(&changes).await?;
        // 规则已在设置中：即使写入期间请求过期或被取消，已加载的 Chat 也与设置保持一致。
        let mut chats = self.inner.chats.lock().await;
        self.sync_persistent_rules(&mut chats, &changes, Some(chat_id));
        if let Some(chat) = chats.get_mut(&chat_id) {
            apply_rule_changes(&mut chat.snapshot, &project, &changes[&project]);
        }
        Ok(())
    }
}

/// Chat 规则集从当前变为 `rules` 时各项目分区的持久规则改动。规则按完整内容比较：内容变化
/// 的规则（同 id）既删除旧的也添加新的。删除与修改写回规则记录的分区，新规则写入工作目录的分区。
pub(super) fn persistent_rule_changes(
    snapshot: &ChatSnapshot,
    rules: &[PermissionRule],
) -> Result<RuleChanges, AgentError> {
    if rules
        .iter()
        .any(|rule| rule.persistent && rule.loop_id.is_some())
    {
        return Err(AgentError::Invalid(
            "persistent rules apply to every goal in the project and cannot name a goal".into(),
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
    let project_of = |id: &Uuid| {
        snapshot
            .persistent_rule_projects
            .get(id)
            .cloned()
            .unwrap_or_else(|| snapshot.workdir.clone())
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

/// 调用方已把规则集整体替换后，更新持久规则所属分区的记录。
pub(super) fn record_rule_projects(snapshot: &mut ChatSnapshot, changes: &RuleChanges) {
    for (project, change) in changes {
        for id in &change.removed {
            snapshot.persistent_rule_projects.remove(id);
        }
        for rule in &change.added {
            snapshot
                .persistent_rule_projects
                .insert(rule.id, project.clone());
        }
    }
}

/// 在 Chat 的规则集中应用一个分区的持久规则改动；有变化时递增规则版本。
fn apply_rule_changes(snapshot: &mut ChatSnapshot, project: &Path, change: &ProjectRuleChanges) {
    let replaced = change
        .removed
        .iter()
        .chain(change.added.iter().map(|rule| &rule.id))
        .collect::<HashSet<_>>();
    let count = snapshot.policy.rules.len();
    snapshot
        .policy
        .rules
        .retain(|rule| !(rule.persistent && replaced.contains(&rule.id)));
    let is_changed = snapshot.policy.rules.len() != count || !change.added.is_empty();
    for id in replaced {
        snapshot.persistent_rule_projects.remove(id);
    }
    for rule in &change.added {
        snapshot.policy.rules.push(rule.clone());
        snapshot
            .persistent_rule_projects
            .insert(rule.id, project.to_path_buf());
    }
    if is_changed {
        snapshot.policy.revision += 1;
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

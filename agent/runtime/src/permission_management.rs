use std::path::PathBuf;

use tickloop_agent_config::{AutoSettings, SettingsError};
use tickloop_agent_tools::permissions::{canonical_directory, rule_views, validate_rule};

use super::{Agent, AgentError, ChatBookkeeping};
use tickloop_agent_protocol::{
    ApiKeyStatus, AutoReviewerHealth, AutoReviewerPreference, AutoSettingsRevision, ChatId,
    ChatSnapshot, PermissionChange, PermissionRule, PermissionsRevision, PermissionsView,
};

#[derive(Debug, thiserror::Error)]
pub enum PermissionChangeError {
    #[error("permissions changed while editing")]
    Conflict { latest: Box<PermissionsView> },
    #[error("invalid permission change: {0}")]
    Invalid(String),
    #[error(transparent)]
    Agent(#[from] AgentError),
}

impl Agent {
    pub async fn permissions_view(&self, chat_id: ChatId) -> Result<PermissionsView, AgentError> {
        let _settings_gate = self.inner.settings_writes.lock().await;
        self.permissions_view_locked(chat_id).await
    }

    pub(super) async fn permissions_view_locked(
        &self,
        chat_id: ChatId,
    ) -> Result<PermissionsView, AgentError> {
        let workdir = {
            let chats = self.inner.chats.lock().await;
            chats
                .get(&chat_id)
                .ok_or(AgentError::NotFound)?
                .snapshot
                .workdir
                .clone()
        };
        let (saved_rules, warnings, project_rules) = self.read_project_rules(&workdir).await?;
        let (auto_settings, auto_settings_revision) = self.auto_settings_snapshot().await?;
        let jev_key_status = self.jev_key_status().await?;
        let auto_reviewer_health = self
            .inner
            .auto_health
            .lock()
            .await
            .status(tokio::time::Instant::now());
        let mut chats = self.inner.chats.lock().await;
        let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
        if chat.snapshot.workdir != workdir {
            return Err(AgentError::Busy);
        }
        if let Some(saved_rules) =
            saved_rules.filter(|_| warnings.is_empty() && chat.snapshot.delegation.is_none())
        {
            let current = chat
                .snapshot
                .policy
                .rules
                .iter()
                .filter(|rule| rule.persistent)
                .collect::<Vec<_>>();
            if serde_json::to_value(&current).expect("rules serialize")
                != serde_json::to_value(&saved_rules).expect("rules serialize")
            {
                chat.snapshot.policy.rules.retain(|rule| !rule.persistent);
                chat.bookkeeping.persistent_rule_projects = saved_rules
                    .iter()
                    .map(|rule| (rule.id, workdir.clone()))
                    .collect();
                chat.snapshot.policy.rules.extend(saved_rules);
                chat.snapshot.policy.revision += 1;
                self.save(&chat.snapshot);
            }
        }
        Ok(view_from_snapshot(
            &chat.snapshot,
            &chat.bookkeeping,
            chat.client.identity().requested_model,
            project_rules,
            warnings,
            AutoViewState {
                settings: auto_settings,
                revision: auto_settings_revision,
                key_status: jev_key_status,
                health: auto_reviewer_health,
            },
        ))
    }

    pub async fn apply_permission_change(
        &self,
        chat_id: ChatId,
        change: PermissionChange,
        expected_revision: PermissionsRevision,
    ) -> Result<PermissionsView, PermissionChangeError> {
        self.require_root_chat(chat_id, "change permission rules or directories")
            .await?;
        let _settings_gate = self.inner.settings_writes.lock().await;
        let latest = self.permissions_view_locked(chat_id).await?;
        if latest.revision != expected_revision {
            return Err(PermissionChangeError::Conflict {
                latest: Box::new(latest),
            });
        }
        match change {
            PermissionChange::UpsertRule { rule } => {
                validate_rule(&rule)
                    .map_err(|reason| PermissionChangeError::Invalid(reason.into()))?;
                let (workdir, rules, changes) = {
                    let chats = self.inner.chats.lock().await;
                    let chat = chats.get(&chat_id).ok_or(AgentError::NotFound)?;
                    let snapshot = &chat.snapshot;
                    validate_editable_rule(snapshot, &rule)?;
                    let mut rules = snapshot.policy.rules.clone();
                    if let Some(index) = rules.iter().position(|existing| existing.id == rule.id) {
                        if serde_json::to_value(&rules[index]).expect("rule serializes")
                            == serde_json::to_value(&rule).expect("rule serializes")
                        {
                            return Err(PermissionChangeError::Invalid("rule is unchanged".into()));
                        }
                        rules[index] = rule;
                    } else {
                        rules.push(rule);
                    }
                    let changes = super::chat_settings::persistent_rule_changes(
                        snapshot,
                        &chat.bookkeeping,
                        &rules,
                    )?;
                    (snapshot.workdir.clone(), rules, changes)
                };
                self.apply_rule_change(chat_id, workdir, rules, changes, &expected_revision)
                    .await?;
            }
            PermissionChange::RemoveRule { id } => {
                let (workdir, rules, changes) = {
                    let chats = self.inner.chats.lock().await;
                    let chat = chats.get(&chat_id).ok_or(AgentError::NotFound)?;
                    let snapshot = &chat.snapshot;
                    if snapshot
                        .policy
                        .project_constraints
                        .iter()
                        .chain(&snapshot.policy.managed_constraints)
                        .any(|constraint| constraint.id == id)
                    {
                        return Err(PermissionChangeError::Invalid("rule is read-only".into()));
                    }
                    let mut rules = snapshot.policy.rules.clone();
                    let count = rules.len();
                    rules.retain(|rule| rule.id != id);
                    if count == rules.len() {
                        return Err(PermissionChangeError::Invalid(
                            "rule does not exist or is read-only".into(),
                        ));
                    }
                    let changes = super::chat_settings::persistent_rule_changes(
                        snapshot,
                        &chat.bookkeeping,
                        &rules,
                    )?;
                    (snapshot.workdir.clone(), rules, changes)
                };
                self.apply_rule_change(chat_id, workdir, rules, changes, &expected_revision)
                    .await?;
            }
            PermissionChange::AddDirectory { path } => {
                let workdir = latest.working_directory.clone();
                self.require_working_directory_trust(&workdir)?;
                let path = canonical_directory(if path.is_absolute() {
                    path
                } else {
                    workdir.join(path)
                })
                .map_err(|reason| PermissionChangeError::Invalid(reason.into()))?;
                let mut chats = self.inner.chats.lock().await;
                let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
                if chat.snapshot.policy.revision != expected_revision.local()
                    || chat.snapshot.workdir != workdir
                {
                    drop(chats);
                    let latest = self.permissions_view_locked(chat_id).await?;
                    return Err(PermissionChangeError::Conflict {
                        latest: Box::new(latest),
                    });
                }
                if chat.snapshot.additional_directories.contains(&path) {
                    return Err(PermissionChangeError::Invalid(
                        "directory is already configured".into(),
                    ));
                }
                chat.snapshot.additional_directories.push(path);
                chat.snapshot.policy.revision += 1;
                self.save(&chat.snapshot);
            }
            PermissionChange::RemoveDirectory { path } => {
                let workdir = latest.working_directory.clone();
                self.require_working_directory_trust(&workdir)?;
                let path = if path.is_absolute() {
                    path
                } else {
                    workdir.join(path)
                };
                let path = path.canonicalize().unwrap_or(path);
                let mut chats = self.inner.chats.lock().await;
                let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
                if chat.snapshot.policy.revision != expected_revision.local()
                    || chat.snapshot.workdir != workdir
                {
                    drop(chats);
                    let latest = self.permissions_view_locked(chat_id).await?;
                    return Err(PermissionChangeError::Conflict {
                        latest: Box::new(latest),
                    });
                }
                let old_len = chat.snapshot.additional_directories.len();
                chat.snapshot
                    .additional_directories
                    .retain(|existing| existing != &path);
                if old_len == chat.snapshot.additional_directories.len() {
                    return Err(PermissionChangeError::Invalid(
                        "directory is not configured".into(),
                    ));
                }
                chat.snapshot.policy.revision += 1;
                self.save(&chat.snapshot);
            }
            PermissionChange::SelectReviewer { reviewer } => {
                if latest.preferred_reviewer == reviewer {
                    return Err(PermissionChangeError::Invalid(
                        "reviewer is unchanged".into(),
                    ));
                }
                if reviewer == AutoReviewerPreference::Jev
                    && latest.jev_key_status == ApiKeyStatus::Missing
                {
                    return Err(PermissionChangeError::Invalid("Jev key is missing".into()));
                }
                let settings = self.inner.settings.clone().ok_or_else(|| {
                    PermissionChangeError::Invalid("Auto reviewer settings are unavailable".into())
                })?;
                let expected_auto = expected_revision.auto_settings().clone();
                let result = tokio::task::spawn_blocking(move || {
                    settings.update_auto_reviewer_cas(&expected_auto, reviewer)
                })
                .await
                .map_err(|error| AgentError::Storage(error.to_string()))?;
                match result {
                    Ok(_) => {}
                    Err(SettingsError::Conflict) => {
                        let latest = self.permissions_view_locked(chat_id).await?;
                        return Err(PermissionChangeError::Conflict {
                            latest: Box::new(latest),
                        });
                    }
                    Err(error) => return Err(AgentError::Storage(error.to_string()).into()),
                }
            }
            PermissionChange::ChangeAutoRules { change } => {
                let mut rules = latest.auto_rule_settings.clone();
                rules
                    .apply(change)
                    .map_err(PermissionChangeError::Invalid)?;
                let settings = self.inner.settings.clone().ok_or_else(|| {
                    PermissionChangeError::Invalid("Auto rule settings are unavailable".into())
                })?;
                let expected_auto = expected_revision.auto_settings().clone();
                let result = tokio::task::spawn_blocking(move || {
                    settings.update_auto_rules_cas(&expected_auto, &rules)
                })
                .await
                .map_err(|error| AgentError::Storage(error.to_string()))?;
                match result {
                    Ok(_) => {}
                    Err(SettingsError::Conflict) => {
                        let latest = self.permissions_view_locked(chat_id).await?;
                        return Err(PermissionChangeError::Conflict {
                            latest: Box::new(latest),
                        });
                    }
                    Err(error) => return Err(AgentError::Storage(error.to_string()).into()),
                }
            }
        }
        self.permissions_view_locked(chat_id)
            .await
            .map_err(Into::into)
    }

    pub(super) async fn apply_rule_change(
        &self,
        chat_id: ChatId,
        workdir: PathBuf,
        rules: Vec<PermissionRule>,
        changes: super::chat_settings::RuleChanges,
        expected: &PermissionsRevision,
    ) -> Result<(), PermissionChangeError> {
        let mut chats = self.inner.chats.lock().await;
        let snapshot = &chats.get(&chat_id).ok_or(AgentError::NotFound)?.snapshot;
        if snapshot.policy.revision != expected.local() || snapshot.workdir != workdir {
            drop(chats);
            let latest = self.permissions_view_locked(chat_id).await?;
            return Err(PermissionChangeError::Conflict {
                latest: Box::new(latest),
            });
        }
        if !changes.is_empty() {
            let settings = self.inner.settings.clone();
            if let Some(settings) = settings {
                let project_revision = expected.project_rules().to_owned();
                let changes_for_write = changes.clone();
                let result = tokio::task::spawn_blocking(move || {
                    settings.update_permission_rules_cas(
                        &workdir,
                        &project_revision,
                        changes_for_write
                            .iter()
                            .map(|(path, change)| (path.as_path(), change)),
                    )
                })
                .await
                .map_err(|error| AgentError::Storage(error.to_string()))?;
                match result {
                    Ok(()) => {}
                    Err(SettingsError::Conflict) => {
                        drop(chats);
                        let latest = self.permissions_view_locked(chat_id).await?;
                        return Err(PermissionChangeError::Conflict {
                            latest: Box::new(latest),
                        });
                    }
                    Err(error) => return Err(AgentError::Storage(error.to_string()).into()),
                }
            }
        }
        self.sync_persistent_rules(&mut chats, &changes, Some(chat_id));
        let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
        chat.snapshot.policy.rules = rules;
        super::chat_settings::record_rule_projects(&mut chat.bookkeeping, &changes);
        chat.snapshot.policy.revision += 1;
        self.save(&chat.snapshot);
        Ok(())
    }

    async fn read_project_rules(
        &self,
        project: &std::path::Path,
    ) -> Result<(Option<Vec<PermissionRule>>, Vec<String>, String), AgentError> {
        let Some(settings) = self.inner.settings.clone() else {
            return Ok((None, Vec::new(), String::new()));
        };
        let project = project.to_path_buf();
        let (rules, warnings, revision) =
            tokio::task::spawn_blocking(move || settings.project_rules_with_revision(&project))
                .await
                .map_err(|error| AgentError::Storage(error.to_string()))?
                .map_err(|error| AgentError::Storage(error.to_string()))?;
        Ok((Some(rules), warnings, revision))
    }
}

struct AutoViewState {
    settings: AutoSettings,
    revision: AutoSettingsRevision,
    key_status: ApiKeyStatus,
    health: AutoReviewerHealth,
}

fn view_from_snapshot(
    snapshot: &ChatSnapshot,
    bookkeeping: &ChatBookkeeping,
    chat_model_label: String,
    project_rules: String,
    warnings: Vec<String>,
    auto: AutoViewState,
) -> PermissionsView {
    let AutoViewState {
        settings: auto_settings,
        revision: auto_settings_revision,
        key_status: jev_key_status,
        health: auto_reviewer_health,
    } = auto;
    let rules = rule_views(
        &snapshot.policy.rules,
        |rule| {
            bookkeeping
                .persistent_rule_projects
                .get(&rule.id)
                .cloned()
                .unwrap_or_else(|| snapshot.workdir.clone())
        },
        &snapshot.workdir,
        snapshot.policy.mode,
        &snapshot.policy.project_constraints,
        &snapshot.policy.managed_constraints,
    );
    PermissionsView {
        chat_id: snapshot.id,
        revision: PermissionsRevision::new(
            snapshot.policy.revision,
            project_rules,
            auto_settings_revision.clone(),
        ),
        execution_mode: snapshot.policy.mode,
        chat_model_label,
        preferred_reviewer: auto_settings.reviewer,
        auto_policy: auto_settings.policy_for_directory(&snapshot.workdir),
        auto_rule_settings: auto_settings.rules,
        auto_settings_revision,
        jev_key_status,
        auto_reviewer_health,
        selected_loop_id: snapshot.loops.last().map(|loop_snapshot| loop_snapshot.id),
        rules,
        working_directory: snapshot.workdir.clone(),
        additional_directories: snapshot.additional_directories.clone(),
        warnings,
    }
}

fn validate_editable_rule(
    snapshot: &ChatSnapshot,
    rule: &PermissionRule,
) -> Result<(), PermissionChangeError> {
    if snapshot
        .policy
        .project_constraints
        .iter()
        .chain(&snapshot.policy.managed_constraints)
        .any(|constraint| constraint.id == rule.id)
    {
        return Err(PermissionChangeError::Invalid("rule is read-only".into()));
    }
    if let Some(loop_id) = rule.loop_id
        && !snapshot
            .loops
            .iter()
            .any(|loop_snapshot| loop_snapshot.id == loop_id)
    {
        return Err(PermissionChangeError::Invalid(
            "Loop does not exist in this Chat".into(),
        ));
    }
    Ok(())
}

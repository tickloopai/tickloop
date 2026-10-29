//! Session-only configuration of an idle managed Chat. This never selects a process default or
//! writes user settings; the host must still serialize its managed control operations.

use std::sync::atomic::Ordering;

use tickloop_harness_model::{validate_model_config, validate_reasoning_effort};
use tickloop_harness_protocol::{
    ChatId, CommandState, ExecutionMode, LoopState, ModelConfig, OperationState, ReasoningEffort,
    UserShellState,
};

use crate::{Agent, AgentError, Chat};

impl Agent {
    /// Removes a never-published managed Chat after creation failed. No execution may have been
    /// admitted: this intentionally refuses to discard an existing session or any work.
    pub async fn discard_unpublished_managed_chat(
        &self,
        chat_id: ChatId,
    ) -> Result<(), AgentError> {
        self.require_managed_configuration()?;
        let mut chats = self.inner.chats.lock().await;
        let chat = chats.get(&chat_id).ok_or(AgentError::NotFound)?;
        require_managed_root(chat)?;
        if !chat.snapshot.messages.is_empty() || !managed_chat_idle(chat).await? {
            return Err(AgentError::Busy);
        }
        chats.remove(&chat_id);
        drop(chats);
        self.inner.subagent_registry.lock().await.remove(&chat_id);
        self.release_acp_session_claim(chat_id).await;
        Ok(())
    }

    pub async fn user_skill_catalog(
        &self,
        chat_id: ChatId,
    ) -> Result<Vec<(String, String)>, AgentError> {
        self.rediscover_skills(chat_id).await;
        let chats = self.inner.chats.lock().await;
        let chat = chats.get(&chat_id).ok_or(AgentError::NotFound)?;
        Ok(chat
            .skills
            .definitions()
            .filter(|skill| skill.user_invocable())
            .map(|skill| (skill.name.clone(), skill.description.clone()))
            .collect())
    }

    /// Whether a managed root Chat has no execution, queued work, waits or unresolved commands.
    /// This observation reserves nothing; configuration checks again under the mutation lock.
    pub async fn managed_session_idle(&self, chat_id: ChatId) -> Result<bool, AgentError> {
        self.require_managed_configuration()?;
        let chats = self.inner.chats.lock().await;
        let chat = chats.get(&chat_id).ok_or(AgentError::NotFound)?;
        require_managed_root(chat)?;
        managed_chat_idle(chat).await
    }

    /// Changes only this managed session, atomically with its idle check. Invalid requests and
    /// busy Chats leave the model, execution mode and effort unchanged. Ask is `AcceptEdits`.
    /// `effort` is checked against the resulting model; `Some(None)` is the automatic effort.
    pub async fn configure_managed_session(
        &self,
        chat_id: ChatId,
        model: Option<ModelConfig>,
        mode: Option<ExecutionMode>,
        effort: Option<Option<ReasoningEffort>>,
    ) -> Result<(), AgentError> {
        self.require_managed_configuration()?;
        if let Some(mode) = mode {
            validate_managed_mode(
                mode,
                self.inner
                    .host_options
                    .execution_mode_override
                    .unwrap_or(ExecutionMode::Auto),
            )?;
        }
        let selected = model
            .map(|config| {
                validate_model_config(&config)
                    .map_err(|error| AgentError::Invalid(error.to_string()))?;
                let client = self.inner.models.client(&config)?;
                Ok::<_, AgentError>((config, client))
            })
            .transpose()?;

        // Match the settings APIs' lock order, without reading or writing settings. Holding the
        // Chat lock through validation, durable metadata and application prevents a submission
        // from starting between the idle check and the two-field change.
        let _settings_gate = self.inner.settings_writes.lock().await;
        let mut chats = self.inner.chats.lock().await;
        self.require_managed_configuration()?;
        let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
        require_managed_root(chat)?;
        if !managed_chat_idle(chat).await? {
            return Err(AgentError::Busy);
        }

        let next_model = selected
            .as_ref()
            .map_or(&chat.snapshot.model, |(config, _)| config);
        let next_mode = mode.unwrap_or(chat.snapshot.policy.mode);
        if let Some(effort) = effort {
            validate_reasoning_effort(next_model, effort)
                .map_err(|error| AgentError::Invalid(error.to_string()))?;
        }
        // ACP metadata is recorded by the persistence adapter before the infallible in-memory
        // application. Its helper must not acquire the Chat lock or flush Chat snapshots.
        if let Some(recorder) = &self.inner.recorder {
            if let Some(mode) = mode
                && let Some(metadata) = recorder.acp_session(chat_id).await?
            {
                validate_managed_mode(mode, metadata.startup_permission_ceiling.mode)?;
            }
            recorder
                .record_acp_configuration(chat_id, next_model, next_mode)
                .await?;
        }
        if let Some((config, client)) = selected {
            if !tickloop_harness_model::compatible_private_history(&chat.snapshot.model, &config) {
                chat.snapshot.context_window_tokens = None;
            }
            if validate_reasoning_effort(&config, chat.snapshot.reasoning_effort).is_err() {
                chat.snapshot.reasoning_effort = None;
            }
            chat.snapshot.model = config;
            chat.client = client;
            chat.snapshot.pending_model = None;
            chat.pending_client = None;
            chat.pending_model_thinking = None;
        }
        if let Some(effort) = effort {
            chat.snapshot.reasoning_effort = effort;
        }
        if let Some(mode) = mode
            && chat.snapshot.policy.mode != mode
        {
            chat.change_execution_mode(mode);
            chat.snapshot.policy.revision += 1;
        }
        self.save(&chat.snapshot);
        Ok(())
    }

    fn require_managed_configuration(&self) -> Result<(), AgentError> {
        if !self.inner.tool_policy.is_managed() {
            return Err(AgentError::Invalid(
                "managed configuration is unavailable to ordinary Chats".into(),
            ));
        }
        if self.inner.closed.load(Ordering::SeqCst)
            || self.inner.upgrade_paused.load(Ordering::SeqCst)
            || self.inner.shutdown.is_cancelled()
            || self.inner.handover.is_cancelled()
        {
            return Err(AgentError::Busy);
        }
        Ok(())
    }
}

fn require_managed_root(chat: &Chat) -> Result<(), AgentError> {
    if chat.snapshot.delegation.is_some() {
        return Err(AgentError::Invalid(
            "managed configuration requires a root Chat".into(),
        ));
    }
    Ok(())
}

/// Full Access is selectable only below a Full Access ceiling, which the host granted at launch.
fn validate_managed_mode(mode: ExecutionMode, ceiling: ExecutionMode) -> Result<(), AgentError> {
    if mode == ExecutionMode::Plan {
        return Err(AgentError::Invalid(
            "managed sessions support only Ask, Auto or Full Access mode".into(),
        ));
    }
    if ceiling == ExecutionMode::Plan
        || (ceiling == ExecutionMode::AcceptEdits && mode != ExecutionMode::AcceptEdits)
        || (mode == ExecutionMode::FullAccess && ceiling != ExecutionMode::FullAccess)
    {
        return Err(AgentError::Invalid(
            "execution mode exceeds the managed startup ceiling".into(),
        ));
    }
    Ok(())
}

async fn managed_chat_idle(chat: &Chat) -> Result<bool, AgentError> {
    let snapshot = &chat.snapshot;
    if chat.active.is_some()
        || chat.active_run.is_some()
        || chat.upgrade_wait.is_some()
        || !chat.parked.is_empty()
        || !chat.ready_runs.is_empty()
        || !chat.releases.is_empty()
        || chat.branch_reservation.is_some()
        || chat.family_stopping
        || !chat.family_finishing.is_empty()
        || chat.task_checkpoint_pending
        || chat.pending_client.is_some()
        || chat.pending_model_thinking.is_some()
        || snapshot.pending_model.is_some()
        || !snapshot.pending_inputs.is_empty()
        || !snapshot.pending_commands.is_empty()
        || !snapshot.permissions.is_empty()
        || !snapshot.questions.is_empty()
        || !chat.bookkeeping.lifecycle.open_entries().is_empty()
        || matches!(
            snapshot.chat_loop.state,
            LoopState::Queued
                | LoopState::Running
                | LoopState::WaitingPermission
                | LoopState::WaitingQuestion
                | LoopState::WaitingPlan
        )
        // An outcome-unknown record is history, not activity; only an unsettled Run's operation
        // can still be in flight.
        || snapshot.operations.iter().any(|operation| {
            matches!(
                operation.state,
                OperationState::Proposed | OperationState::Admitted
            ) && chat.unsettled_runs.contains(&operation.run_id)
        })
        || snapshot
            .commands
            .iter()
            .any(|command| command_running(&command.state))
        || snapshot.user_shells.iter().any(|shell| {
            !matches!(
                shell.state,
                UserShellState::Completed | UserShellState::Failed
            )
        })
    {
        return Ok(false);
    }
    // Watchers project command updates asynchronously. Consult the source as well, while the
    // Chat lock excludes new model and user-Shell admissions.
    Ok(!chat
        .tools
        .commands()
        .await
        .iter()
        .any(|command| command_running(&command.state))
        && !chat
            .tools
            .user_commands()
            .await
            .iter()
            .any(|command| command_running(&command.state)))
}

fn command_running(state: &CommandState) -> bool {
    matches!(
        state,
        CommandState::Running | CommandState::Background | CommandState::Cancelling
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tickloop_harness_protocol::AgentHostOptions;
    use tickloop_harness_test_support::{ScriptedModel, config, harness::ChosenModels};
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::{HarnessOptions, HarnessSeams, ToolPolicy, WaitHosting};

    fn managed_agent(ceiling: ExecutionMode) -> Agent {
        Agent::with_seams(
            HarnessSeams {
                search: None,
                transcripts: None,
                environment: Some(Arc::new(crate::environment::FolderEnvironment::bare())),
                models: ChosenModels::new(Arc::new(ScriptedModel::new(vec![]))),
            },
            HarnessOptions {
                model: config(),
                settings: None,
                search: Default::default(),
                host: AgentHostOptions {
                    execution_mode_override: Some(ceiling),
                    ..AgentHostOptions::embedded_defaults()
                },
                tools: ToolPolicy::acp(),
                host_rules: Vec::new(),
                task_tools: None,
                waits: WaitHosting::Process,
                citation_style: Default::default(),
                system_prompt_addition: None,
                request_gates: Vec::new(),
                ..HarnessOptions::local(config())
            },
        )
        .unwrap()
    }

    #[tokio::test]
    async fn managed_configuration_does_not_select_a_process_default() {
        let agent = managed_agent(ExecutionMode::Auto);
        let directory = tempfile::tempdir().unwrap();
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        let mut selected = config();
        selected.model = "session-only".into();
        agent
            .configure_managed_session(chat, Some(selected), Some(ExecutionMode::AcceptEdits), None)
            .await
            .unwrap();
        let snapshot = agent.snapshot(chat).await.unwrap();
        assert_eq!(snapshot.model.model, "session-only");
        assert_eq!(snapshot.policy.mode, ExecutionMode::AcceptEdits);
        assert!(agent.managed_session_idle(chat).await.unwrap());
        let another = agent.create_chat(directory.path().into()).await.unwrap();
        let another = agent.snapshot(another).await.unwrap();
        assert_eq!(another.model.model, config().model);
        assert_eq!(another.policy.mode, ExecutionMode::Auto);
        agent.shutdown().await;
    }

    #[tokio::test]
    async fn busy_configuration_changes_neither_field() {
        let agent = managed_agent(ExecutionMode::Auto);
        let directory = tempfile::tempdir().unwrap();
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        agent
            .inner
            .chats
            .lock()
            .await
            .get_mut(&chat)
            .unwrap()
            .active = Some(CancellationToken::new());
        let mut selected = config();
        selected.model = "not-applied".into();
        assert!(!agent.managed_session_idle(chat).await.unwrap());
        assert!(matches!(
            agent
                .configure_managed_session(
                    chat,
                    Some(selected),
                    Some(ExecutionMode::AcceptEdits),
                    None
                )
                .await,
            Err(AgentError::Busy)
        ));
        let snapshot = agent.snapshot(chat).await.unwrap();
        assert_eq!(snapshot.model.model, config().model);
        assert_eq!(snapshot.policy.mode, ExecutionMode::Auto);
        agent
            .inner
            .chats
            .lock()
            .await
            .get_mut(&chat)
            .unwrap()
            .active = None;
        agent.shutdown().await;
    }

    #[tokio::test]
    async fn rejected_mode_does_not_apply_the_model() {
        let agent = managed_agent(ExecutionMode::AcceptEdits);
        let directory = tempfile::tempdir().unwrap();
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        let mut selected = config();
        selected.model = "not-applied".into();
        assert!(
            agent
                .configure_managed_session(chat, Some(selected), Some(ExecutionMode::Auto), None)
                .await
                .is_err()
        );
        let snapshot = agent.snapshot(chat).await.unwrap();
        assert_eq!(snapshot.model.model, config().model);
        assert_eq!(snapshot.policy.mode, ExecutionMode::AcceptEdits);
        agent.shutdown().await;
    }

    #[tokio::test]
    async fn unsupported_effort_changes_nothing() {
        let agent = managed_agent(ExecutionMode::Auto);
        let directory = tempfile::tempdir().unwrap();
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        let mut selected = config();
        selected.model = "not-applied".into();
        assert!(matches!(
            agent
                .configure_managed_session(
                    chat,
                    Some(selected),
                    Some(ExecutionMode::AcceptEdits),
                    Some(Some(ReasoningEffort::Max)),
                )
                .await,
            Err(AgentError::Invalid(_))
        ));
        let snapshot = agent.snapshot(chat).await.unwrap();
        assert_eq!(snapshot.model.model, config().model);
        assert_eq!(snapshot.policy.mode, ExecutionMode::Auto);
        agent
            .configure_managed_session(chat, None, None, Some(None))
            .await
            .unwrap();
        assert_eq!(agent.snapshot(chat).await.unwrap().reasoning_effort, None);
        agent.shutdown().await;
    }

    #[test]
    fn managed_modes_cannot_exceed_startup_ceiling() {
        for ceiling in [
            ExecutionMode::AcceptEdits,
            ExecutionMode::Auto,
            ExecutionMode::FullAccess,
        ] {
            assert!(validate_managed_mode(ExecutionMode::AcceptEdits, ceiling).is_ok());
            assert_eq!(
                validate_managed_mode(ExecutionMode::FullAccess, ceiling).is_ok(),
                ceiling == ExecutionMode::FullAccess
            );
            assert!(validate_managed_mode(ExecutionMode::Plan, ceiling).is_err());
        }
        assert!(validate_managed_mode(ExecutionMode::FullAccess, ExecutionMode::Plan).is_err());
        assert!(validate_managed_mode(ExecutionMode::Auto, ExecutionMode::AcceptEdits).is_err());
        assert!(validate_managed_mode(ExecutionMode::Auto, ExecutionMode::Auto).is_ok());
        assert!(validate_managed_mode(ExecutionMode::Auto, ExecutionMode::FullAccess).is_ok());
        assert!(validate_managed_mode(ExecutionMode::AcceptEdits, ExecutionMode::Plan).is_err());
    }

    #[test]
    fn only_running_commands_keep_a_managed_chat_busy() {
        for state in [
            CommandState::Running,
            CommandState::Background,
            CommandState::Cancelling,
        ] {
            assert!(command_running(&state));
        }
        // An unknown outcome is history the settlement already reported, not activity.
        for state in [
            CommandState::Completed,
            CommandState::Failed,
            CommandState::Unknown,
        ] {
            assert!(!command_running(&state));
        }
    }
}

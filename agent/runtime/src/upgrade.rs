//! Planned process replacement checkpoints. These preserve live authorization and execution
//! intent and must only travel over the daemon's authenticated, private handoff channel.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::*;

const CHECKPOINT_VERSION: u32 = 1;

#[derive(Clone, Serialize, Deserialize)]
pub struct AgentUpgradeCheckpoint {
    version: u32,
    transfer_id: Uuid,
    selected_model: ModelConfig,
    chats: Vec<ChatCheckpoint>,
    session_permissions: session_permissions::SessionPermissionContext,
    family_mailbox: Value,
    subagents: HashMap<ChatId, Vec<SubagentRegistration>>,
    #[serde(with = "map_entries")]
    invocations: HashMap<(RunId, String), delegation::SubagentInvocation>,
    host_contexts: host_context::HostContexts,
    tick_deliveries: Value,
    trusted_directories: HashSet<PathBuf>,
    shell_descriptions: HashMap<Uuid, Option<String>>,
    task_tools_enabled: bool,
    host_tools: Option<crate::host_tools::HostToolUpgradeCheckpoint>,
}

impl AgentUpgradeCheckpoint {
    pub fn transfer_id(&self) -> Uuid {
        self.transfer_id
    }

    pub fn chat_roots(&self) -> impl Iterator<Item = (ChatId, Option<ChatId>)> + '_ {
        self.chats.iter().map(|chat| {
            (
                chat.snapshot.id,
                chat.snapshot
                    .delegation
                    .as_ref()
                    .map(|delegation| delegation.root_chat_id()),
            )
        })
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct ChatCheckpoint {
    #[serde(with = "live_snapshot")]
    snapshot: ChatSnapshot,
    bookkeeping: ChatBookkeeping,
    family_generation: u64,
    start_new_tool_result_message: bool,
    needs_plan_mode_exit_notice: bool,
    deferred_messages: Vec<Message>,
    feedback_inputs: HashMap<RunId, feedback_inbox::FeedbackIdentity>,
    output_style: OutputStyle,
    auto_denials: auto_mode::DenialTracking,
    denial_window: denials::DenialWindow,
    content_replacements: tool_results::ContentReplacementState,
    file_history: Option<Value>,
    shells: Value,
    waiting: Option<Receipt>,
    file_reads: Value,
    /// When the goal's running time was last settled, so the successor goes on from there.
    #[serde(default)]
    goal_clock_ms: u64,
    #[serde(default)]
    goal_recorded_tokens: u64,
}

impl Agent {
    /// Freezes a complete Worker family at a boundary with no in-flight execution. A busy
    /// family stays live and returns `None`; no user operation is cancelled to make it idle.
    pub async fn prepare_upgrade(&self) -> Result<Option<AgentUpgradeCheckpoint>, AgentError> {
        if self.inner.closed.load(Ordering::SeqCst)
            || self.inner.upgrade_paused.load(Ordering::SeqCst)
        {
            return Ok(None);
        }
        let Ok(guard) = self.inner.upgrade_gate.clone().try_write_owned() else {
            return Ok(None);
        };
        if !self.inner.title_requests.lock().await.is_empty() {
            return Ok(None);
        }
        let host_tools = {
            let provider = self.inner.host_tools.read().unwrap();
            match provider.as_ref() {
                Some(provider) => {
                    match provider.upgrade_checkpoint().map_err(AgentError::Invalid)? {
                        Some(state) => Some(crate::host_tools::HostToolUpgradeCheckpoint { state }),
                        None => return Ok(None),
                    }
                }
                None => None,
            }
        };
        let chats = self.inner.chats.lock().await;
        if self.inner.closed.load(Ordering::SeqCst)
            || self.inner.handover.is_cancelled()
            || chats.values().any(|chat| {
                (chat.active.is_some() && chat.upgrade_wait.is_none())
                    || !chat.releases.is_empty()
                    || chat.branch_reservation.is_some()
                    || chat
                        .active
                        .as_ref()
                        .is_some_and(CancellationToken::is_cancelled)
                    || !chat.user_cancelled_runs.is_empty()
                    || !chat.parked.is_empty()
                    || !chat.ready_runs.is_empty()
                    || chat.family_stopping
                    || !chat.family_finishing.is_empty()
                    || chat.task_checkpoint_pending
                    || chat.has_foreground_user_shell()
                    || !chat.snapshot.permissions.is_empty()
                    || !chat.snapshot.questions.is_empty()
                    || chat.pending_client.is_some()
                    || chat.pending_model_thinking.is_some()
                    || chat.snapshot.request_usage.entries().iter().any(|request| {
                        request.outcome == tickloop_agent_protocol::RequestOutcome::InFlight
                    })
            })
        {
            return Ok(None);
        }
        let mut saved = Vec::with_capacity(chats.len());
        for chat in chats.values() {
            let Some(shells) = chat
                .tools
                .checkpoint_shells()
                .await
                .map_err(|error| AgentError::Invalid(error.to_string()))?
            else {
                return Ok(None);
            };
            self.checkpoint_transcript(&chat.snapshot, &chat.bookkeeping)
                .await
                .map_err(AgentError::Storage)?;
            saved.push(ChatCheckpoint {
                snapshot: chat.snapshot.clone(),
                bookkeeping: chat.bookkeeping.clone(),
                family_generation: chat.family_generation,
                start_new_tool_result_message: chat.start_new_tool_result_message,
                needs_plan_mode_exit_notice: chat.needs_plan_mode_exit_notice,
                goal_clock_ms: chat.goal_clock_ms,
                goal_recorded_tokens: chat.goal_recorded_tokens,
                deferred_messages: chat.deferred_messages.clone(),
                feedback_inputs: chat.feedback_inputs.clone(),
                output_style: chat.output_style,
                auto_denials: chat.auto_denials.clone(),
                denial_window: chat.denial_window.clone(),
                content_replacements: chat.content_replacements.clone(),
                file_history: chat
                    .file_history
                    .as_ref()
                    .map(FileHistory::upgrade_checkpoint),
                shells,
                waiting: chat.upgrade_wait.clone(),
                file_reads: live_snapshot::checkpoint_reads(
                    &chat.tools.file_reads().upgrade_checkpoint(),
                ),
            });
        }
        let host_contexts = self.inner.host_contexts.lock().unwrap().clone();
        let tick_deliveries = self.tick_deliveries().upgrade_checkpoint();
        let trusted_directories = self
            .inner
            .trusted_working_directories
            .lock()
            .unwrap()
            .clone();
        let selected_model = self.inner.selected_model.lock().unwrap().config.clone();
        let checkpoint = AgentUpgradeCheckpoint {
            version: CHECKPOINT_VERSION,
            transfer_id: tickloop_ids::generate(),
            selected_model,
            chats: saved,
            session_permissions: self.inner.session_permissions.lock().await.clone(),
            family_mailbox: serde_json::to_value(&*self.inner.family_mailbox.lock().await)
                .map_err(|error| AgentError::Storage(error.to_string()))?,
            subagents: self.inner.subagent_registry.lock().await.clone(),
            invocations: self.inner.subagent_invocations.lock().await.clone(),
            host_contexts,
            tick_deliveries,
            trusted_directories,
            shell_descriptions: self.inner.shell_descriptions.lock().await.clone(),
            task_tools_enabled: self.inner.task_tools_enabled.load(Ordering::SeqCst),
            host_tools,
        };
        if self.inner.closed.load(Ordering::SeqCst) {
            return Ok(None);
        }
        self.inner.upgrade_paused.store(true, Ordering::SeqCst);
        *self.inner.upgrade_guard.lock().await = Some(guard);
        Ok(Some(checkpoint))
    }

    /// Cancels a prepared handoff before execution ownership or Shell ownership moved.
    pub async fn cancel_upgrade(&self) {
        if self.inner.upgrade_restore.lock().await.is_some()
            && !self.inner.closed.load(Ordering::SeqCst)
        {
            return;
        }
        let was_paused = self.inner.upgrade_paused.swap(false, Ordering::SeqCst);
        self.inner.upgrade_guard.lock().await.take();
        self.inner.ledger_changes.send_replace(());
        self.inner.tick_wake.notify_one();
        if was_paused && !self.inner.closed.load(Ordering::SeqCst) {
            let chats = self
                .inner
                .chats
                .lock()
                .await
                .keys()
                .copied()
                .collect::<Vec<_>>();
            for chat in chats {
                let _ = self.start_user_shell_queue(chat).await;
                if let Ok(Some(next)) = self.next_run(chat).await {
                    self.run_task(next);
                }
            }
        }
    }

    /// Builds the successor in a paused state. It neither adopts Shells nor writes recovery
    /// changes to Transcripts, and never applies ordinary historical Continue rules.
    pub async fn restore_upgrade(
        &self,
        checkpoint: AgentUpgradeCheckpoint,
    ) -> Result<(), AgentError> {
        if checkpoint.version != CHECKPOINT_VERSION {
            return Err(AgentError::Invalid(
                "unsupported Worker upgrade checkpoint version".into(),
            ));
        }
        let guard = self.inner.upgrade_gate.clone().write_owned().await;
        if !self.inner.chats.lock().await.is_empty() {
            return Err(AgentError::Invalid(
                "Worker upgrade restore requires a fresh runtime".into(),
            ));
        }
        self.inner.upgrade_paused.store(true, Ordering::SeqCst);
        *self.inner.upgrade_guard.lock().await = Some(guard);
        {
            let provider = self.inner.host_tools.read().unwrap();
            match (provider.as_ref(), checkpoint.host_tools.clone()) {
                (Some(provider), Some(checkpoint)) => provider
                    .restore_upgrade(checkpoint.state)
                    .map_err(AgentError::Invalid)?,
                (None, None) => {}
                _ => {
                    return Err(AgentError::Invalid(
                        "host tool providers changed during Worker upgrade".into(),
                    ));
                }
            }
        }
        {
            let mut selected = self.inner.selected_model.lock().unwrap();
            if selected.config != checkpoint.selected_model {
                let client = self
                    .inner
                    .models
                    .client(&checkpoint.selected_model)
                    .map_err(|error| AgentError::Invalid(error.to_string()))?;
                *selected = chat_settings::SelectedModel {
                    config: checkpoint.selected_model.clone(),
                    client,
                };
            }
        }
        *self.inner.trusted_working_directories.lock().unwrap() =
            checkpoint.trusted_directories.clone();
        *self.inner.session_permissions.lock().await = checkpoint.session_permissions.clone();
        *self.inner.family_mailbox.lock().await =
            serde_json::from_value(checkpoint.family_mailbox.clone())
                .map_err(|error| AgentError::Invalid(error.to_string()))?;
        *self.inner.subagent_registry.lock().await = checkpoint.subagents.clone();
        *self.inner.subagent_invocations.lock().await = checkpoint.invocations.clone();
        *self.inner.host_contexts.lock().unwrap() = checkpoint.host_contexts.clone();
        *self.inner.shell_descriptions.lock().await = checkpoint.shell_descriptions.clone();
        self.inner
            .task_tools_enabled
            .store(checkpoint.task_tools_enabled, Ordering::SeqCst);
        for saved in &checkpoint.chats {
            let snapshot = saved.snapshot.clone();
            if let Some(folder) = &snapshot.folder {
                self.require_working_directory_trust(folder)?;
            }
            let client = {
                let selected = self.inner.selected_model.lock().unwrap();
                if selected.config == snapshot.model {
                    selected.client.clone()
                } else {
                    self.inner
                        .models
                        .client(&snapshot.model)
                        .map_err(|error| AgentError::Invalid(error.to_string()))?
                }
            };
            self.adopt_upgrade_transcript(&snapshot, &saved.bookkeeping)
                .await?;
            let tools = self.create_chat_tools(&snapshot);
            for (path, read) in live_snapshot::restore_reads(saved.file_reads.clone())
                .map_err(AgentError::Invalid)?
            {
                tools.file_reads().record(path, read);
            }
            let skills = self.discover_skills(snapshot.folder.as_deref());
            let file_history = match (&snapshot.folder, saved.file_history.clone()) {
                (Some(folder), Some(saved)) => Some(
                    FileHistory::from_upgrade(
                        self.file_history_directory(snapshot.id, folder),
                        saved,
                    )
                    .map_err(AgentError::Invalid)?,
                ),
                (None, None) => None,
                _ => {
                    return Err(AgentError::Invalid(
                        "a Chat's Folder changed during Worker upgrade".into(),
                    ));
                }
            };
            self.inner.chats.lock().await.insert(
                snapshot.id,
                Chat {
                    ids: tickloop_ids::IdContext::new(),
                    snapshot,
                    bookkeeping: saved.bookkeeping.clone(),
                    family_generation: saved.family_generation,
                    family_stopping: false,
                    task_checkpoint_pending: false,
                    family_finishing: HashSet::new(),
                    start_new_tool_result_message: saved.start_new_tool_result_message,
                    needs_plan_mode_exit_notice: saved.needs_plan_mode_exit_notice,
                    active: saved
                        .waiting
                        .as_ref()
                        .map(|_| self.inner.shutdown.child_token()),
                    active_run: saved.waiting.as_ref().map(|receipt| receipt.run_id),
                    upgrade_wait: saved.waiting.clone(),
                    parked: HashMap::new(),
                    releases: HashMap::new(),
                    ready_runs: VecDeque::new(),
                    client,
                    pending_client: None,
                    pending_model_thinking: None,
                    tools,
                    skills,
                    content_replacements: saved.content_replacements.clone(),
                    file_history,
                    watching_commands: false,
                    watching_user_shells: false,
                    user_shell_queue: Arc::new(Notify::new()),
                    feedback_watcher: None,
                    feedback_inputs: saved.feedback_inputs.clone(),
                    feedback_cleanup_started: false,
                    user_loops: HashMap::new(),
                    deferred_messages: saved.deferred_messages.clone(),
                    user_cancelled_runs: HashSet::new(),
                    output_style: saved.output_style,
                    auto_denials: saved.auto_denials.clone(),
                    denial_window: saved.denial_window.clone(),
                    branch_reservation: None,
                    goal_recorded_tokens: saved.goal_recorded_tokens,
                    loaded_from_history: false,
                    goal_clock_ms: saved.goal_clock_ms,
                },
            );
        }
        self.tick_deliveries()
            .restore_upgrade(checkpoint.tick_deliveries.clone())
            .map_err(AgentError::Invalid)?;
        *self.inner.upgrade_restore.lock().await = Some(checkpoint);
        Ok(())
    }

    /// Called only after the daemon commits the successor's execution authority.
    pub async fn activate_upgrade(&self) -> Result<(), AgentError> {
        if self.inner.upgrade_adopted.load(Ordering::SeqCst) {
            return Ok(());
        }
        let checkpoint = self
            .inner
            .upgrade_restore
            .lock()
            .await
            .clone()
            .ok_or_else(|| AgentError::Invalid("Worker has no prepared upgrade restore".into()))?;
        for saved in &checkpoint.chats {
            let tools = self.inner.chats.lock().await[&saved.snapshot.id]
                .tools
                .clone();
            tools
                .restore_shells(saved.shells.clone())
                .await
                .map_err(|error| AgentError::Invalid(error.to_string()))?;
        }
        self.inner.upgrade_adopted.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Releases queued work only after the daemon durably records activation. Recovery must
    /// never replay this checkpoint once this boundary has been passed.
    pub async fn release_upgrade(&self) -> Result<(), AgentError> {
        if !self.inner.upgrade_adopted.load(Ordering::SeqCst) {
            return Err(AgentError::Invalid(
                "Worker must adopt ownership before release".into(),
            ));
        }
        let checkpoint = self
            .inner
            .upgrade_restore
            .lock()
            .await
            .take()
            .ok_or_else(|| AgentError::Invalid("Worker has no adopted upgrade".into()))?;
        if let Some(shells) = self.shell_runtime() {
            shells
                .commit_handoffs()
                .await
                .map_err(AgentError::Invalid)?;
        }
        self.inner.upgrade_paused.store(false, Ordering::SeqCst);
        self.inner.upgrade_guard.lock().await.take();
        for saved in checkpoint.chats {
            let id = saved.snapshot.id;
            let tools = {
                let mut chats = self.inner.chats.lock().await;
                let chat = chats.get_mut(&id).unwrap();
                chat.watching_commands = true;
                chat.watching_user_shells = true;
                chat.tools.clone()
            };
            self.watch_commands(id, tools.clone()).await;
            self.watch_user_shells(id, tools.clone()).await;
            for command in tools.commands().await {
                self.record_command(id, command).await?;
            }
            for command in tools.user_commands().await {
                self.record_user_shell(id, command).await?;
            }
            self.start_event_source_watcher(id, saved.snapshot.chat_loop.id)
                .await;
            self.start_feedback_cleanup(id).await;
            self.start_user_shell_queue(id).await?;
            if let Some(receipt) = saved.waiting {
                let cancel = self.inner.chats.lock().await[&id].active.clone().unwrap();
                let agent = self.clone();
                self.inner.tasks.spawn(async move {
                    let result = agent.after_complete_response(&receipt, &cancel).await;
                    let mut next = match result {
                        Ok(Some(request)) => Some((receipt, request, cancel)),
                        outcome => {
                            let result = match outcome {
                                Err(error) => Err(error),
                                Ok(None) => agent.inner.chats.lock().await[&receipt.chat_id]
                                    .snapshot
                                    .request_usage
                                    .run_conversation_usage(receipt.run_id)
                                    .map_err(|error| error.to_string()),
                                Ok(Some(_)) => unreachable!(),
                            };
                            agent
                                .finish_run(receipt, cancel, result, false, false)
                                .await
                        }
                    };
                    while let Some((receipt, request, cancel)) = next {
                        next = agent.run(receipt, request, cancel).await;
                    }
                });
            } else if let Some(next) = self.next_run(id).await? {
                self.run_task(next);
            } else {
                // The predecessor may have stopped just before continuing the goal.
                self.reconsider_goal(id).await;
            }
        }
        self.tick_deliveries().activate_upgrade();
        self.inner.tick_wake.notify_one();
        // The Worker keeps time for the Chats it took over, idle ones included.
        self.rearm_scheduled_timer();
        Ok(())
    }

    /// Relinquishes an already transferred Worker. Explicit user stop still uses `shutdown`.
    pub async fn shutdown_for_upgrade(&self) {
        self.inner.closed.store(true, Ordering::SeqCst);
        self.inner.chat_leases.close();
        self.inner.shutdown.cancel();
        self.inner.tasks.abort_all();
        self.inner.tasks.close();
        self.inner.tasks.wait().await;
        self.inner.chat_leases.release_all();
        self.close_transcripts().await;
        self.inner.upgrade_guard.lock().await.take();
    }

    pub(super) async fn after_complete_response(
        &self,
        receipt: &Receipt,
        cancel: &CancellationToken,
    ) -> Result<Option<ModelRequest>, String> {
        self.inner
            .chats
            .lock()
            .await
            .get_mut(&receipt.chat_id)
            .ok_or("Agent is unavailable")?
            .upgrade_wait = Some(receipt.clone());
        let waited = async {
            Ok::<_, String>(
                self.wait_for_subagents(receipt, cancel, None).await?
                    || self.wait_for_task_commands(receipt, cancel).await?,
            )
        }
        .await;
        let _upgrade = self.inner.upgrade_gate.read().await;
        self.inner
            .chats
            .lock()
            .await
            .get_mut(&receipt.chat_id)
            .ok_or("Agent is unavailable")?
            .upgrade_wait = None;
        if waited? {
            self.merge_queued_messages(receipt)
                .await
                .map_err(|error| error.to_string())?;
            let chats = self.inner.chats.lock().await;
            return self
                .continuation_request(&chats[&receipt.chat_id])
                .map(Some);
        }
        let mut chats = self.inner.chats.lock().await;
        let chat = chats
            .get_mut(&receipt.chat_id)
            .ok_or("Agent is unavailable")?;
        if handback::remind(chat, receipt) {
            self.save(&chat.snapshot);
            return self.continuation_request(chat).map(Some);
        }
        Ok(None)
    }
}

pub(super) mod map_entries {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<'a, K: Serialize + 'a, V: Serialize + 'a, M, S: Serializer>(
        map: &'a M,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        &'a M: IntoIterator<Item = (&'a K, &'a V)>,
    {
        map.into_iter().collect::<Vec<_>>().serialize(serializer)
    }

    pub fn deserialize<
        'de,
        K: Deserialize<'de>,
        V: Deserialize<'de>,
        M: FromIterator<(K, V)>,
        D: Deserializer<'de>,
    >(
        deserializer: D,
    ) -> Result<M, D::Error> {
        Ok(Vec::<(K, V)>::deserialize(deserializer)?
            .into_iter()
            .collect())
    }
}

pub(super) mod instant {
    use serde::{Deserialize, Deserializer, Serializer};
    use tokio::time::{Duration, Instant};

    pub fn serialize<S: Serializer>(instant: &Instant, serializer: S) -> Result<S::Ok, S::Error> {
        let now = Instant::now();
        let wall = tickloop_agent_base::private_fs::unix_time_milliseconds() as i64;
        let at = if *instant >= now {
            wall.saturating_add(instant.duration_since(now).as_millis() as i64)
        } else {
            wall.saturating_sub(now.duration_since(*instant).as_millis() as i64)
        };
        serializer.serialize_i64(at)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Instant, D::Error> {
        let at = i64::deserialize(deserializer)?;
        let wall = tickloop_agent_base::private_fs::unix_time_milliseconds() as i64;
        let now = Instant::now();
        let restored = if at >= wall {
            now.checked_add(Duration::from_millis(at.saturating_sub(wall) as u64))
        } else {
            now.checked_sub(Duration::from_millis(wall.saturating_sub(at) as u64))
        };
        restored.ok_or_else(|| {
            serde::de::Error::custom("upgrade deadline is outside the supported clock range")
        })
    }
}

pub(super) mod optional_instant {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use tokio::time::Instant;

    #[derive(Serialize, Deserialize)]
    struct Deadline(#[serde(with = "super::instant")] Instant);

    pub fn serialize<S: Serializer>(
        value: &Option<Instant>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        value.map(Deadline).serialize(serializer)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Instant>, D::Error> {
        Ok(Option::<Deadline>::deserialize(deserializer)?.map(|value| value.0))
    }
}

mod live_snapshot;

#[cfg(test)]
mod tests;

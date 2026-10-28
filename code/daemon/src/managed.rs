//! Invocation-scoped control. Secrets are never part of observations or command results.
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

use serde::{Deserialize, Serialize};
use tickloop_code_access::{
    AccessError, AccessErrorCode, AgentCommand, ExecutionProfile, ExecutionRef, LaunchSpec,
    MANAGED_LEASE_SECONDS, ManagedToken,
};
use tickloop_harness::protocol::{ExecutionMode, PermissionScope};
use tokio::sync::Mutex;
use uuid::Uuid;

#[derive(Default)]
pub(crate) struct ManagedRegistry {
    entries: Mutex<HashMap<ExecutionRef, Arc<ManagedEntry>>>,
}

pub(crate) struct ManagedEntry {
    pub launch: LaunchSpec,
    pub chat: Uuid,
    pub token: ManagedToken,
    pub gate: Arc<Mutex<()>>,
    history: Mutex<
        Option<(
            Uuid,
            tickloop_harness::runtime::transcript::DisplayHistorySnapshot,
        )>,
    >,
    state: Mutex<State>,
}
struct State {
    operation: Uuid,
    expires: Instant,
    released: bool,
    submitted: bool,
    settled: bool,
    cleanup_attempts: u8,
    cleanup_running: bool,
    cleanup_error: Option<String>,
    worker_settled: bool,
    retry_at: Instant,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct SavedManaged {
    execution: ExecutionRef,
    chat: Uuid,
    launch: LaunchSpec,
    token: ManagedToken,
    operation: Uuid,
    expires_at: SystemTime,
    released: bool,
    submitted: bool,
    settled: bool,
    cleanup_attempts: u8,
    cleanup_error: Option<String>,
    worker_settled: bool,
}

fn denied() -> AccessError {
    AccessError::new(
        AccessErrorCode::AccessDenied,
        "managed execution control proof is invalid or expired",
    )
}

pub(crate) fn validate_launch(launch: &LaunchSpec) -> Result<(), AccessError> {
    if launch.profile == ExecutionProfile::Local || launch.exact_chat.is_some() {
        return Err(AccessError::invalid(
            "managed execution requires a new Chat and a managed profile",
        ));
    }
    if launch.host_options.execution_mode_override == Some(ExecutionMode::Plan) {
        return Err(AccessError::invalid(
            "managed execution does not support Plan mode",
        ));
    }
    Ok(())
}

pub(crate) fn normalized_launch(mut launch: LaunchSpec) -> LaunchSpec {
    if let Ok(folder) = launch.folder.canonicalize() {
        launch.folder = folder;
    }
    if let Some(root) = &launch.config_root
        && let Ok(root) = root.canonicalize()
    {
        launch.config_root = Some(root);
    }
    use tickloop_harness::protocol::PromptPolicy;
    launch
        .host_options
        .execution_mode_override
        .get_or_insert(ExecutionMode::Auto);
    launch.host_options.prompt_policy = match launch.profile {
        ExecutionProfile::OneShot => PromptPolicy::DenyOnPrompt,
        _ => PromptPolicy::Interactive,
    };
    launch
}

impl ManagedRegistry {
    pub async fn install(
        &self,
        execution: ExecutionRef,
        chat: Uuid,
        launch: LaunchSpec,
        token: ManagedToken,
    ) {
        self.entries.lock().await.insert(
            execution,
            Arc::new(ManagedEntry {
                launch,
                chat,
                token,
                gate: Arc::new(Mutex::new(())),
                history: Mutex::new(None),
                state: Mutex::new(State {
                    operation: tickloop_harness::ids::generate(),
                    expires: Instant::now() + Duration::from_secs(MANAGED_LEASE_SECONDS),
                    released: false,
                    submitted: false,
                    settled: false,
                    cleanup_attempts: 0,
                    cleanup_running: false,
                    cleanup_error: None,
                    worker_settled: false,
                    retry_at: Instant::now(),
                }),
            }),
        );
    }
    pub async fn contains(&self, execution: ExecutionRef) -> bool {
        self.entries.lock().await.contains_key(&execution)
    }
    pub async fn entry(&self, execution: ExecutionRef) -> Option<Arc<ManagedEntry>> {
        self.entries.lock().await.get(&execution).cloned()
    }
    /// Whether `entry` is still the registered control of `execution`, rather than one a
    /// completed release or a replacing load already forgot.
    pub async fn is_current(&self, execution: ExecutionRef, entry: &Arc<ManagedEntry>) -> bool {
        self.entries
            .lock()
            .await
            .get(&execution)
            .is_some_and(|current| Arc::ptr_eq(current, entry))
    }
    /// Forgets `entry`, its proof and its launch, unless another entry replaced it meanwhile.
    pub async fn remove(&self, execution: ExecutionRef, entry: &Arc<ManagedEntry>) {
        let mut entries = self.entries.lock().await;
        if entries
            .get(&execution)
            .is_some_and(|current| Arc::ptr_eq(current, entry))
        {
            entries.remove(&execution);
        }
    }
    pub async fn reacquirable(&self, execution: ExecutionRef) -> bool {
        let Some(entry) = self.entries.lock().await.get(&execution).cloned() else {
            return false;
        };
        entry.launch.profile == ExecutionProfile::Acp && entry.released_cleanly().await
    }
    pub async fn warning(&self, execution: ExecutionRef) -> Option<String> {
        let entry = self.entries.lock().await.get(&execution).cloned()?;
        let state = entry.state.lock().await;
        state.cleanup_error.as_ref().map(|error| {
            format!(
                "Managed execution cleanup failed (attempt {}/3): {error}",
                state.cleanup_attempts
            )
        })
    }
    pub async fn authorize(
        &self,
        command: &AgentCommand,
    ) -> Result<Option<Arc<ManagedEntry>>, AccessError> {
        self.authorize_inner(command, true).await
    }
    pub async fn authorize_worker(
        &self,
        command: &AgentCommand,
    ) -> Result<Option<Arc<ManagedEntry>>, AccessError> {
        self.authorize_inner(command, false).await
    }
    async fn authorize_inner(
        &self,
        command: &AgentCommand,
        deadline: bool,
    ) -> Result<Option<Arc<ManagedEntry>>, AccessError> {
        let Some(execution) = command.execution() else {
            return Ok(None);
        };
        let entry = self.entries.lock().await.get(&execution).cloned();
        let token = match command {
            AgentCommand::ManagedSubmit { token, .. }
            | AgentCommand::ManagedConfigure { token, .. }
            | AgentCommand::ManagedHeartbeat { token, .. }
            | AgentCommand::ManagedSettle { token, .. }
            | AgentCommand::ManagedRelease { token, .. }
            | AgentCommand::ManagedFinalizeRelease { token, .. }
            | AgentCommand::ManagedDecidePermission { token, .. } => Some(token),
            _ => None,
        };
        match (entry, token) {
            (Some(entry), Some(token)) => {
                let state = entry.state.lock().await;
                let worker_cleanup = !deadline
                    && matches!(
                        command,
                        AgentCommand::ManagedRelease { .. }
                            | AgentCommand::ManagedFinalizeRelease { .. }
                    );
                if !entry.token.matches(token)
                    || (state.released && !worker_cleanup)
                    || (deadline && state.expires <= Instant::now())
                {
                    return Err(denied());
                }
                if matches!(command, AgentCommand::ManagedConfigure { .. })
                    && entry.launch.profile != ExecutionProfile::Acp
                {
                    return Err(AccessError::new(
                        AccessErrorCode::AccessDenied,
                        "configuration requires an ACP session",
                    ));
                }
                if let AgentCommand::ManagedDecidePermission { scope, rule, .. } = command
                    && (entry.launch.profile != ExecutionProfile::Acp
                        || *scope != PermissionScope::Once
                        || rule.is_some())
                {
                    return Err(AccessError::new(
                        AccessErrorCode::AccessDenied,
                        "managed permissions permit only ACP once decisions",
                    ));
                }
                drop(state);
                Ok(Some(entry))
            }
            (Some(_), None) | (None, Some(_)) => Err(denied()),
            (None, None) => Ok(None),
        }
    }
    pub async fn checkpoint(&self) -> Vec<SavedManaged> {
        let entries = self.entries.lock().await;
        let mut saved = Vec::new();
        for (execution, entry) in entries.iter() {
            let state = entry.state.lock().await;
            saved.push(SavedManaged {
                execution: *execution,
                chat: entry.chat,
                launch: entry.launch.clone(),
                token: entry.token.clone(),
                operation: state.operation,
                expires_at: SystemTime::now()
                    + state.expires.saturating_duration_since(Instant::now()),
                released: state.released,
                submitted: state.submitted,
                settled: state.settled,
                cleanup_attempts: state.cleanup_attempts,
                cleanup_error: state.cleanup_error.clone(),
                worker_settled: state.worker_settled,
            });
        }
        saved
    }
    pub async fn restore(&self, saved: Vec<SavedManaged>) {
        let mut entries = self.entries.lock().await;
        for saved in saved {
            entries.insert(
                saved.execution,
                Arc::new(ManagedEntry {
                    launch: saved.launch,
                    chat: saved.chat,
                    token: saved.token,
                    gate: Arc::new(Mutex::new(())),
                    history: Mutex::new(None),
                    state: Mutex::new(State {
                        operation: saved.operation,
                        expires: Instant::now()
                            + saved
                                .expires_at
                                .duration_since(SystemTime::now())
                                .unwrap_or_default()
                                .min(Duration::from_secs(MANAGED_LEASE_SECONDS)),
                        released: saved.released,
                        submitted: saved.submitted,
                        settled: saved.settled,
                        cleanup_attempts: saved.cleanup_attempts,
                        cleanup_error: saved.cleanup_error,
                        worker_settled: saved.worker_settled,
                        cleanup_running: false,
                        retry_at: Instant::now(),
                    }),
                }),
            );
        }
    }
    /// Revocation precedes cleanup. Failed cleanup is retained and retried at most three times.
    pub async fn expired(&self) -> Vec<(ExecutionRef, Arc<ManagedEntry>)> {
        let entries = self.entries.lock().await;
        let mut expired = Vec::new();
        for (execution, entry) in entries.iter() {
            let mut state = entry.state.lock().await;
            let newly_expired = !state.released && state.expires <= Instant::now();
            let retry = state.released
                && state.cleanup_error.is_some()
                && state.cleanup_attempts < 3
                && !state.cleanup_running
                && state.retry_at <= Instant::now();
            if newly_expired || retry {
                // Round settlement leaves session MCP alive. A first revocation needs the
                // stronger Worker release even when the previous prompt already settled.
                if newly_expired {
                    state.worker_settled = false;
                }
                state.released = true;
                state.cleanup_running = true;
                state.cleanup_attempts += 1;
                expired.push((*execution, entry.clone()));
            }
        }
        expired
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryCursor {
    snapshot: Uuid,
    offset: usize,
}

impl ManagedEntry {
    pub async fn history(
        &self,
        cursor: Option<String>,
    ) -> Result<tickloop_code_access::ManagedHistoryPage, AccessError> {
        let previous = cursor
            .map(|cursor| {
                if cursor.len() > 4096 {
                    return Err(AccessError::invalid("history cursor too large"));
                }
                serde_json::from_str::<HistoryCursor>(&cursor)
                    .map_err(|_| AccessError::invalid("invalid history cursor"))
            })
            .transpose()?;
        let mut history = self.history.lock().await;
        if previous.is_none() {
            let root =
                self.launch.config_root.clone().ok_or_else(|| {
                    AccessError::invalid("ACP history requires a configuration root")
                })?;
            let snapshot =
                crate::records::managed_history_snapshot(&tickloop_code_access::TranscriptRef {
                    store_root: root,
                    chat_id: self.chat,
                })
                .await?;
            *history = Some((tickloop_harness::ids::generate(), snapshot));
        }
        let (id, snapshot) = history.as_ref().ok_or_else(|| {
            AccessError::invalid("history replay snapshot is no longer available")
        })?;
        let offset = if let Some(previous) = previous {
            if previous.snapshot != *id {
                return Err(AccessError::new(
                    AccessErrorCode::VersionConflict,
                    "history replay snapshot changed",
                ));
            }
            previous.offset
        } else {
            0
        };
        let page = snapshot
            .page(offset, 100)
            .map_err(|e| AccessError::invalid(e.to_string()))?;
        let next_cursor = page
            .next_offset
            .map(|offset| {
                serde_json::to_string(&HistoryCursor {
                    snapshot: *id,
                    offset,
                })
            })
            .transpose()
            .map_err(|e| AccessError::invalid(e.to_string()))?;
        Ok(tickloop_code_access::ManagedHistoryPage {
            session_id: self.chat,
            items: page
                .items
                .into_iter()
                .map(serde_json::to_value)
                .collect::<Result<_, _>>()
                .map_err(|e| AccessError::invalid(e.to_string()))?,
            next_cursor,
        })
    }

    pub async fn renew(&self) -> Result<(), AccessError> {
        let mut state = self.state.lock().await;
        if state.released || state.expires <= Instant::now() {
            return Err(denied());
        }
        state.expires = Instant::now() + Duration::from_secs(MANAGED_LEASE_SECONDS);
        Ok(())
    }
    pub async fn revoke(&self) {
        let mut state = self.state.lock().await;
        if !state.released {
            state.worker_settled = false;
        }
        state.released = true;
        state.cleanup_running = true;
        state.cleanup_attempts += 1;
    }
    pub async fn begin_submit(&self) -> Result<(), AccessError> {
        let mut state = self.state.lock().await;
        if self.launch.profile == ExecutionProfile::OneShot && state.submitted {
            return Err(AccessError::invalid(
                "one-shot execution accepts exactly one input",
            ));
        }
        if state.cleanup_error.is_some() {
            return Err(AccessError::new(
                AccessErrorCode::Busy,
                "prior managed cleanup has not succeeded",
            ));
        }
        if state.settled {
            state.operation = tickloop_harness::ids::generate();
        }
        state.submitted = true;
        state.settled = false;
        state.worker_settled = false;
        state.cleanup_attempts = 0;
        Ok(())
    }
    pub async fn settle(&self) {
        self.state.lock().await.settled = true;
    }
    pub async fn operation(&self) -> Uuid {
        self.state.lock().await.operation
    }
    pub async fn worker_settled(&self) -> bool {
        self.state.lock().await.worker_settled
    }
    /// Revoked, settled, released by its Worker and with no cleanup running or failed.
    pub async fn released_cleanly(&self) -> bool {
        let state = self.state.lock().await;
        state.released
            && state.settled
            && state.worker_settled
            && !state.cleanup_running
            && state.cleanup_error.is_none()
    }
    /// Records the hosted cleanup of a settled prompt. A round settlement never closes session
    /// MCP, so it is never a Worker release; and once a release or lease expiry revoked the
    /// proof, that cleanup owns this state and a late settlement must not overwrite it.
    pub async fn round_finished(&self, error: Option<String>) {
        let mut state = self.state.lock().await;
        if !state.released {
            state.cleanup_error = error;
        }
    }
    pub async fn cleanup_finished(&self, worker_settled: bool, error: Option<String>) {
        let mut state = self.state.lock().await;
        state.worker_settled |= worker_settled;
        state.cleanup_running = false;
        state.cleanup_error = error;
        state.retry_at = Instant::now() + Duration::from_secs(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tickloop_harness::protocol::{AgentHostOptions, PromptPolicy};

    async fn fixture() -> (ManagedRegistry, ExecutionRef, ManagedToken) {
        let registry = ManagedRegistry::default();
        let execution = ExecutionRef {
            daemon_epoch: tickloop_harness::ids::generate(),
            agent_id: tickloop_harness::ids::generate(),
            generation: 1,
        };
        let token = ManagedToken::new();
        let mut launch = LaunchSpec::embedded(std::env::temp_dir());
        launch.profile = ExecutionProfile::Acp;
        registry
            .install(
                execution,
                tickloop_harness::ids::generate(),
                launch,
                token.clone(),
            )
            .await;
        (registry, execution, token)
    }
    fn heartbeat(execution: ExecutionRef, token: &ManagedToken) -> AgentCommand {
        AgentCommand::ManagedHeartbeat {
            execution,
            token: token.clone(),
        }
    }

    #[tokio::test]
    async fn lease_expiry_revokes_proof_and_retains_bounded_retry_failure() {
        let (registry, execution, token) = fixture().await;
        let entry = registry
            .authorize(&heartbeat(execution, &token))
            .await
            .unwrap()
            .unwrap();
        let operation = entry.operation().await;
        entry.state.lock().await.expires = Instant::now() - Duration::from_secs(1);
        assert_eq!(
            registry
                .authorize(&heartbeat(execution, &token))
                .await
                .err()
                .unwrap()
                .code,
            AccessErrorCode::AccessDenied
        );
        for attempt in 1..=3 {
            assert_eq!(registry.expired().await.len(), 1);
            assert!(registry.expired().await.is_empty());
            entry
                .cleanup_finished(true, Some("host cleanup unavailable".into()))
                .await;
            assert_eq!(entry.operation().await, operation);
            assert!(
                registry
                    .warning(execution)
                    .await
                    .unwrap()
                    .contains(&format!("attempt {attempt}/3"))
            );
            entry.state.lock().await.retry_at = Instant::now();
        }
        assert!(registry.expired().await.is_empty());
        // A late renewal cannot revive a revoked proof.
        assert!(entry.renew().await.is_err());
        assert!(
            registry
                .authorize(&heartbeat(execution, &token))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn expiry_after_prompt_settlement_still_requires_worker_session_release() {
        let (registry, execution, token) = fixture().await;
        let entry = registry
            .authorize(&heartbeat(execution, &token))
            .await
            .unwrap()
            .unwrap();
        entry.round_finished(None).await;
        entry.settle().await;
        assert!(
            !entry.worker_settled().await,
            "round settlement did not close session MCP"
        );
        entry.state.lock().await.expires = Instant::now() - Duration::from_secs(1);
        assert_eq!(registry.expired().await.len(), 1);
        assert!(!entry.worker_settled().await);
        // A successful release is kept across retries of the remaining hosted cleanup.
        entry
            .cleanup_finished(true, Some("hosted cleanup pending".into()))
            .await;
        entry.state.lock().await.retry_at = Instant::now();
        assert_eq!(registry.expired().await.len(), 1);
        assert!(entry.worker_settled().await);
    }

    #[tokio::test]
    async fn settlement_finishing_after_revocation_leaves_release_cleanup_in_charge() {
        let (registry, execution, token) = fixture().await;
        let entry = registry
            .authorize(&heartbeat(execution, &token))
            .await
            .unwrap()
            .unwrap();
        // The lease expires while a prompt settlement holds the gate.
        entry.state.lock().await.expires = Instant::now() - Duration::from_secs(1);
        assert_eq!(registry.expired().await.len(), 1);
        entry.round_finished(None).await;
        entry.settle().await;
        assert!(!entry.worker_settled().await);
        assert!(!entry.released_cleanly().await);
        assert!(!registry.reacquirable(execution).await);
        // The expiry gate timed out first; a later settlement cannot clear its failure.
        entry
            .cleanup_finished(false, Some("managed expiry admission timed out".into()))
            .await;
        entry.round_finished(None).await;
        assert!(registry.warning(execution).await.is_some());
        assert!(!registry.reacquirable(execution).await);
        // Only the Worker release makes the entry releasable.
        entry.cleanup_finished(true, None).await;
        assert!(entry.released_cleanly().await);
        assert!(registry.reacquirable(execution).await);
        assert!(registry.is_current(execution, &entry).await);
        registry.remove(execution, &entry).await;
        assert!(!registry.contains(execution).await);
    }

    #[tokio::test]
    async fn checkpoint_preserves_proof_and_deadline_but_never_revives_expired_lease() {
        let (registry, execution, token) = fixture().await;
        let saved = registry.checkpoint().await;
        let saved: Vec<SavedManaged> =
            serde_json::from_slice(&serde_json::to_vec(&saved).unwrap()).unwrap();
        let restored = ManagedRegistry::default();
        restored.restore(saved).await;
        assert!(
            restored
                .authorize(&heartbeat(execution, &token))
                .await
                .is_ok()
        );
        let mut saved = restored.checkpoint().await;
        saved[0].expires_at = SystemTime::UNIX_EPOCH;
        let expired = ManagedRegistry::default();
        expired.restore(saved).await;
        assert!(
            expired
                .authorize(&heartbeat(execution, &token))
                .await
                .is_err()
        );
        assert_eq!(expired.expired().await.len(), 1);
    }

    #[tokio::test]
    async fn heartbeat_does_not_wait_for_settlement_gate_and_revalidation_detects_revocation() {
        let (registry, execution, token) = fixture().await;
        let entry = registry
            .authorize(&heartbeat(execution, &token))
            .await
            .unwrap()
            .unwrap();
        let _settlement = entry.gate.lock().await;
        tokio::time::timeout(Duration::from_millis(100), async {
            registry
                .authorize(&heartbeat(execution, &token))
                .await
                .unwrap()
                .unwrap()
                .renew()
                .await
                .unwrap();
        })
        .await
        .unwrap();
        entry.revoke().await;
        assert!(
            registry
                .authorize(&heartbeat(execution, &token))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn reacquisition_requires_revocation_and_both_cleanup_acknowledgements() {
        let (registry, execution, token) = fixture().await;
        let entry = registry
            .authorize(&heartbeat(execution, &token))
            .await
            .unwrap()
            .unwrap();
        assert!(!registry.reacquirable(execution).await);
        entry.revoke().await;
        entry.settle().await;
        assert!(!registry.reacquirable(execution).await);
        entry
            .cleanup_finished(true, Some("host cleanup unknown".into()))
            .await;
        assert!(!registry.reacquirable(execution).await);
        entry.cleanup_finished(true, None).await;
        assert!(registry.reacquirable(execution).await);
        assert!(
            registry
                .authorize(&heartbeat(execution, &token))
                .await
                .is_err()
        );
        let finalize = AgentCommand::ManagedFinalizeRelease { execution, token };
        assert!(registry.authorize(&finalize).await.is_err());
        assert!(registry.authorize_worker(&finalize).await.is_ok());
    }

    #[tokio::test]
    async fn configuration_requires_acp_proof_and_never_a_generic_mutation() {
        let (registry, execution, token) = fixture().await;
        let configure = AgentCommand::ManagedConfigure {
            execution,
            token: token.clone(),
            change: Default::default(),
        };
        assert!(registry.authorize(&configure).await.is_ok());
        let wrong = AgentCommand::ManagedConfigure {
            execution,
            token: ManagedToken::new(),
            change: Default::default(),
        };
        assert_eq!(
            registry.authorize(&wrong).await.err().unwrap().code,
            AccessErrorCode::AccessDenied
        );
        let generic = AgentCommand::Configure {
            execution,
            change: tickloop_code_access::ConfigurationChange::ExecutionMode {
                mode: ExecutionMode::FullAccess,
            },
            provider: None,
        };
        assert_eq!(
            registry.authorize(&generic).await.err().unwrap().code,
            AccessErrorCode::AccessDenied
        );
    }

    #[tokio::test]
    async fn history_cursors_use_retained_snapshot_and_reject_another_replay() {
        let (registry, execution, token) = fixture().await;
        let entry = registry
            .authorize(&heartbeat(execution, &token))
            .await
            .unwrap()
            .unwrap();
        let id = tickloop_harness::ids::generate();
        *entry.history.lock().await = Some((
            id,
            tickloop_harness::runtime::transcript::DisplayHistorySnapshot {
                position: tickloop_harness::runtime::transcript::StorePosition(42),
                nodes: Vec::new(),
            },
        ));
        // No filesystem read is needed for later pages: this stays valid even if a transcript
        // subsequently grows or its path becomes unavailable.
        let cursor = serde_json::to_string(&HistoryCursor {
            snapshot: id,
            offset: 0,
        })
        .unwrap();
        let page = entry.history(Some(cursor.clone())).await.unwrap();
        assert!(page.items.is_empty());
        assert!(page.next_cursor.is_none());
        assert_eq!(page.session_id, entry.chat);
        entry.history.lock().await.as_mut().unwrap().0 = tickloop_harness::ids::generate();
        assert_eq!(
            entry.history(Some(cursor)).await.unwrap_err().code,
            AccessErrorCode::VersionConflict
        );
        let cursor = serde_json::to_string(&HistoryCursor {
            snapshot: entry.history.lock().await.as_ref().unwrap().0,
            offset: 1,
        })
        .unwrap();
        assert!(entry.history(Some(cursor)).await.is_err());
    }

    #[test]
    fn managed_profiles_default_auto_preserve_explicit_mode_and_refuse_plan() {
        let mut launch = LaunchSpec::embedded(std::env::temp_dir());
        launch.profile = ExecutionProfile::OneShot;
        launch.host_options = AgentHostOptions::product_defaults();
        let normalized = normalized_launch(launch.clone());
        assert_eq!(
            normalized.host_options.execution_mode_override,
            Some(ExecutionMode::Auto)
        );
        assert_eq!(
            normalized.host_options.prompt_policy,
            PromptPolicy::DenyOnPrompt
        );
        launch.host_options.execution_mode_override = Some(ExecutionMode::FullAccess);
        assert_eq!(
            normalized_launch(launch.clone())
                .host_options
                .execution_mode_override,
            Some(ExecutionMode::FullAccess)
        );
        launch.host_options.execution_mode_override = Some(ExecutionMode::Plan);
        assert!(validate_launch(&launch).is_err());
    }
}

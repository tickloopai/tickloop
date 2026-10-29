//! The root Agents the Agents overview keeps across daemon restarts, following Claude Code's agent
//! view job registry: one private JSON file per root Agent in `jobs/` under the configuration root
//! its history saves to. A daemon keeps its running roots' files current; the first time it uses a
//! configuration root's registry, it marks the Agents recorded as running that it does not run as
//! ended. Nothing here loads or runs an Agent.

use std::{
    collections::{HashMap, HashSet},
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use serde::{Deserialize, Serialize};
use tickloop_code_access::{
    AccessError, AccessErrorCode, AgentSummary, EndReason, EndedAgent, EndedOutcome,
    ExecutionAvailability, TranscriptRef,
};
use tickloop_harness::base::private_fs::{
    PRIVATE_FILE_MODE, create_private_directory, replace_file_atomically, unix_time_milliseconds,
    verify_private_directory,
};
use tickloop_harness::protocol::LoopState;
use uuid::Uuid;

pub const REGISTRY_DIRECTORY: &str = "jobs";
const RECORD_EXTENSION: &str = "json";
const RECORD_VERSION: u32 = 1;
/// Claude Code's `KOt`: an Agent whose last activity is older than this when its execution is found
/// gone reads as stopped instead of failed.
pub const STALE_AFTER_MILLISECONDS: u64 = 48 * 60 * 60 * 1000;
/// Claude Code's default `cleanupPeriodDays`.
pub const DEFAULT_CLEANUP_PERIOD_DAYS: u64 = 30;
/// Claude Code's 5-second grace: a record that says running but whose execution this daemon does
/// not list reads as failed only once it is older than this, so a just-created Agent is not.
pub const UNLISTED_GRACE_MILLISECONDS: u64 = 5_000;
const DAY_MILLISECONDS: u64 = 24 * 60 * 60 * 1000;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecordStatus {
    /// Running in the daemon that last wrote the record.
    Live,
    /// Its last turn had finished with nothing pending when its execution ended.
    Completed,
    Failed,
    Stopped,
}

/// One root Agent as `jobs/<chat id>.json` stores it.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AgentRecord {
    pub version: u32,
    pub agent_id: Uuid,
    pub chat_id: Uuid,
    pub folder: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    /// Unix milliseconds.
    pub created_at: u64,
    pub updated_at: u64,
    pub last_state: LoopState,
    pub status: RecordStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_reason: Option<EndReason>,
    /// Whether work was still coming while its Loop was idle, at the last state written: queued
    /// input, an active Goal, a scheduled task, a received Tick Subscription, a running Shell or
    /// a working Subagent. Records written before it was kept read as nothing pending.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_work: Option<bool>,
}

impl AgentRecord {
    fn ended(mut self, status: RecordStatus, reason: EndReason, ended_at: u64) -> Self {
        self.status = status;
        self.end_reason = Some(reason);
        self.ended_at = Some(ended_at);
        self
    }

    fn is_ended(&self) -> bool {
        self.status != RecordStatus::Live
    }

    fn outcome(&self) -> Option<EndedOutcome> {
        match self.status {
            RecordStatus::Live => None,
            RecordStatus::Completed => Some(EndedOutcome::Completed),
            RecordStatus::Failed => Some(EndedOutcome::Failed),
            RecordStatus::Stopped => Some(EndedOutcome::Stopped),
        }
    }

    /// Claude Code's done job: the turn ended and nothing was pending. A turn that ended in a
    /// failure is as terminal; its record keeps that it failed.
    fn finished_its_turn(&self) -> bool {
        matches!(
            self.last_state,
            LoopState::Idle | LoopState::Interrupted | LoopState::Failed
        ) && !self.pending_work.unwrap_or(false)
    }

    /// Claude Code's blocked job, which no reaper ends: a question or plan left for the user.
    /// A permission prompt is mid-turn work in Claude Code and is reaped like it.
    fn awaited_the_user(&self) -> bool {
        matches!(
            self.last_state,
            LoopState::WaitingQuestion | LoopState::WaitingPlan
        )
    }

    /// Ends a record whose execution is gone, as Claude Code's restart reaper does. A finished
    /// turn stays finished and a question stays for the user; other work, an idle Agent with
    /// pending work included, failed now when it was active within
    /// [`STALE_AFTER_MILLISECONDS`] and stopped back then when it was not. `gone` says how the
    /// execution went: [`EndReason::WorkerLost`] is Claude Code's display fallback, which fails
    /// work without the 48-hour rule.
    fn end_without_execution(&mut self, now: u64, gone: EndReason) {
        let stale = now.saturating_sub(self.updated_at) > STALE_AFTER_MILLISECONDS;
        let (status, reason, ended_at) = if self.finished_its_turn() {
            let status = if self.last_state == LoopState::Failed {
                RecordStatus::Failed
            } else {
                RecordStatus::Completed
            };
            (status, EndReason::Finished, self.updated_at)
        } else if stale && !self.awaited_the_user() && gone == EndReason::DaemonStopped {
            (RecordStatus::Stopped, EndReason::DaemonOff, self.updated_at)
        } else {
            (RecordStatus::Failed, gone, now)
        };
        self.status = status;
        self.end_reason = Some(reason);
        self.ended_at = Some(ended_at);
    }

    /// An earlier reaper ended a finished turn as failed or stopped before records kept whether
    /// work was pending; such a record reads as finished, as it would now.
    fn reaped_before_pending_work_was_kept(&self) -> bool {
        self.pending_work.is_none()
            && matches!(
                self.end_reason,
                Some(EndReason::DaemonStopped | EndReason::DaemonOff)
            )
            && self.finished_its_turn()
    }
}

/// The running record of the root Agent `summary` describes, keeping what `existing` knew first.
fn record_of(summary: &AgentSummary, existing: Option<&AgentRecord>, now: u64) -> AgentRecord {
    AgentRecord {
        version: RECORD_VERSION,
        agent_id: summary.execution.agent_id,
        chat_id: summary.chat_id,
        folder: summary.folder.clone(),
        workspace: summary.workspace.clone(),
        title: summary.title.clone(),
        preview: summary.preview.clone(),
        created_at: existing.map_or(now, |record| record.created_at),
        updated_at: summary
            .updated_at
            .or(existing.map(|record| record.updated_at))
            .unwrap_or(now),
        last_state: summary.state.clone(),
        status: RecordStatus::Live,
        ended_at: None,
        end_reason: None,
        pending_work: summary.pending_work,
    }
}

/// Whether a Subagent of the root `root` still works: its turn runs or waits on the user.
fn family_works(directory: &[AgentSummary], root: &AgentSummary) -> bool {
    let parents = directory
        .iter()
        .map(|summary| (summary.execution, summary.parent))
        .collect::<HashMap<_, _>>();
    let descends = |summary: &AgentSummary| {
        let mut visited = HashSet::new();
        let mut current = summary.parent;
        while let Some(parent) = current.filter(|parent| visited.insert(*parent)) {
            if parent == root.execution {
                return true;
            }
            current = parents.get(&parent).copied().flatten();
        }
        false
    };
    directory.iter().any(|summary| {
        summary.availability == ExecutionAvailability::Live
            && !matches!(
                summary.state,
                LoopState::Idle | LoopState::Interrupted | LoopState::Failed
            )
            && descends(summary)
    })
}

type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;
type OpenRegistry = Box<dyn Fn(PathBuf) -> AgentRegistry + Send + Sync>;

/// The registries of the configuration roots this daemon's Agents save their history to.
pub struct AgentRegistries {
    open: OpenRegistry,
    registries: Mutex<HashMap<PathBuf, Arc<AgentRegistry>>>,
}

impl AgentRegistries {
    pub fn new(open: impl Fn(PathBuf) -> AgentRegistry + Send + Sync + 'static) -> Self {
        Self {
            open: Box::new(open),
            registries: Mutex::default(),
        }
    }

    /// Registries that age ended Agents out by each root's own `cleanupPeriodDays`.
    pub fn from_settings() -> Self {
        Self::new(|root| {
            let cleanup_period_days =
                tickloop_code_settings::UserSettings::in_root(&root).cleanup_period_days();
            AgentRegistry::new(root, cleanup_period_days)
        })
    }

    /// The registry of `store_root`, reconciled against `directory` when this daemon first uses
    /// it; `directory` holds every Agent this daemon runs.
    pub(crate) fn registry(
        &self,
        store_root: &Path,
        directory: &[AgentSummary],
    ) -> Arc<AgentRegistry> {
        let mut registries = self
            .registries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(registry) = registries.get(store_root) {
            return registry.clone();
        }
        let registry = Arc::new((self.open)(store_root.to_path_buf()));
        registry.reconcile(directory);
        registries.insert(store_root.to_path_buf(), registry.clone());
        registry
    }

    /// Records the root Agents `directory` lists in the registry of the root each saves to.
    pub(crate) fn observe(&self, directory: &[AgentSummary]) {
        let roots = directory
            .iter()
            .filter_map(|summary| summary.record.as_ref())
            .map(|record| record.store_root.clone())
            .collect::<HashSet<_>>();
        for root in roots {
            self.registry(&root, directory).observe(directory);
        }
    }
}

pub struct AgentRegistry {
    store_root: PathBuf,
    directory: PathBuf,
    cleanup_period_milliseconds: u64,
    clock: Clock,
    state: Mutex<RegistryState>,
}

#[derive(Default)]
struct RegistryState {
    /// The records as last read or written; `None` until the daemon reconciled them at its start.
    records: Option<HashMap<Uuid, AgentRecord>>,
}

impl AgentRegistry {
    /// The registry of the configuration root `store_root`. Ended Agents older than
    /// `cleanup_period_days` (Claude Code's default when unset) leave it at the next daemon start.
    pub fn new(store_root: PathBuf, cleanup_period_days: Option<u64>) -> Self {
        let days = cleanup_period_days.unwrap_or(DEFAULT_CLEANUP_PERIOD_DAYS);
        Self {
            directory: store_root.join(REGISTRY_DIRECTORY),
            store_root,
            cleanup_period_milliseconds: days.saturating_mul(DAY_MILLISECONDS),
            clock: Arc::new(unix_time_milliseconds),
            state: Mutex::default(),
        }
    }

    pub fn with_clock(mut self, clock: Arc<dyn Fn() -> u64 + Send + Sync>) -> Self {
        self.clock = clock;
        self
    }

    /// Ends the records of Agents the daemon does not run and drops ended ones past the cleanup
    /// period, then starts following `directory`.
    pub(crate) fn reconcile(&self, directory: &[AgentSummary]) {
        let now = (self.clock)();
        let running = self.running_roots(directory);
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let mut records = HashMap::new();
        for mut record in self.read_all() {
            let unexecuted = !running.contains(&record.chat_id);
            if unexecuted && (!record.is_ended() || record.reaped_before_pending_work_was_kept()) {
                record.end_without_execution(now, EndReason::DaemonStopped);
                self.write_logged(&record);
            }
            let expired = record.is_ended()
                && !running.contains(&record.chat_id)
                && record
                    .ended_at
                    .unwrap_or(record.updated_at)
                    .saturating_add(self.cleanup_period_milliseconds)
                    < now;
            if expired {
                self.remove_logged(record.chat_id);
            } else {
                records.insert(record.chat_id, record);
            }
        }
        state.records = Some(records);
        drop(state);
        self.observe(directory);
    }

    /// Records each root Agent of this configuration root that `directory` lists, writing only
    /// what changed. A Subagent belongs to its root and never gets a record of its own.
    pub(crate) fn observe(&self, directory: &[AgentSummary]) {
        let now = (self.clock)();
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let Some(records) = state.records.as_mut() else {
            return;
        };
        for summary in directory.iter().filter(|summary| self.is_own_root(summary)) {
            let existing = records.get(&summary.chat_id);
            let mut summary = summary.clone();
            if family_works(directory, &summary) {
                summary.pending_work = Some(true);
            }
            let summary = &summary;
            let mut record = record_of(summary, existing, now);
            if summary.availability == ExecutionAvailability::Lost {
                record.end_without_execution(now, EndReason::WorkerLost);
                if let Some(ended_at) = existing.and_then(|record| record.ended_at) {
                    record.ended_at = Some(ended_at);
                }
            }
            if existing != Some(&record) {
                self.write_logged(&record);
                records.insert(record.chat_id, record);
            }
        }
    }

    /// Records that the user stopped the root Agent `summary` describes.
    pub(crate) fn mark_stopped(&self, summary: &AgentSummary) {
        if !self.is_own_root(summary) {
            return;
        }
        let now = (self.clock)();
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let Some(records) = state.records.as_mut() else {
            return;
        };
        let record = record_of(summary, records.get(&summary.chat_id), now).ended(
            RecordStatus::Stopped,
            EndReason::UserStopped,
            now,
        );
        self.write_logged(&record);
        records.insert(record.chat_id, record);
    }

    /// The ended root Agents `directory` does not list, with whether their history can load.
    pub(crate) async fn ended(&self, directory: &[AgentSummary]) -> Vec<EndedAgent> {
        let listed = directory
            .iter()
            .map(|summary| summary.chat_id)
            .collect::<HashSet<_>>();
        let now = (self.clock)();
        let records = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            let Some(records) = state.records.as_mut() else {
                return Vec::new();
            };
            // Claude Code's display fallback: a record still saying running whose execution this
            // daemon does not list ends once past the grace, and the record says so too.
            for record in records.values_mut() {
                if !record.is_ended()
                    && !listed.contains(&record.chat_id)
                    && now.saturating_sub(record.created_at) >= UNLISTED_GRACE_MILLISECONDS
                {
                    record.end_without_execution(now, EndReason::WorkerLost);
                    self.write_logged(record);
                }
            }
            records.values().cloned().collect::<Vec<_>>()
        };
        let mut ended = Vec::new();
        for record in records {
            if listed.contains(&record.chat_id) {
                continue;
            }
            let Some(outcome) = record.outcome() else {
                continue;
            };
            let resumable = tickloop_harness::runtime::transcript::records::saved_record_exists(
                self.store_root.clone(),
                record.chat_id,
            )
            .await
            .unwrap_or(false);
            ended.push(EndedAgent {
                record: TranscriptRef {
                    store_root: self.store_root.clone(),
                    chat_id: record.chat_id,
                },
                agent_id: record.agent_id,
                folder: record.folder,
                workspace: record.workspace,
                title: record.title,
                preview: record.preview,
                created_at: record.created_at,
                updated_at: record.updated_at,
                ended_at: record.ended_at.unwrap_or(record.updated_at),
                last_state: record.last_state,
                outcome,
                reason: record.end_reason.unwrap_or(EndReason::DaemonStopped),
                resumable,
            });
        }
        ended.sort_by_key(|agent| (std::cmp::Reverse(agent.ended_at), agent.record.chat_id));
        ended
    }

    /// Removes the ended Agent of `chat_id`; its Transcript stays. A running Agent is refused.
    pub(crate) fn forget(
        &self,
        chat_id: Uuid,
        directory: &[AgentSummary],
    ) -> Result<(), AccessError> {
        if directory.iter().any(|summary| summary.chat_id == chat_id) {
            return Err(AccessError::new(
                AccessErrorCode::Busy,
                "this Agent is running; only an ended Agent can be removed from the list",
            ));
        }
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let records = state
            .records
            .as_mut()
            .ok_or_else(|| AccessError::unavailable("the Agent list is still loading"))?;
        let ended = records.get(&chat_id).is_some_and(AgentRecord::is_ended);
        if !ended {
            return Err(AccessError::not_found(
                "no ended Agent of this Chat is listed",
            ));
        }
        match std::fs::remove_file(self.record_path(chat_id)) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(AccessError::new(
                    AccessErrorCode::Storage,
                    format!("cannot remove the ended Agent: {error}"),
                ));
            }
        }
        records.remove(&chat_id);
        Ok(())
    }

    fn running_roots(&self, directory: &[AgentSummary]) -> HashSet<Uuid> {
        directory
            .iter()
            .filter(|summary| {
                self.is_own_root(summary) && summary.availability == ExecutionAvailability::Live
            })
            .map(|summary| summary.chat_id)
            .collect()
    }

    fn is_own_root(&self, summary: &AgentSummary) -> bool {
        summary.parent.is_none()
            && summary.subagent.is_none()
            && summary
                .record
                .as_ref()
                .is_some_and(|record| record.store_root == self.store_root)
    }

    fn record_path(&self, chat_id: Uuid) -> PathBuf {
        self.directory
            .join(format!("{}.{RECORD_EXTENSION}", chat_id.hyphenated()))
    }

    /// Every readable record; a file that does not parse is skipped and left in place.
    fn read_all(&self) -> Vec<AgentRecord> {
        let entries = match std::fs::read_dir(&self.directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Vec::new(),
            Err(error) => {
                tracing::warn!(%error, directory = %self.directory.display(), "cannot read the Agent registry");
                return Vec::new();
            }
        };
        if let Err(error) = verify_private_directory(&self.directory, "Agent registry") {
            tracing::warn!(%error, "ignoring the Agent registry");
            return Vec::new();
        }
        let mut records = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some(RECORD_EXTENSION) {
                continue;
            }
            let parsed = std::fs::read(&path)
                .map_err(|error| error.to_string())
                .and_then(|bytes| {
                    serde_json::from_slice::<AgentRecord>(&bytes).map_err(|error| error.to_string())
                });
            match parsed {
                Ok(record) if path == self.record_path(record.chat_id) => records.push(record),
                Ok(_) => tracing::warn!(path = %path.display(), "skipping a misnamed Agent record"),
                Err(error) => {
                    tracing::warn!(%error, path = %path.display(), "skipping an unreadable Agent record")
                }
            }
        }
        records
    }

    fn write(&self, record: &AgentRecord) -> io::Result<()> {
        create_private_directory(&self.directory)?;
        verify_private_directory(&self.directory, "Agent registry")?;
        let contents = serde_json::to_vec_pretty(record).map_err(io::Error::other)?;
        replace_file_atomically(
            &self.record_path(record.chat_id),
            &contents,
            PRIVATE_FILE_MODE,
        )
    }

    fn write_logged(&self, record: &AgentRecord) {
        if let Err(error) = self.write(record) {
            tracing::warn!(%error, chat = %record.chat_id, "cannot write the Agent record");
        }
    }

    fn remove_logged(&self, chat_id: Uuid) {
        if let Err(error) = std::fs::remove_file(self.record_path(chat_id))
            && error.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!(%error, chat = %chat_id, "cannot remove the Agent record");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use tickloop_code_access::ExecutionRef;

    use super::*;

    const HOUR: u64 = 60 * 60 * 1000;

    struct Fixture {
        _directory: tempfile::TempDir,
        root: PathBuf,
        now: Arc<AtomicU64>,
    }

    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            Self {
                root: directory.path().join("root"),
                _directory: directory,
                now: Arc::new(AtomicU64::new(1_000 * HOUR)),
            }
        }

        fn registry(&self, cleanup_period_days: Option<u64>) -> AgentRegistry {
            let now = self.now.clone();
            AgentRegistry::new(self.root.clone(), cleanup_period_days)
                .with_clock(Arc::new(move || now.load(Ordering::SeqCst)))
        }

        fn advance(&self, milliseconds: u64) {
            self.now.fetch_add(milliseconds, Ordering::SeqCst);
        }

        fn now(&self) -> u64 {
            self.now.load(Ordering::SeqCst)
        }

        /// A root Agent in the middle of a turn.
        fn root_summary(&self, chat_id: Uuid) -> AgentSummary {
            AgentSummary {
                availability: ExecutionAvailability::Live,
                execution: ExecutionRef {
                    daemon_epoch: Uuid::from_u128(1),
                    agent_id: Uuid::from_u128(chat_id.as_u128() + 1),
                    generation: 0,
                },
                chat_id,
                folder: Some("/work".into()),
                state: LoopState::Running,
                run_id: None,
                run_started_at: None,
                title: "Fix the parser".into(),
                subagent: None,
                has_work: Some(true),
                pending_work: None,
                updated_at: Some(self.now()),
                preview: None,
                waiting_permissions: 0,
                waiting_questions: 0,
                parent: None,
                record: Some(TranscriptRef {
                    store_root: self.root.clone(),
                    chat_id,
                }),
                workspace: None,
                capabilities: Vec::new(),
                views: Vec::new(),
            }
        }
    }

    fn statuses(registry: &AgentRegistry) -> Vec<(Uuid, RecordStatus, Option<EndReason>)> {
        let mut records = registry
            .read_all()
            .into_iter()
            .map(|record| (record.chat_id, record.status, record.end_reason))
            .collect::<Vec<_>>();
        records.sort_by_key(|(chat, ..)| *chat);
        records
    }

    #[test]
    fn a_registry_follows_nothing_until_the_daemon_reconciled_it() {
        let fixture = Fixture::new();
        let registry = fixture.registry(None);
        let chat = Uuid::from_u128(10);

        registry.observe(&[fixture.root_summary(chat)]);
        assert!(registry.read_all().is_empty());

        registry.reconcile(&[fixture.root_summary(chat)]);
        assert_eq!(statuses(&registry), [(chat, RecordStatus::Live, None)]);
    }

    #[test]
    fn an_agent_gone_at_restart_fails_within_48_hours_and_stops_after_them() {
        let fixture = Fixture::new();
        let recent = Uuid::from_u128(10);
        let stale = Uuid::from_u128(20);
        let before = fixture.registry(None);
        before.reconcile(&[]);
        let mut stale_summary = fixture.root_summary(stale);
        stale_summary.updated_at = Some(fixture.now() - STALE_AFTER_MILLISECONDS - 1);
        let stale_activity = stale_summary.updated_at.unwrap();
        before.observe(&[fixture.root_summary(recent), stale_summary]);
        fixture.advance(HOUR);

        let after = fixture.registry(None);
        after.reconcile(&[]);

        assert_eq!(
            statuses(&after),
            [
                (recent, RecordStatus::Failed, Some(EndReason::DaemonStopped)),
                (stale, RecordStatus::Stopped, Some(EndReason::DaemonOff)),
            ]
        );
        let records = after.read_all();
        let ended_at = |chat| {
            records
                .iter()
                .find(|record| record.chat_id == chat)
                .unwrap()
                .ended_at
        };
        assert_eq!(ended_at(recent), Some(fixture.now()));
        assert_eq!(ended_at(stale), Some(stale_activity));
    }

    #[test]
    fn a_root_a_handoff_kept_running_stays_live() {
        let fixture = Fixture::new();
        let chat = Uuid::from_u128(10);
        let before = fixture.registry(None);
        before.reconcile(&[fixture.root_summary(chat)]);

        let after = fixture.registry(None);
        after.reconcile(&[fixture.root_summary(chat)]);

        assert_eq!(statuses(&after), [(chat, RecordStatus::Live, None)]);
    }

    #[test]
    fn a_lost_worker_fails_its_root_at_once() {
        let fixture = Fixture::new();
        let chat = Uuid::from_u128(10);
        let registry = fixture.registry(None);
        registry.reconcile(&[fixture.root_summary(chat)]);
        let mut lost = fixture.root_summary(chat);
        lost.availability = ExecutionAvailability::Lost;

        registry.observe(&[lost]);

        assert_eq!(
            statuses(&registry),
            [(chat, RecordStatus::Failed, Some(EndReason::WorkerLost))]
        );
    }

    #[test]
    fn subagents_and_other_configuration_roots_get_no_record() {
        let fixture = Fixture::new();
        let registry = fixture.registry(None);
        let root = fixture.root_summary(Uuid::from_u128(10));
        let mut child = fixture.root_summary(Uuid::from_u128(20));
        child.parent = Some(root.execution);
        let mut elsewhere = fixture.root_summary(Uuid::from_u128(30));
        elsewhere.record.as_mut().unwrap().store_root = "/elsewhere".into();
        let mut unsaved = fixture.root_summary(Uuid::from_u128(40));
        unsaved.record = None;

        registry.reconcile(&[root.clone(), child, elsewhere, unsaved]);

        assert_eq!(
            statuses(&registry),
            [(root.chat_id, RecordStatus::Live, None)]
        );
    }

    #[test]
    fn ended_agents_past_the_cleanup_period_leave_at_the_next_start() {
        let fixture = Fixture::new();
        let old = Uuid::from_u128(10);
        let recent = Uuid::from_u128(20);
        let running = Uuid::from_u128(30);
        let first = fixture.registry(Some(7));
        first.reconcile(&[]);
        first.observe(&[fixture.root_summary(old)]);
        fixture.advance(HOUR);
        fixture.registry(Some(7)).reconcile(&[]);
        fixture.advance(6 * DAY_MILLISECONDS);
        let second = fixture.registry(Some(7));
        second.reconcile(&[]);
        second.observe(&[fixture.root_summary(recent), fixture.root_summary(running)]);
        fixture.advance(DAY_MILLISECONDS + HOUR);

        let third = fixture.registry(Some(7));
        third.reconcile(&[fixture.root_summary(running)]);

        assert_eq!(
            statuses(&third),
            [
                (recent, RecordStatus::Failed, Some(EndReason::DaemonStopped)),
                (running, RecordStatus::Live, None),
            ]
        );
    }

    #[test]
    fn writes_happen_only_when_a_root_changes() {
        let fixture = Fixture::new();
        let chat = Uuid::from_u128(10);
        let registry = fixture.registry(None);
        registry.reconcile(&[fixture.root_summary(chat)]);
        let path = registry.record_path(chat);
        let written = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::fs::remove_file(&path).unwrap();

        registry.observe(&[fixture.root_summary(chat)]);
        assert!(!path.exists(), "an unchanged root was written again");

        let mut renamed = fixture.root_summary(chat);
        renamed.title = "Fix the lexer".into();
        registry.observe(&[renamed]);
        assert!(std::fs::metadata(&path).unwrap().modified().unwrap() >= written);
        assert_eq!(registry.read_all()[0].title, "Fix the lexer");
    }

    #[tokio::test]
    async fn forgetting_refuses_running_and_unknown_agents() {
        let fixture = Fixture::new();
        let chat = Uuid::from_u128(10);
        let registry = fixture.registry(None);
        registry.reconcile(&[fixture.root_summary(chat)]);

        let running = registry
            .forget(chat, &[fixture.root_summary(chat)])
            .unwrap_err();
        assert_eq!(running.code, AccessErrorCode::Busy);
        let unknown = registry.forget(Uuid::from_u128(99), &[]).unwrap_err();
        assert_eq!(unknown.code, AccessErrorCode::NotFound);

        let restarted = fixture.registry(None);
        restarted.reconcile(&[]);
        assert_eq!(restarted.ended(&[]).await.len(), 1);
        restarted.forget(chat, &[]).unwrap();
        assert!(restarted.ended(&[]).await.is_empty());
        assert!(restarted.read_all().is_empty());
    }

    #[test]
    fn an_agent_waiting_for_the_user_still_fails_after_48_hours() {
        let fixture = Fixture::new();
        let chat = Uuid::from_u128(10);
        let before = fixture.registry(None);
        before.reconcile(&[]);
        let mut waiting = fixture.root_summary(chat);
        waiting.state = LoopState::WaitingQuestion;
        waiting.updated_at = Some(fixture.now() - STALE_AFTER_MILLISECONDS - 1);
        before.observe(&[waiting]);

        let after = fixture.registry(None);
        after.reconcile(&[]);

        assert_eq!(
            statuses(&after),
            [(chat, RecordStatus::Failed, Some(EndReason::DaemonStopped))]
        );
        assert_eq!(after.read_all()[0].last_state, LoopState::WaitingQuestion);
    }

    #[tokio::test]
    async fn an_unlisted_running_record_reads_as_failed_only_after_five_seconds() {
        let fixture = Fixture::new();
        let chat = Uuid::from_u128(10);
        let registry = fixture.registry(None);
        registry.reconcile(&[fixture.root_summary(chat)]);

        fixture.advance(UNLISTED_GRACE_MILLISECONDS - 1);
        assert!(registry.ended(&[]).await.is_empty());

        fixture.advance(1);
        let ended = registry.ended(&[]).await;
        assert_eq!(ended.len(), 1);
        assert_eq!(ended[0].outcome, EndedOutcome::Failed);
        assert_eq!(ended[0].reason, EndReason::WorkerLost);
        // The record says what the overview shows.
        assert_eq!(
            statuses(&registry),
            [(chat, RecordStatus::Failed, Some(EndReason::WorkerLost))]
        );
    }

    #[tokio::test]
    async fn a_stopped_root_is_ended_by_the_user() {
        let fixture = Fixture::new();
        let chat = Uuid::from_u128(10);
        let registry = fixture.registry(None);
        registry.reconcile(&[fixture.root_summary(chat)]);
        fixture.advance(HOUR);

        registry.mark_stopped(&fixture.root_summary(chat));

        assert_eq!(
            statuses(&registry),
            [(chat, RecordStatus::Stopped, Some(EndReason::UserStopped))]
        );
        let ended = registry.ended(&[]).await;
        assert_eq!(ended[0].outcome, EndedOutcome::Stopped);
        assert_eq!(ended[0].ended_at, fixture.now());
    }

    /// A root Agent whose turn ended with nothing pending.
    fn finished(fixture: &Fixture, chat: Uuid) -> AgentSummary {
        AgentSummary {
            state: LoopState::Idle,
            pending_work: Some(false),
            ..fixture.root_summary(chat)
        }
    }

    /// Records `summaries` in one daemon, then reconciles them in the next one `elapsed` later.
    fn restart_after(fixture: &Fixture, summaries: &[AgentSummary], elapsed: u64) -> AgentRegistry {
        let before = fixture.registry(None);
        before.reconcile(&[]);
        before.observe(summaries);
        fixture.advance(elapsed);
        let after = fixture.registry(None);
        after.reconcile(&[]);
        after
    }

    fn record(registry: &AgentRegistry, chat: Uuid) -> AgentRecord {
        registry
            .read_all()
            .into_iter()
            .find(|record| record.chat_id == chat)
            .unwrap()
    }

    #[test]
    fn a_finished_turn_with_nothing_pending_stays_completed_at_any_age() {
        let idle = Uuid::from_u128(10);
        let interrupted = Uuid::from_u128(20);
        for elapsed in [HOUR, STALE_AFTER_MILLISECONDS + HOUR] {
            let fixture = Fixture::new();
            let finished_at = fixture.now();
            let interrupted_summary = AgentSummary {
                state: LoopState::Interrupted,
                ..finished(&fixture, interrupted)
            };
            let after = restart_after(
                &fixture,
                &[finished(&fixture, idle), interrupted_summary],
                elapsed,
            );
            assert_eq!(
                statuses(&after),
                [
                    (idle, RecordStatus::Completed, Some(EndReason::Finished)),
                    (
                        interrupted,
                        RecordStatus::Completed,
                        Some(EndReason::Finished)
                    ),
                ],
                "{elapsed}"
            );
            assert_eq!(record(&after, idle).ended_at, Some(finished_at));
        }
    }

    #[tokio::test]
    async fn a_finished_turn_lists_as_completed_without_failure() {
        let fixture = Fixture::new();
        let chat = Uuid::from_u128(10);
        let after = restart_after(&fixture, &[finished(&fixture, chat)], HOUR);

        let ended = after.ended(&[]).await;

        assert_eq!(ended.len(), 1);
        assert_eq!(ended[0].outcome, EndedOutcome::Completed);
        assert_eq!(ended[0].reason, EndReason::Finished);
    }

    #[test]
    fn a_turn_that_failed_stays_failed_without_the_48_hour_rule() {
        let fixture = Fixture::new();
        let chat = Uuid::from_u128(10);
        let failed = AgentSummary {
            state: LoopState::Failed,
            ..finished(&fixture, chat)
        };

        let after = restart_after(&fixture, &[failed], STALE_AFTER_MILLISECONDS + HOUR);

        assert_eq!(
            statuses(&after),
            [(chat, RecordStatus::Failed, Some(EndReason::Finished))]
        );
    }

    #[test]
    fn an_idle_agent_with_pending_work_fails_within_48_hours_and_stops_after_them() {
        for (elapsed, expected) in [
            (HOUR, (RecordStatus::Failed, EndReason::DaemonStopped)),
            (
                STALE_AFTER_MILLISECONDS + HOUR,
                (RecordStatus::Stopped, EndReason::DaemonOff),
            ),
        ] {
            let fixture = Fixture::new();
            let chat = Uuid::from_u128(10);
            let waiting_on_work = AgentSummary {
                pending_work: Some(true),
                ..finished(&fixture, chat)
            };

            let after = restart_after(&fixture, &[waiting_on_work], elapsed);

            assert_eq!(
                statuses(&after),
                [(chat, expected.0, Some(expected.1))],
                "{elapsed}"
            );
        }
    }

    #[test]
    fn a_working_subagent_keeps_its_idle_root_pending() {
        let fixture = Fixture::new();
        let root = finished(&fixture, Uuid::from_u128(10));
        let mut child = fixture.root_summary(Uuid::from_u128(20));
        child.parent = Some(root.execution);
        child.subagent = Some(tickloop_harness::protocol::SubagentRegistration {
            child_chat_id: child.chat_id.into(),
            handle: "audit".into(),
            description: "Audit".into(),
            created_at: 0,
            color: None,
        });
        let mut grandchild = fixture.root_summary(Uuid::from_u128(30));
        grandchild.parent = Some(child.execution);
        child.state = LoopState::Idle;

        let after = restart_after(&fixture, &[root.clone(), child, grandchild], HOUR);

        assert_eq!(record(&after, root.chat_id).pending_work, Some(true));
        assert_eq!(
            statuses(&after),
            [(
                root.chat_id,
                RecordStatus::Failed,
                Some(EndReason::DaemonStopped)
            )]
        );
    }

    #[test]
    fn mid_turn_work_and_a_permission_prompt_fail_within_48_hours_and_stop_after_them() {
        for state in [
            LoopState::Running,
            LoopState::Queued,
            LoopState::WaitingPermission,
        ] {
            for (elapsed, expected) in [
                (HOUR, (RecordStatus::Failed, EndReason::DaemonStopped)),
                (
                    STALE_AFTER_MILLISECONDS + HOUR,
                    (RecordStatus::Stopped, EndReason::DaemonOff),
                ),
            ] {
                let fixture = Fixture::new();
                let chat = Uuid::from_u128(10);
                let working = AgentSummary {
                    state: state.clone(),
                    ..fixture.root_summary(chat)
                };

                let after = restart_after(&fixture, &[working], elapsed);

                assert_eq!(
                    statuses(&after),
                    [(chat, expected.0, Some(expected.1))],
                    "{state:?} {elapsed}"
                );
            }
        }
    }

    #[test]
    fn a_question_or_plan_left_for_the_user_is_never_stopped() {
        for state in [LoopState::WaitingQuestion, LoopState::WaitingPlan] {
            let fixture = Fixture::new();
            let chat = Uuid::from_u128(10);
            let asking = AgentSummary {
                state: state.clone(),
                ..fixture.root_summary(chat)
            };

            let after = restart_after(&fixture, &[asking], STALE_AFTER_MILLISECONDS + HOUR);

            assert_eq!(
                statuses(&after),
                [(chat, RecordStatus::Failed, Some(EndReason::DaemonStopped))],
                "{state:?}"
            );
            assert_eq!(record(&after, chat).last_state, state);
        }
    }

    #[test]
    fn records_without_the_pending_fact_read_as_finished_when_their_turn_ended() {
        let fixture = Fixture::new();
        let chat = Uuid::from_u128(10);
        let unknown = AgentSummary {
            pending_work: None,
            ..finished(&fixture, chat)
        };

        let after = restart_after(&fixture, &[unknown], HOUR);

        assert_eq!(
            statuses(&after),
            [(chat, RecordStatus::Completed, Some(EndReason::Finished))]
        );
    }

    #[test]
    fn a_finished_turn_an_earlier_reaper_failed_reads_as_completed_again() {
        let fixture = Fixture::new();
        let registry = fixture.registry(None);
        let chat = Uuid::from_u128(10);
        let updated_at = fixture.now() - HOUR;
        create_private_directory(&registry.directory).unwrap();
        let old_reap = serde_json::json!({
            "version": 1,
            "agentId": Uuid::from_u128(11),
            "chatId": chat,
            "folder": "/work",
            "title": "问候与交流",
            "createdAt": updated_at - HOUR,
            "updatedAt": updated_at,
            "lastState": "Idle",
            "status": "failed",
            "endedAt": fixture.now(),
            "endReason": "daemon_stopped"
        });
        std::fs::write(
            registry.record_path(chat),
            serde_json::to_vec(&old_reap).unwrap(),
        )
        .unwrap();
        let working = Uuid::from_u128(20);
        let mut old_working = old_reap.clone();
        old_working["chatId"] = serde_json::json!(working);
        old_working["lastState"] = serde_json::json!("Running");
        std::fs::write(
            registry.record_path(working),
            serde_json::to_vec(&old_working).unwrap(),
        )
        .unwrap();

        registry.reconcile(&[]);

        assert_eq!(
            statuses(&registry),
            [
                (chat, RecordStatus::Completed, Some(EndReason::Finished)),
                (
                    working,
                    RecordStatus::Failed,
                    Some(EndReason::DaemonStopped)
                ),
            ]
        );
        assert_eq!(record(&registry, chat).ended_at, Some(updated_at));
    }

    #[tokio::test]
    async fn the_display_fallback_keeps_a_finished_turn_completed_and_writes_what_it_shows() {
        let fixture = Fixture::new();
        let done = Uuid::from_u128(10);
        let working = Uuid::from_u128(20);
        let registry = fixture.registry(None);
        registry.reconcile(&[finished(&fixture, done), fixture.root_summary(working)]);
        fixture.advance(UNLISTED_GRACE_MILLISECONDS);

        let ended = registry.ended(&[]).await;

        let outcome = |chat| {
            ended
                .iter()
                .find(|agent| agent.record.chat_id == chat)
                .map(|agent| (agent.outcome, agent.reason))
        };
        assert_eq!(
            outcome(done),
            Some((EndedOutcome::Completed, EndReason::Finished))
        );
        assert_eq!(
            outcome(working),
            Some((EndedOutcome::Failed, EndReason::WorkerLost))
        );
        assert_eq!(
            statuses(&registry),
            [
                (done, RecordStatus::Completed, Some(EndReason::Finished)),
                (working, RecordStatus::Failed, Some(EndReason::WorkerLost)),
            ]
        );
    }

    #[test]
    fn a_lost_worker_keeps_a_finished_turn_completed() {
        let fixture = Fixture::new();
        let chat = Uuid::from_u128(10);
        let registry = fixture.registry(None);
        registry.reconcile(&[finished(&fixture, chat)]);
        let mut lost = finished(&fixture, chat);
        lost.availability = ExecutionAvailability::Lost;

        registry.observe(&[lost]);

        assert_eq!(
            statuses(&registry),
            [(chat, RecordStatus::Completed, Some(EndReason::Finished))]
        );
    }
}

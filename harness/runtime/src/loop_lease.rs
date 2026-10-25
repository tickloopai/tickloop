//! Cross-process lease and receiving state for Triggers; the lock file follows Claude Code's
//! `utils/cronTasksLock.ts`.
//!
//! The same Chat can be resumed in several processes, so its Tick Subscriptions and Artifact
//! feedback may reach several processes at once. "Within a Chat, only one process delivers at any
//! moment, and nothing is delivered after `/stop` pauses receiving" is a safety constraint: the
//! single entry point of the wake-up base acquires the lease before each delivery, and only the
//! process holding the lease hands the input to the Loop.
//!
//! **Lease**: before delivery, acquire `<Chat directory>/trigger/lease.lock`. The lock file is
//! created exclusively (write a temporary file, then hard-link it), and its content
//! `{chatId, pid, nonce, startedAt, host}` is only for diagnostics; ownership is decided by
//! `flock(LOCK_EX | LOCK_NB)` on the file, and [`ChatLease`] keeps the file descriptor open while
//! held. When the process exits (including by SIGKILL) the kernel releases the flock, and PID
//! liveness is never checked, so there is no window between "confirm the holder exited" and
//! deletion, and PID reuse has no effect. The flock belongs to the open file description: two Agent
//! instances in the same process each open the file and exclude each other just the same; files
//! opened by Rust are close-on-exec, so child processes started by tools do not inherit the lock.
//!
//! On release, the lock file is deleted while the flock is still held, then the descriptor is
//! closed. After obtaining the flock, an acquirer confirms that the lock path still points to the
//! file it opened (same device and inode): if it opened an old file that the holder then deleted,
//! the path is gone or now names a new file, and it drops this descriptor and reopens. Only the
//! side holding the flock deletes, so it never deletes a lock held by someone else. When the host
//! recorded in the lock differs from this machine, takeover is refused and reported: the lock
//! directory may be on a file system shared by several machines, where flock is not guaranteed to
//! work across hosts.
//!
//! **Receiving state**: `trigger/receiving.state` records
//! `{generation, receiving, updatedAt, pid}`, replaced atomically via a temporary file plus rename,
//! with read-modify-write done under the flock of `trigger/.receiving.lock`. Both pausing and
//! resuming receiving increment the generation. The lock holder reads it before each delivery: if
//! receiving is paused and the generation is newer than this process knows, it syncs the local
//! receiving switch (pause receiving, stop Artifact listening, release the lease) and does not
//! deliver. The old approach of only deleting the lock file would let another instance take over
//! and keep delivering.
//!
//! **Takeover**: each time the lease is newly acquired, the Chat's Trigger state (Trigger records
//! in the ledger, Trigger input awaiting execution, sequence numbers already delivered by Ticks) is
//! first rebuilt entirely from storage, and until that finishes every delivery and take for the
//! same Chat waits on the same barrier; if the rebuild fails nothing is delivered, and the next
//! delivery retries. What the previous holder delivered, executed or settled during its lease thus
//! matches what a clean load sees. User input and Agent messages need no lease; they belong to
//! their own processes and do not change on takeover.
//!
//! Unlike Claude Code: on resume, Claude Code rewrites the owner to itself whenever the sessionId
//! matches; two Tickloop processes can continue the same Chat at the same time, so the holder is
//! decided by flock. Chats of non-persisting Agents are never resumed by other processes and use
//! neither the lease nor the receiving state.

use std::collections::HashMap;
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{self, Read, Write};
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::Agent;
use tickloop_harness_base::private_fs::{
    PRIVATE_FILE_MODE, PrivateDirectory, replace_file_atomically,
};
use tickloop_harness_protocol::{
    ChatId, LoopTriggerNotice, LoopTriggerNoticeKind, PauseReason, Receiving,
};

const LEASE_DIRECTORY: &str = "trigger";
const LEASE_FILE: &str = "lease.lock";
const STATE_FILE: &str = "receiving.state";
const STATE_WRITE_LOCK_FILE: &str = ".receiving.lock";
/// How long a write of the receiving switch waits for another process's write before it tries
/// again.
const RECEIVING_WRITE_RETRY: std::time::Duration = std::time::Duration::from_millis(10);
/// Number of reopens when the opened lock file happens to have been deleted or replaced by the
/// holder; beyond it, the lock is treated as held by another process.
const LOCK_OPEN_ATTEMPTS: usize = 3;
/// How long a source waits to enter the Chat's Trigger boundary before it hears the Chat is
/// busy: a take holds the boundary while the record of its start is retried.
const SOURCE_BOUNDARY_WAIT: std::time::Duration = std::time::Duration::from_secs(5);
const HELD_ELSEWHERE_MESSAGE: &str =
    "another Tickloop process holds this Chat's Trigger; the event was not delivered";

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct LeaseHolder {
    chat_id: ChatId,
    pid: u32,
    nonce: Uuid,
    started_at: u64,
    host: String,
}

/// Only the host matters when reading the lock content; old-format or corrupt content has no host
/// and is treated as this machine.
#[derive(Deserialize)]
struct RecordedHost {
    host: Option<String>,
}

/// A lease held by this process: the lock file's descriptor and flock are kept while held. Released
/// on drop.
pub(super) struct ChatLease {
    path: PathBuf,
    file: File,
}

impl ChatLease {
    fn is_still_at_path(&self) -> bool {
        refers_to_same_file(&self.file, &self.path).unwrap_or(false)
    }
}

impl Drop for ChatLease {
    fn drop(&mut self) {
        // Delete while still holding the flock; `file` is closed afterwards and the kernel releases
        // the flock.
        if self.is_still_at_path() {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReceivingRecord {
    generation: u64,
    receiving: bool,
    updated_at: u64,
    pid: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ReceivingChange {
    Resumed,
    Paused,
}

enum LeaseAttempt {
    Acquired(ChatLease),
    HeldElsewhere,
    ForeignHost(String),
}

/// Conclusion from reading the receiving state after holding the lease.
enum LeaseCheck {
    /// Held as the holding `epoch`; until `synced`, this process has not taken over what another
    /// process delivered to the Chat while it held the lease.
    Held {
        epoch: u64,
        synced: bool,
    },
    HeldElsewhere,
    ForeignHost(String),
    /// Held, while another process recorded a pause newer than this one knows.
    PausedElsewhere {
        generation: u64,
        epoch: u64,
        synced: bool,
    },
    /// The Agent is shutting down or handing over: no lease is taken any more.
    Closing,
}

/// Decision before one delivery.
pub(super) enum TriggerPermit {
    /// Deliver under `holding`, which the acceptance or the start checks again under the Chats
    /// lock: a lease released meanwhile no longer stands behind it.
    Fire(LeaseHolding),
    /// Cannot deliver for now (held by another process, lock from another host, Loop not
    /// receiving); retry later. `recoverable` while the Loop receives and only the lease keeps it
    /// out, so waiting input is retried.
    Wait {
        reason: String,
        recoverable: bool,
    },
    /// The receiving state shows receiving was paused in another process; the local state has been
    /// synced.
    Withdrawn,
    Failed(String),
}

/// What taking a Chat's lease came to.
pub(super) enum LeaseTaken {
    /// Held, the Chat's Trigger state taken over.
    Held(LeaseHolding),
    /// Held, while another process paused receiving: this one pauses too.
    PausedElsewhere(LeaseHolding),
}

/// Why a Chat's lease could not be taken.
pub(super) enum LeaseRefused {
    NotLoaded,
    /// Another live process holds it.
    HeldElsewhere,
    /// A lock written on another host holds it.
    ForeignHost(String),
    Closing,
    /// The lease files, or the Transcript the takeover reads, could not be used.
    Failed(String),
}

/// The lease holding a permit was given under; `None` where a host needs no lease, as one that
/// keeps waits durably or saves no Chats.
#[derive(Clone, Copy, Debug)]
pub(super) struct LeaseHolding(Option<u64>);

impl LeaseHolding {
    /// Whether another process may take the lease once it goes back: what must reach the store
    /// before it does waits for the store.
    pub(super) fn passes_on(self) -> bool {
        self.0.is_some()
    }
}

/// How long a Chat whose Trigger input waits on a failed permit waits before the first retry,
/// doubling with each failure up to the longest.
const PERMIT_RETRY_FIRST: std::time::Duration = std::time::Duration::from_secs(1);
const PERMIT_RETRY_LONGEST: std::time::Duration = std::time::Duration::from_secs(60);

/// The retry of a Chat's waiting Trigger input after failed permits: how many failed in a row,
/// and the scheduled retry, if one is, by its id.
#[derive(Default)]
struct PermitRetry {
    failures: u32,
    scheduled: Option<(u64, tokio_util::sync::CancellationToken)>,
}

/// The Chat's one serialization boundary for Triggers, held while it is entered. Taking or
/// releasing the lease, taking the Chat's Trigger state over, recording the receiving switch,
/// accepting a delivery and starting queued Trigger input, with the start recorded durably,
/// each happen whole within it.
pub(super) type TriggerBoundary = tokio::sync::OwnedMutexGuard<()>;

/// One holding of a Chat's lease. Taken, it is not synced: no Trigger is accepted or started
/// under it until the Chat's Trigger state is taken over from the store.
struct HeldLease {
    lease: ChatLease,
    epoch: u64,
    synced: bool,
}

/// All leases held by this process, the known receiving-state generations and pending notices, all
/// per Chat.
#[derive(Default)]
pub(super) struct ChatLeases {
    held: Mutex<HashMap<ChatId, HeldLease>>,
    /// Set as the Agent shuts down, under `held`: a check after it takes no lease, so none is
    /// taken after the leases are released.
    closing: std::sync::atomic::AtomicBool,
    epochs: std::sync::atomic::AtomicU64,
    boundaries: Mutex<HashMap<ChatId, Arc<tokio::sync::Mutex<()>>>>,
    retry_ids: std::sync::atomic::AtomicU64,
    known_generations: Mutex<HashMap<ChatId, u64>>,
    notices: Mutex<HashMap<ChatId, LoopTriggerNotice>>,
    retries: Mutex<HashMap<ChatId, PermitRetry>>,
    /// How many wait to enter each Chat's boundary, for tests to know a waiter is in place.
    #[cfg(any(test, feature = "test-support"))]
    boundary_waiters: Arc<Mutex<HashMap<ChatId, usize>>>,
}

impl ChatLeases {
    /// Ensures the lease is held and reads the receiving state. Acquisition completes while holding
    /// the same `held` lock, so concurrent deliveries within this process are not misjudged as held
    /// by another process because of our own flock.
    fn check(&self, directory: &PrivateDirectory, chat_id: ChatId) -> io::Result<LeaseCheck> {
        let key = chat_id;
        // The holding is taken out of `held` while its files are looked at, so no file work is
        // done under the lock every Chat shares; the Chat's boundary keeps anyone else of this
        // process from its holding meanwhile, and a shutdown that closes meanwhile finds it
        // closing below.
        let taken = {
            let mut held = self.held.lock().unwrap();
            if self.closing.load(std::sync::atomic::Ordering::SeqCst) {
                return Ok(LeaseCheck::Closing);
            }
            held.remove(&key)
        };
        // When the lock file was deleted or replaced externally, another process may already hold
        // the new file: drop the old descriptor and acquire again.
        let holding = match taken.filter(|holding| holding.lease.is_still_at_path()) {
            Some(holding) => holding,
            None => match try_acquire(directory, chat_id)? {
                LeaseAttempt::Acquired(lease) => HeldLease {
                    lease,
                    epoch: self
                        .epochs
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                    synced: false,
                },
                LeaseAttempt::HeldElsewhere => return Ok(LeaseCheck::HeldElsewhere),
                LeaseAttempt::ForeignHost(host) => return Ok(LeaseCheck::ForeignHost(host)),
            },
        };
        let (epoch, synced) = (holding.epoch, holding.synced);
        {
            let mut held = self.held.lock().unwrap();
            if self.closing.load(std::sync::atomic::Ordering::SeqCst) {
                drop(held);
                drop(holding);
                return Ok(LeaseCheck::Closing);
            }
            held.insert(key, holding);
        }
        let known = self.known_generation(key);
        Ok(match read_receiving(directory)? {
            Some(state) if !state.receiving && state.generation > known => {
                LeaseCheck::PausedElsewhere {
                    generation: state.generation,
                    epoch,
                    synced,
                }
            }
            Some(_) | None => LeaseCheck::Held { epoch, synced },
        })
    }

    fn is_synced(&self, chat_id: ChatId, epoch: u64) -> bool {
        self.held
            .lock()
            .unwrap()
            .get(&chat_id)
            .is_some_and(|holding| holding.epoch == epoch && holding.synced)
    }

    /// The holding `epoch` took the Chat's Trigger state over, and a retry waiting for it is no
    /// longer needed. A later holding takes it over again.
    fn mark_synced(&self, chat_id: ChatId, epoch: u64) {
        if let Some(holding) = self
            .held
            .lock()
            .unwrap()
            .get_mut(&chat_id)
            .filter(|holding| holding.epoch == epoch)
        {
            holding.synced = true;
        }
        self.cancel_retry(chat_id);
    }

    /// The holding this process has of the Chat's lease, synced or not.
    pub(super) fn holding(&self, chat_id: ChatId) -> Option<LeaseHolding> {
        self.held
            .lock()
            .unwrap()
            .get(&chat_id)
            .map(|holding| LeaseHolding(Some(holding.epoch)))
    }

    /// Whether a permit given under `holding` still stands: the lease it was given under is still
    /// held and synced.
    pub(super) fn stands(&self, chat_id: ChatId, holding: LeaseHolding) -> bool {
        match holding.0 {
            None => true,
            Some(epoch) => self.is_synced(chat_id, epoch),
        }
    }

    /// Schedules the retry of the Chat's waiting Trigger input after a failed permit, unless one
    /// is scheduled: its delay and cancellation.
    fn schedule_retry(
        &self,
        chat_id: ChatId,
    ) -> Option<(
        u64,
        std::time::Duration,
        tokio_util::sync::CancellationToken,
    )> {
        let mut retries = self.retries.lock().unwrap();
        let retry = retries.entry(chat_id).or_default();
        if retry.scheduled.is_some() {
            return None;
        }
        let delay = PERMIT_RETRY_FIRST
            .saturating_mul(2u32.saturating_pow(retry.failures))
            .min(PERMIT_RETRY_LONGEST);
        retry.failures = retry.failures.saturating_add(1);
        let id = self
            .retry_ids
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let cancel = tokio_util::sync::CancellationToken::new();
        retry.scheduled = Some((id, cancel.clone()));
        Some((id, delay, cancel))
    }

    /// The retry `id` came due: it is no longer scheduled, unless another took its place.
    fn retry_came_due(&self, chat_id: ChatId, id: u64) {
        if let Some(retry) = self.retries.lock().unwrap().get_mut(&chat_id)
            && retry
                .scheduled
                .as_ref()
                .is_some_and(|(scheduled, _)| *scheduled == id)
        {
            retry.scheduled = None;
        }
    }

    fn cancel_retry(&self, chat_id: ChatId) {
        if let Some(retry) = self.retries.lock().unwrap().remove(&chat_id)
            && let Some((_, scheduled)) = retry.scheduled
        {
            scheduled.cancel();
        }
    }

    fn boundary(&self, chat_id: ChatId) -> Arc<tokio::sync::Mutex<()>> {
        self.boundaries
            .lock()
            .unwrap()
            .entry(chat_id)
            .or_default()
            .clone()
    }

    fn known_generation(&self, key: ChatId) -> u64 {
        self.known_generations
            .lock()
            .unwrap()
            .get(&key)
            .copied()
            .unwrap_or(0)
    }

    fn remember_generation(&self, key: ChatId, generation: u64) {
        let mut known = self.known_generations.lock().unwrap();
        let entry = known.entry(key).or_insert(0);
        *entry = (*entry).max(generation);
    }

    pub(super) fn set_notice(&self, chat_id: ChatId, kind: LoopTriggerNoticeKind, message: String) {
        self.notices.lock().unwrap().insert(
            chat_id,
            LoopTriggerNotice {
                chat_id,
                kind,
                message,
            },
        );
    }

    pub(super) fn clear_notice(&self, chat_id: ChatId) {
        self.notices.lock().unwrap().remove(&chat_id);
    }

    /// Whether this process holds the Chat's lease now and took over what its last holder left.
    pub(super) fn holds_synced(&self, chat_id: ChatId) -> bool {
        self.held
            .lock()
            .unwrap()
            .get(&chat_id)
            .is_some_and(|holding| holding.synced)
    }

    pub(super) fn release(&self, chat_id: ChatId) {
        let released = self.held.lock().unwrap().remove(&chat_id);
        drop(released);
        self.cancel_retry(chat_id);
        self.clear_notice(chat_id);
    }

    /// No lease is taken from now on; set under `held`, so a check in progress either put its
    /// lease back before, for `release_all` to release, or drops it.
    pub(super) fn close(&self) {
        let _held = self.held.lock().unwrap();
        self.closing
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub(super) fn release_all(&self) {
        let released = std::mem::take(&mut *self.held.lock().unwrap());
        drop(released);
        for (_, retry) in std::mem::take(&mut *self.retries.lock().unwrap()) {
            if let Some((_, scheduled)) = retry.scheduled {
                scheduled.cancel();
            }
        }
        self.notices.lock().unwrap().clear();
    }
}

/// Counts one waiter of a Chat's boundary while it waits, whether it gets in or is dropped.
#[cfg(any(test, feature = "test-support"))]
struct BoundaryWaiter {
    waiters: Arc<Mutex<HashMap<ChatId, usize>>>,
    chat_id: ChatId,
}

#[cfg(any(test, feature = "test-support"))]
impl BoundaryWaiter {
    fn new(waiters: &Arc<Mutex<HashMap<ChatId, usize>>>, chat_id: ChatId) -> Self {
        *waiters.lock().unwrap().entry(chat_id).or_default() += 1;
        Self {
            waiters: waiters.clone(),
            chat_id,
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
impl Drop for BoundaryWaiter {
    fn drop(&mut self) {
        if let Some(count) = self.waiters.lock().unwrap().get_mut(&self.chat_id) {
            *count -= 1;
        }
    }
}

/// Tries to acquire the lease: opens the lock file (creating it exclusively when absent) and tries
/// flock.
fn try_acquire(directory: &PrivateDirectory, chat_id: ChatId) -> io::Result<LeaseAttempt> {
    directory.create()?;
    let path = directory.path().join(LEASE_FILE);
    let holder = LeaseHolder {
        chat_id,
        pid: std::process::id(),
        nonce: tickloop_ids::generate(),
        started_at: tickloop_harness_base::private_fs::unix_time_milliseconds(),
        host: local_host_name(),
    };
    let content = serde_json::to_vec(&holder).map_err(io::Error::other)?;
    for _ in 0..LOCK_OPEN_ATTEMPTS {
        let mut file = match OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                create_exclusive(directory, &path, &content)?;
                continue;
            }
            Err(error) => return Err(error),
        };
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Ok(LeaseAttempt::HeldElsewhere),
            Err(TryLockError::Error(error)) => return Err(error),
        }
        if !refers_to_same_file(&file, &path)? {
            continue;
        }
        let mut recorded = Vec::new();
        file.read_to_end(&mut recorded)?;
        if let Some(host) = serde_json::from_slice::<RecordedHost>(&recorded)
            .ok()
            .and_then(|recorded| recorded.host)
            .filter(|host| *host != holder.host)
        {
            return Ok(LeaseAttempt::ForeignHost(host));
        }
        file.set_len(0)?;
        file.write_all_at(&content, 0)?;
        return Ok(LeaseAttempt::Acquired(ChatLease { path, file }));
    }
    Ok(LeaseAttempt::HeldElsewhere)
}

/// Writes a temporary file and then hard-links it to the lock path, so the diagnostic content is
/// complete when the lock file appears. When the path already exists, the caller opens it.
fn create_exclusive(directory: &PrivateDirectory, path: &Path, content: &[u8]) -> io::Result<()> {
    let temporary = unique_temporary_path(directory);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(PRIVATE_FILE_MODE)
        .open(&temporary)?;
    let linked = file
        .write_all(content)
        .and_then(|()| std::fs::hard_link(&temporary, path));
    let _ = std::fs::remove_file(&temporary);
    match linked {
        Err(error) if error.kind() != io::ErrorKind::AlreadyExists => Err(error),
        _ => Ok(()),
    }
}

fn unique_temporary_path(directory: &PrivateDirectory) -> PathBuf {
    directory.path().join(format!(
        ".{}.{}.tmp",
        std::process::id(),
        tickloop_ids::generate()
    ))
}

fn refers_to_same_file(file: &File, path: &Path) -> io::Result<bool> {
    let opened = file.metadata()?;
    match std::fs::symlink_metadata(path) {
        Ok(current) => Ok(current.dev() == opened.dev() && current.ino() == opened.ino()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn local_host_name() -> String {
    let mut buffer = [0u8; 256];
    // SAFETY: The buffer length matches the length passed; gethostname writes at most that length.
    let result = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) };
    if result != 0 {
        return String::new();
    }
    let length = buffer
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(buffer.len());
    String::from_utf8_lossy(&buffer[..length]).into_owned()
}

fn receiving_path(directory: &PrivateDirectory) -> PathBuf {
    directory.path().join(STATE_FILE)
}

fn read_receiving(directory: &PrivateDirectory) -> io::Result<Option<ReceivingRecord>> {
    let path = receiving_path(directory);
    match std::fs::read(&path) {
        Ok(content) => serde_json::from_slice(&content).map(Some).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} is unreadable: {error}", path.display()),
            )
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Reads, increments and atomically replaces the receiving state under the state write lock,
/// returning the state in the file after writing.
fn open_receiving_write_lock(directory: &PrivateDirectory) -> io::Result<File> {
    directory.create()?;
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(PRIVATE_FILE_MODE)
        .custom_flags(libc::O_NOFOLLOW)
        .open(directory.path().join(STATE_WRITE_LOCK_FILE))
}

/// Reads, increments and atomically replaces the receiving state under the state write lock,
/// returning the state in the file after writing; returns `None` when another process holds the
/// write lock, and the caller retries later.
fn write_receiving(
    directory: &PrivateDirectory,
    change: ReceivingChange,
) -> io::Result<Option<ReceivingRecord>> {
    let guard = open_receiving_write_lock(directory)?;
    match guard.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Ok(None),
        Err(TryLockError::Error(error)) => return Err(error),
    }
    let current = read_receiving(directory)?;
    let next = ReceivingRecord {
        generation: current.map_or(0, |current| current.generation) + 1,
        receiving: match change {
            ReceivingChange::Resumed => true,
            ReceivingChange::Paused => false,
        },
        updated_at: tickloop_harness_base::private_fs::unix_time_milliseconds(),
        pid: std::process::id(),
    };
    let contents = serde_json::to_vec(&next).map_err(io::Error::other)?;
    replace_file_atomically(&receiving_path(directory), &contents, PRIVATE_FILE_MODE)?;
    Ok(Some(next))
}

impl Agent {
    async fn lease_directory(&self, chat_id: ChatId) -> Option<PrivateDirectory> {
        self.inner
            .chats
            .lock()
            .await
            .get(&chat_id)
            .and_then(|chat| Some(chat.tools.chat_directory()?.join(LEASE_DIRECTORY)))
    }

    /// Enters the Chat's Trigger boundary, waiting while another holds it.
    pub(super) async fn enter_trigger_boundary(&self, chat_id: ChatId) -> TriggerBoundary {
        #[cfg(any(test, feature = "test-support"))]
        let _waiting = BoundaryWaiter::new(&self.inner.chat_leases.boundary_waiters, chat_id);
        self.inner.chat_leases.boundary(chat_id).lock_owned().await
    }

    /// Whether this process holds the Chat's lease and took the Chat over under it, so it alone
    /// writes the records of the Trigger sources' state. A host that keeps waits durably, or
    /// keeps no Transcript, always does until it closes.
    pub(super) fn holds_trigger_lease(&self, chat_id: ChatId) -> bool {
        let leases = &self.inner.chat_leases;
        if leases.closing.load(std::sync::atomic::Ordering::SeqCst) {
            return false;
        }
        self.inner.recorder.is_none()
            || self.inner.waits == super::WaitHosting::Durable
            || leases
                .holding(chat_id)
                .is_some_and(|holding| leases.stands(chat_id, holding))
    }

    /// Gives the Chat's lease back, unless a batch of its Transcript the store failed to take
    /// still waits to be appended again: appended after another process took the lease over, it
    /// would land after that process's records, as a source's stale state among them. The lease
    /// goes back with a later release once the batch is in, or with the process.
    pub(super) fn release_trigger_lease(&self, chat_id: ChatId) {
        if self.transcript_holds_failed_batch(chat_id) {
            return;
        }
        self.inner.chat_leases.release(chat_id);
    }

    /// Enters the Chat's Trigger boundary for a source, as long as it is free soon enough;
    /// `None` while it stays held.
    pub(super) async fn enter_trigger_boundary_for_source(
        &self,
        chat_id: ChatId,
    ) -> Option<TriggerBoundary> {
        tokio::time::timeout(SOURCE_BOUNDARY_WAIT, self.enter_trigger_boundary(chat_id))
            .await
            .ok()
    }

    /// How many wait to enter the Chat's Trigger boundary now.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn trigger_boundary_waiters_for_tests(&self, chat_id: ChatId) -> usize {
        self.inner
            .chat_leases
            .boundary_waiters
            .lock()
            .unwrap()
            .get(&chat_id)
            .copied()
            .unwrap_or(0)
    }

    /// Takes the Chat's lease within its boundary, whether the Loop receives or not: a holding
    /// newly taken takes the Chat's Trigger state over first.
    pub(super) async fn take_lease(
        &self,
        chat_id: ChatId,
        _within: &TriggerBoundary,
    ) -> Result<LeaseTaken, LeaseRefused> {
        if self
            .inner
            .chat_leases
            .closing
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(LeaseRefused::Closing);
        }
        // A host that keeps waits durably holds the Chat's whole family under its lease: no
        // other process delivers to it.
        if self.inner.recorder.is_none() || self.inner.waits == super::WaitHosting::Durable {
            return Ok(LeaseTaken::Held(LeaseHolding(None)));
        }
        let Some(directory) = self.lease_directory(chat_id).await else {
            return Err(LeaseRefused::NotLoaded);
        };
        #[cfg(any(test, feature = "test-support"))]
        self.pause_point_blocking(
            chat_id,
            super::wakeup::trigger_pauses::PausePoint::LeaseChecking,
        );
        // Done here, not on the blocking pool: a few small local files and a lock it only tries,
        // and nothing of it may outlive the boundary, as work handed to the pool would once its
        // waiter is dropped.
        let checked = self
            .inner
            .chat_leases
            .check(&directory, chat_id)
            .map_err(|error| error.to_string());
        let leases = &self.inner.chat_leases;
        let (epoch, synced, paused_elsewhere) = match checked {
            Err(error) => {
                let message = format!("could not take this Chat's Trigger lease: {error}");
                leases.set_notice(chat_id, LoopTriggerNoticeKind::Warning, message.clone());
                return Err(LeaseRefused::Failed(message));
            }
            Ok(LeaseCheck::HeldElsewhere) => {
                leases.set_notice(
                    chat_id,
                    LoopTriggerNoticeKind::HeldByAnotherProcess,
                    "another Tickloop process holds this Chat's Trigger".into(),
                );
                return Err(LeaseRefused::HeldElsewhere);
            }
            Ok(LeaseCheck::ForeignHost(host)) => {
                let message = format!(
                    "this Chat's Trigger lock was written on host {host}; the trigger directory \
                     may be shared between machines, so this process will not take it over. \
                     Remove the lock file if that host no longer runs Tickloop"
                );
                leases.set_notice(chat_id, LoopTriggerNoticeKind::Warning, message.clone());
                return Err(LeaseRefused::ForeignHost(message));
            }
            Ok(LeaseCheck::Closing) => return Err(LeaseRefused::Closing),
            Ok(LeaseCheck::PausedElsewhere {
                generation,
                epoch,
                synced,
            }) => {
                leases.remember_generation(chat_id, generation);
                (epoch, synced, true)
            }
            Ok(LeaseCheck::Held { epoch, synced }) => (epoch, synced, false),
        };
        if !synced {
            if let Err(error) = self.reload_trigger_state(chat_id).await {
                let message = format!(
                    "could not take over this Chat's Trigger state from its Transcript: {error}; \
                     no Trigger is delivered until it can"
                );
                leases.set_notice(chat_id, LoopTriggerNoticeKind::Warning, message.clone());
                return Err(LeaseRefused::Failed(message));
            }
            leases.mark_synced(chat_id, epoch);
        }
        let holding = LeaseHolding(Some(epoch));
        Ok(if paused_elsewhere {
            LeaseTaken::PausedElsewhere(holding)
        } else {
            LeaseTaken::Held(holding)
        })
    }

    /// Called before each delivery, within the Chat's Trigger boundary: holds the Chat's lease,
    /// reads the receiving state, and confirms the Loop is still receiving. When the receiving
    /// state shows receiving was paused in another process, syncs the local receiving switch and
    /// returns `Withdrawn`.
    pub(super) async fn acquire_trigger_permit(
        &self,
        chat_id: ChatId,
        within: &TriggerBoundary,
    ) -> TriggerPermit {
        let holding = match self.take_lease(chat_id, within).await {
            Ok(LeaseTaken::Held(holding)) => holding,
            Ok(LeaseTaken::PausedElsewhere(_)) => {
                self.pause_receiving_paused_elsewhere(chat_id).await;
                return TriggerPermit::Withdrawn;
            }
            Err(LeaseRefused::NotLoaded) => {
                return TriggerPermit::Wait {
                    reason: "chat is no longer loaded".into(),
                    recoverable: false,
                };
            }
            Err(LeaseRefused::HeldElsewhere) => {
                return TriggerPermit::Wait {
                    reason: HELD_ELSEWHERE_MESSAGE.into(),
                    recoverable: true,
                };
            }
            Err(LeaseRefused::ForeignHost(message)) => {
                return TriggerPermit::Wait {
                    reason: message,
                    recoverable: true,
                };
            }
            Err(LeaseRefused::Closing) => {
                return TriggerPermit::Wait {
                    reason: "this Agent is shutting down".into(),
                    recoverable: false,
                };
            }
            Err(LeaseRefused::Failed(message)) => return TriggerPermit::Failed(message),
        };
        if holding.0.is_none() {
            return TriggerPermit::Fire(holding);
        }
        let leases = &self.inner.chat_leases;
        let receiving = self
            .inner
            .chats
            .lock()
            .await
            .get(&chat_id)
            .map(|chat| chat.snapshot.chat_loop.receiving);
        match receiving {
            Some(Receiving::Receiving) => {}
            // The process keeps the lease while its Transcript cannot take a start: no other
            // process runs what it may have started.
            Some(Receiving::Paused {
                reason: PauseReason::TranscriptUnavailable,
            }) => {
                return TriggerPermit::Wait {
                    reason: "this Chat's Loop paused: its Transcript cannot be written".into(),
                    recoverable: false,
                };
            }
            Some(Receiving::Paused {
                reason: PauseReason::Stopped | PauseReason::HistoryLoaded,
            })
            | None => {
                self.release_trigger_lease(chat_id);
                return TriggerPermit::Wait {
                    reason: "this Chat's Loop is not receiving".into(),
                    recoverable: false,
                };
            }
        }
        leases.clear_notice(chat_id);
        TriggerPermit::Fire(holding)
    }

    /// A permit for the Chat's waiting Trigger input failed: it is tried again after a delay
    /// that doubles with each failure in a row, once at a time, until a takeover succeeds, the
    /// lease is released as receiving pauses, or the Agent shuts down.
    pub(super) fn retry_waiting_trigger_input(&self, chat_id: ChatId) {
        let Some((id, delay, cancel)) = self.inner.chat_leases.schedule_retry(chat_id) else {
            return;
        };
        self.inner
            .tasks
            .spawn(self.clone().retry_after(chat_id, id, delay, cancel));
    }

    /// The retry's future is named, not inferred: the run it starts asks for a permit, whose
    /// failure schedules this again.
    fn retry_after(
        self,
        chat_id: ChatId,
        id: u64,
        delay: std::time::Duration,
        cancel: tokio_util::sync::CancellationToken,
    ) -> futures_util::future::BoxFuture<'static, ()> {
        Box::pin(async move {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = self.inner.shutdown.cancelled() => return,
                _ = tokio::time::sleep(delay) => {}
            }
            self.inner.chat_leases.retry_came_due(chat_id, id);
            let _ = self.start_next_agent_run(chat_id).await;
        })
    }

    /// Records a change of the receiving switch so other processes see it before their next
    /// delivery. A pause should be written before releasing the lease, so the process taking over
    /// does not read the old state before the write. On failure, a notice is shown after the lease
    /// is released, without affecting the local operation. Within the Chat's Trigger boundary, as
    /// the end of the operation that turned the switch here.
    pub(super) async fn record_receiving(
        &self,
        chat_id: ChatId,
        change: ReceivingChange,
        within: TriggerBoundary,
    ) {
        let recorded = match self.lease_directory(chat_id).await {
            Some(directory) if self.inner.recorder.is_some() => self
                .write_receiving_when_free(&directory, change)
                .await
                .map(|state| {
                    self.inner
                        .chat_leases
                        .remember_generation(chat_id, state.generation)
                }),
            Some(_) | None => Ok(()),
        };
        #[cfg(any(test, feature = "test-support"))]
        self.pause_point(
            chat_id,
            super::wakeup::trigger_pauses::PausePoint::ReceivingRecorded,
        )
        .await;
        match change {
            ReceivingChange::Paused => {
                // The sources stop with the Loop where the lease is, before it goes back.
                let refused = match self.take_lease(chat_id, &within).await {
                    Ok(LeaseTaken::Held(holding) | LeaseTaken::PausedElsewhere(holding)) => {
                        self.switch_sources(chat_id, holding, change).await;
                        None
                    }
                    Err(refused) => Some(refused),
                };
                self.release_trigger_lease(chat_id);
                if let Some(refused) = refused
                    && self.has_trigger_source_states()
                {
                    self.inner.chat_leases.set_notice(
                        chat_id,
                        LoopTriggerNoticeKind::Warning,
                        format!(
                            "this Chat's Trigger sources, such as its goal or scheduled tasks, \
                             did not stop here: {}; they stop where the lease is held once that \
                             process learns of the stop",
                            super::wakeup::SourceChangeError::from(refused)
                        ),
                    );
                }
            }
            ReceivingChange::Resumed => {
                // The sources resume with the Loop under the lease. One taken for that alone
                // goes back once they are recorded, for the first delivery to take as before.
                let held_before = self.inner.chat_leases.holding(chat_id).is_some();
                if self.has_trigger_source_states()
                    && let Ok(LeaseTaken::Held(holding)) = self.take_lease(chat_id, &within).await
                    && self.switch_sources(chat_id, holding, change).await
                    && !held_before
                {
                    self.release_trigger_lease(chat_id);
                }
            }
        }
        if let Err(error) = recorded {
            self.inner.chat_leases.set_notice(
                chat_id,
                LoopTriggerNoticeKind::Warning,
                format!(
                    "could not record this Chat's receiving state ({error}); other Tickloop \
                     processes may not see the change"
                ),
            );
        }
    }

    /// Writes the receiving switch once no other process holds its write lock, waiting between
    /// tries without holding a thread; a shutdown ends the wait unwritten.
    async fn write_receiving_when_free(
        &self,
        directory: &PrivateDirectory,
        change: ReceivingChange,
    ) -> Result<ReceivingRecord, String> {
        loop {
            match write_receiving(directory, change) {
                Ok(Some(state)) => return Ok(state),
                Ok(None) => {}
                Err(error) => return Err(error.to_string()),
            }
            tokio::select! {
                _ = self.inner.shutdown.cancelled() => {
                    return Err("the Agent shut down before it could be written".into());
                }
                _ = tokio::time::sleep(RECEIVING_WRITE_RETRY) => {}
            }
        }
    }

    /// Trigger lease notices to show the user, ordered by Chat.
    pub fn loop_trigger_notices(&self) -> Vec<LoopTriggerNotice> {
        let mut notices = self
            .inner
            .chat_leases
            .notices
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect::<Vec<_>>();
        notices.sort_by_key(|notice| notice.chat_id);
        notices
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_retry_is_cancelled_when_a_takeover_succeeds_and_only_its_own_end_clears_it() {
        let leases = ChatLeases::default();
        let chat_id = Uuid::new_v4();
        let (first, _, first_cancel) = leases.schedule_retry(chat_id.into()).unwrap();
        assert!(
            leases.schedule_retry(chat_id.into()).is_none(),
            "one retry at a time"
        );
        leases.mark_synced(chat_id.into(), 0);
        assert!(first_cancel.is_cancelled());
        let (_, _, second_cancel) = leases.schedule_retry(chat_id.into()).unwrap();
        leases.retry_came_due(chat_id.into(), first);
        assert!(
            leases.schedule_retry(chat_id.into()).is_none(),
            "a retry that came due late does not clear the one scheduled after it"
        );
        leases.release(chat_id.into());
        assert!(second_cancel.is_cancelled());
    }

    fn lease_directory() -> (tempfile::TempDir, PrivateDirectory) {
        let root = tempfile::tempdir().unwrap();
        let directory = PrivateDirectory::trusted(root.path().join(LEASE_DIRECTORY));
        (root, directory)
    }

    fn acquired(attempt: LeaseAttempt) -> ChatLease {
        match attempt {
            LeaseAttempt::Acquired(lease) => lease,
            LeaseAttempt::HeldElsewhere => panic!("the lease is held elsewhere"),
            LeaseAttempt::ForeignHost(host) => panic!("the lease belongs to {host}"),
        }
    }

    #[test]
    fn separate_opens_in_one_process_exclude_each_other_until_release() {
        let (_root, directory) = lease_directory();
        let chat_id = Uuid::new_v4();
        let lease = acquired(try_acquire(&directory, chat_id.into()).unwrap());
        assert!(matches!(
            try_acquire(&directory, chat_id.into()).unwrap(),
            LeaseAttempt::HeldElsewhere
        ));
        assert!(lease.path.exists());
        let path = lease.path.clone();
        drop(lease);
        assert!(!path.exists());
        let again = acquired(try_acquire(&directory, chat_id.into()).unwrap());
        assert!(again.path.exists());
    }

    #[test]
    fn a_descriptor_opened_before_the_holder_deleted_the_lock_is_not_a_lease() {
        let (_root, directory) = lease_directory();
        let chat_id = Uuid::new_v4();
        let lease = acquired(try_acquire(&directory, chat_id.into()).unwrap());
        let stale = File::open(&lease.path).unwrap();
        let path = lease.path.clone();
        drop(lease);
        // The flock on the old descriptor can be taken, but the path no longer points to it; the
        // new acquirer creates a new file.
        stale.try_lock().unwrap();
        assert!(!refers_to_same_file(&stale, &path).unwrap());
        let current = acquired(try_acquire(&directory, chat_id.into()).unwrap());
        assert!(refers_to_same_file(&current.file, &path).unwrap());
    }

    #[test]
    fn a_failed_attempt_leaves_the_holders_lock_in_place() {
        let (_root, directory) = lease_directory();
        let chat_id = Uuid::new_v4();
        let lease = acquired(try_acquire(&directory, chat_id.into()).unwrap());
        for _ in 0..10 {
            assert!(matches!(
                try_acquire(&directory, chat_id.into()).unwrap(),
                LeaseAttempt::HeldElsewhere
            ));
        }
        assert!(lease.is_still_at_path());
        let content: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&lease.path).unwrap()).unwrap();
        assert_eq!(content["pid"], std::process::id());
        assert_eq!(content["host"], local_host_name());
    }

    #[test]
    fn every_change_increases_the_generation() {
        let (_root, directory) = lease_directory();
        let resumed = write_receiving(&directory, ReceivingChange::Resumed)
            .unwrap()
            .unwrap();
        let paused = write_receiving(&directory, ReceivingChange::Paused)
            .unwrap()
            .unwrap();
        assert!(resumed.receiving && !paused.receiving);
        assert_eq!(paused.generation, resumed.generation + 1);
        let again = write_receiving(&directory, ReceivingChange::Resumed)
            .unwrap()
            .unwrap();
        assert!(again.receiving);
        assert_eq!(again.generation, paused.generation + 1);
        assert!(read_receiving(&directory).unwrap().unwrap().receiving);
    }

    #[test]
    fn a_pause_recorded_after_the_known_generation_withdraws_the_holder() {
        let (_root, directory) = lease_directory();
        let leases = ChatLeases::default();
        let chat_id = Uuid::new_v4();
        let resumed = write_receiving(&directory, ReceivingChange::Resumed)
            .unwrap()
            .unwrap();
        leases.remember_generation(chat_id.into(), resumed.generation);
        assert!(matches!(
            leases.check(&directory, chat_id.into()).unwrap(),
            LeaseCheck::Held { .. }
        ));
        write_receiving(&directory, ReceivingChange::Paused)
            .unwrap()
            .unwrap();
        assert!(matches!(
            leases.check(&directory, chat_id.into()).unwrap(),
            LeaseCheck::PausedElsewhere { generation, .. } if generation == resumed.generation + 1
        ));
    }

    #[test]
    fn a_lock_deleted_under_the_holder_is_reacquired_on_the_next_check() {
        let (_root, directory) = lease_directory();
        let leases = ChatLeases::default();
        let chat_id = Uuid::new_v4();
        assert!(matches!(
            leases.check(&directory, chat_id.into()).unwrap(),
            LeaseCheck::Held { .. }
        ));
        let path = directory.path().join(LEASE_FILE);
        std::fs::remove_file(&path).unwrap();
        let intruder = acquired(try_acquire(&directory, chat_id.into()).unwrap());
        assert!(matches!(
            leases.check(&directory, chat_id.into()).unwrap(),
            LeaseCheck::HeldElsewhere
        ));
        drop(intruder);
        assert!(matches!(
            leases.check(&directory, chat_id.into()).unwrap(),
            LeaseCheck::Held { .. }
        ));
    }
}

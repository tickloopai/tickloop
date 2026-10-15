//! The daemon's durable record of hosted Tick ownership. Each config root keeps its own
//! private `tickhost/` directory: a lease lock held for the daemon's life, and per Chat its owner
//! state and one file per launch record. Only the daemon writes it; the files outlive host
//! and daemon restarts so ownership, recovery and end events never depend on a live process.

use std::{
    collections::HashMap,
    fs::File,
    io,
    os::fd::AsRawFd,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::Arc,
};

use serde::{Deserialize, Serialize};
use tick_protocol::InstanceRef;
use tickhost::host::{Receipt, TickAdapter, TickOwner};
use tickhost_connect::host_error::HostError;
use tokio::sync::Mutex;
use uuid::Uuid;

use super::controller::{HostController, LIFECYCLE_TIMEOUT, OwnerListing};

use tickloop_agent_base::private_fs::{
    PRIVATE_FILE_MODE, create_private_directory, replace_file_atomically, unix_time_milliseconds,
    verify_private_directory,
};

const REGISTRY_DIRECTORY: &str = "tickhost";
const LEASE_FILE: &str = "registry.lock";
const CHATS_DIRECTORY: &str = "chats";
const OWNER_FILE: &str = "owner.json";
const MANIFEST_FILE: &str = "manifest.json";
const LAUNCH_PREFIX: &str = "launch-";
const RECORD_EXTENSION: &str = ".json";
/// The host's answer for an instance or owner it no longer runs.
pub const TARGET_GONE: &str = "TargetGone";
/// The most one registry record, or one owner or Watch message to the host, may serialize to.
/// Records are kept per Chat and per launch, so this never caps how many there are.
pub const SHARD_BYTES: usize = 1024 * 1024;
const RECORD_TOO_LARGE: &str = "RecordTooLarge";

/// Whether a Chat may own hosted Ticks. Only an explicit StopSubagent ends it.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum OwnerState {
    #[default]
    Active,
    /// StopSubagent stopped this child Chat; its host cleanup is not yet confirmed.
    Stopping { operation: Uuid },
    /// Only an explicit Continue lets this Chat own Ticks again.
    Stopped { operation: Uuid },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum LaunchState {
    /// Durably recorded before the host is asked; the outcome is not known yet.
    Opening,
    Live {
        instance: InstanceRef,
        host_epoch: Uuid,
    },
    /// The host that ran it is gone; only an explicit reopen can start a new instance.
    Lost {
        instance: InstanceRef,
        host_epoch: Uuid,
    },
    /// Lost with host `host_epoch`, then reopened as a new instance by operation `by`.
    Reopened {
        instance: InstanceRef,
        host_epoch: Uuid,
        by: Uuid,
    },
}

/// Which file a path names. Executables also compare their modification time; a directory's
/// changes with every entry, so a working directory compares device and inode only.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct FileIdentity {
    device: u64,
    inode: u64,
    modified: Option<i64>,
}

impl FileIdentity {
    pub fn executable(path: &Path) -> Result<Self, String> {
        Self::read(path, true)
    }

    pub fn directory(path: &Path) -> Result<Self, String> {
        Self::read(path, false)
    }

    fn read(path: &Path, with_modification: bool) -> Result<Self, String> {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::metadata(path).map_err(|error| error.to_string())?;
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            modified: with_modification.then(|| metadata.mtime()),
        })
    }
}

/// The program and working directory an open launched, so a reopen runs the same ones or none.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct LaunchIdentity {
    pub executable: FileIdentity,
    pub directory: FileIdentity,
}

impl LaunchIdentity {
    pub fn of(command: &[String], cwd: &Path) -> Result<Self, String> {
        let program = command.first().ok_or("the launch has no program")?;
        Ok(Self {
            executable: FileIdentity::executable(Path::new(program))?,
            directory: FileIdentity::directory(cwd)?,
        })
    }
}

/// What an explicit open asked for, kept so a lost instance can be reopened as a new one.
/// Credentials are never stored: a reopen takes the current launch environment.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct LaunchRecord {
    pub operation: Uuid,
    pub chat: Uuid,
    pub adapter: TickAdapter,
    pub command: Vec<String>,
    pub cwd: PathBuf,
    pub identity: LaunchIdentity,
    pub title: Option<String>,
    #[serde(flatten)]
    pub state: LaunchState,
    /// When the record was created or its state last changed.
    pub updated_ms: u64,
}

impl LaunchRecord {
    /// The record of an open about to be asked of the host.
    pub fn opening(
        operation: Uuid,
        chat: Uuid,
        adapter: TickAdapter,
        command: Vec<String>,
        cwd: PathBuf,
        title: Option<String>,
    ) -> Result<Self, HostError> {
        let identity = LaunchIdentity::of(&command, &cwd)
            .map_err(|error| HostError::before("LaunchFailed", error))?;
        Ok(Self {
            operation,
            chat,
            adapter,
            command,
            cwd,
            identity,
            title,
            state: LaunchState::Opening,
            updated_ms: 0,
        })
    }

    pub fn instance(&self) -> Option<&InstanceRef> {
        match &self.state {
            LaunchState::Opening => None,
            LaunchState::Live { instance, .. }
            | LaunchState::Lost { instance, .. }
            | LaunchState::Reopened { instance, .. } => Some(instance),
        }
    }
}

#[derive(Deserialize, Serialize)]
struct Manifest {
    revision: u64,
}

/// What a transition does with the record as it is now.
pub enum Transition {
    Keep,
    Write(Box<LaunchRecord>),
    Remove,
}

/// `transition` when the record is still as it was observed, else nothing.
fn settled_launch_transition(
    observed: LaunchRecord,
    current: LaunchRecord,
    transition: Transition,
) -> Transition {
    if current != observed {
        return Transition::Keep;
    }
    transition
}

/// The registries of every config root this daemon serves, opened on first use.
#[derive(Default)]
pub struct Registry {
    roots: Mutex<HashMap<PathBuf, Arc<RootRegistry>>>,
}

impl Registry {
    /// The registry of `config_root`, taking its lease on first use. A root whose lease another
    /// daemon holds is unavailable rather than shared.
    pub async fn root(&self, config_root: &Path) -> Result<Arc<RootRegistry>, HostError> {
        let config_root = config_root.to_path_buf();
        let canonical = tokio::task::spawn_blocking(move || config_root.canonicalize())
            .await
            .map_err(|error| failed(&error))?
            .map_err(|error| unavailable(format!("cannot resolve config root: {error}")))?;
        let mut roots = self.roots.lock().await;
        if let Some(root) = roots.get(&canonical) {
            return Ok(root.clone());
        }
        let opening = canonical.clone();
        let files = tokio::task::spawn_blocking(move || RegistryFiles::open(&opening))
            .await
            .map_err(|error| failed(&error))?
            .map_err(|error| {
                unavailable(format!("cannot open the hosted Tick registry: {error}"))
            })?;
        let root = Arc::new(RootRegistry {
            files: Arc::new(files),
            lifecycles: LifecycleLocks::default(),
        });
        roots.insert(canonical, root.clone());
        Ok(root)
    }
}

/// One config root's registry. Holding it holds the root's lease. Its files are read and
/// written on blocking threads, never on the daemon's async workers.
pub struct RootRegistry {
    files: Arc<RegistryFiles>,
    /// Per Chat: opens, reopens and ends take turns, so an end can never fall between an
    /// open's revision and its launch.
    lifecycles: LifecycleLocks,
}

/// Per Chat, its lifecycle lock. A Chat's entry lasts only while its claims are above zero.
type LifecycleLocks = std::sync::Mutex<HashMap<Uuid, ChatLifecycle>>;

/// The lock a Chat's lifecycle turns take, and how many callers hold or await it.
#[derive(Default)]
struct ChatLifecycle {
    lock: Arc<Mutex<()>>,
    claims: usize,
}

/// One caller's turn at a Chat's lifecycle, counted from when it starts waiting until it drops.
pub struct LifecycleTurn<'a> {
    locks: &'a LifecycleLocks,
    chat: Uuid,
    held: Option<tokio::sync::OwnedMutexGuard<()>>,
}

impl Drop for LifecycleTurn<'_> {
    fn drop(&mut self) {
        // The lock is released before the count drops, so an entry removed at zero is never
        // replaced by a new lock while someone still holds the old one.
        self.held.take();
        let mut locks = self
            .locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(lifecycle) = locks.get_mut(&self.chat) {
            lifecycle.claims -= 1;
            if lifecycle.claims == 0 {
                locks.remove(&self.chat);
            }
        }
    }
}

struct RegistryFiles {
    directory: PathBuf,
    _lease: File,
    writes: std::sync::Mutex<()>,
}

impl RegistryFiles {
    fn open(config_root: &Path) -> io::Result<Self> {
        let directory = config_root.join(REGISTRY_DIRECTORY);
        for private in [directory.clone(), directory.join(CHATS_DIRECTORY)] {
            create_private_directory(&private)?;
            verify_private_directory(&private, "hosted Tick registry directory")?;
        }
        let lease = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(PRIVATE_FILE_MODE)
            .custom_flags(libc::O_NOFOLLOW)
            .open(directory.join(LEASE_FILE))?;
        if unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "another Agent daemon holds this config root's hosted Tick registry",
            ));
        }
        Ok(Self {
            directory,
            _lease: lease,
            writes: std::sync::Mutex::new(()),
        })
    }

    fn written(&self) -> std::sync::MutexGuard<'_, ()> {
        self.writes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn chat_directory(&self, chat: Uuid) -> PathBuf {
        self.directory.join(CHATS_DIRECTORY).join(chat.to_string())
    }

    fn launch_path(&self, chat: Uuid, operation: Uuid) -> PathBuf {
        self.chat_directory(chat)
            .join(format!("{LAUNCH_PREFIX}{operation}{RECORD_EXTENSION}"))
    }

    fn write(&self, path: &Path, value: &impl Serialize) -> Result<(), HostError> {
        let bytes = serde_json::to_vec(value).map_err(|error| failed(&error))?;
        within_shard("the hosted Tick registry record", bytes.len())?;
        let parent = path.parent().expect("registry files live in a directory");
        create_private_directory(parent).map_err(|error| failed(&error))?;
        verify_private_directory(parent, "hosted Tick registry directory")
            .map_err(|error| failed(&error))?;
        replace_file_atomically(path, &bytes, PRIVATE_FILE_MODE).map_err(|error| failed(&error))
    }

    fn revision(&self) -> Result<u64, HostError> {
        match read_unlinked(&self.directory.join(MANIFEST_FILE))? {
            Some(bytes) => serde_json::from_slice::<Manifest>(&bytes)
                .map(|manifest| manifest.revision)
                .map_err(|error| failed(&error)),
            None => Ok(0),
        }
    }

    fn next_revision(&self) -> Result<u64, HostError> {
        let _written = self.written();
        let revision = self.revision()? + 1;
        self.write(&self.directory.join(MANIFEST_FILE), &Manifest { revision })?;
        Ok(revision)
    }

    fn owner_state(&self, chat: Uuid) -> Result<OwnerState, HostError> {
        let Some(directory) = self.existing_chat_directory(chat)? else {
            return Ok(OwnerState::Active);
        };
        match read_unlinked(&directory.join(OWNER_FILE))? {
            Some(bytes) => serde_json::from_slice(&bytes).map_err(|error| failed(&error)),
            None => Ok(OwnerState::Active),
        }
    }

    fn set_owner_state(&self, chat: Uuid, state: &OwnerState) -> Result<(), HostError> {
        let _written = self.written();
        self.write(&self.chat_directory(chat).join(OWNER_FILE), state)
    }

    fn record_launch(&self, mut record: LaunchRecord) -> Result<(), HostError> {
        let _written = self.written();
        record.updated_ms = unix_time_milliseconds();
        self.write(&self.launch_path(record.chat, record.operation), &record)
    }

    fn remove_launch(&self, chat: Uuid, operation: Uuid) -> Result<(), HostError> {
        let _written = self.written();
        if self.existing_chat_directory(chat)?.is_none() {
            return Ok(());
        }
        match std::fs::remove_file(self.launch_path(chat, operation)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(failed(&error)),
        }
    }

    /// Changes one record as it is now: the current record is read and the change applied and
    /// stored under the write guard, so a decision made on an earlier reading never overwrites
    /// or revives what happened meanwhile. Returns whether anything changed.
    fn transition_launch(
        &self,
        chat: Uuid,
        operation: Uuid,
        change: impl FnOnce(LaunchRecord) -> Transition,
    ) -> Result<bool, HostError> {
        let _written = self.written();
        let Some(current) = self
            .launches(chat)?
            .into_iter()
            .find(|record| record.operation == operation)
        else {
            return Ok(false);
        };
        let path = self.launch_path(chat, operation);
        let earlier_state = current.state.clone();
        match change(current) {
            Transition::Keep => Ok(false),
            Transition::Write(record) => {
                let mut record = *record;
                if record.state != earlier_state {
                    record.updated_ms = unix_time_milliseconds();
                }
                self.write(&path, &record).map(|()| true)
            }
            Transition::Remove => match std::fs::remove_file(&path) {
                Ok(()) => Ok(true),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
                Err(error) => Err(failed(&error)),
            },
        }
    }

    /// `chat`'s directory once it is verified as a private real directory; `None` while it does
    /// not exist.
    fn existing_chat_directory(&self, chat: Uuid) -> Result<Option<PathBuf>, HostError> {
        let directory = self.chat_directory(chat);
        match std::fs::symlink_metadata(&directory) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(failed(&error)),
            Ok(_) => verify_private_directory(&directory, "hosted Tick registry directory")
                .map(|()| Some(directory))
                .map_err(|error| failed(&error)),
        }
    }

    fn launches(&self, chat: Uuid) -> Result<Vec<LaunchRecord>, HostError> {
        let Some(directory) = self.existing_chat_directory(chat)? else {
            return Ok(Vec::new());
        };
        let entries = std::fs::read_dir(&directory).map_err(|error| failed(&error))?;
        let mut records = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| failed(&error))?;
            let name = entry.file_name();
            let Some(operation) = name
                .to_str()
                .and_then(|name| name.strip_prefix(LAUNCH_PREFIX))
                .and_then(|name| name.strip_suffix(RECORD_EXTENSION))
            else {
                continue;
            };
            // A record removed since the directory was listed is simply gone.
            let Some(bytes) = read_unlinked(&entry.path())? else {
                continue;
            };
            // A record this build cannot read, such as one written before a format change, says
            // nothing it can act on: it is dropped alone, and the Chat's other records stay.
            let record: LaunchRecord = match serde_json::from_slice(&bytes) {
                Ok(record) => record,
                Err(error) => {
                    eprintln!(
                        "tickloop: dropping unreadable hosted Tick record {}: {error}",
                        entry.path().display()
                    );
                    let _ = std::fs::remove_file(entry.path());
                    continue;
                }
            };
            // The file's place, not its content, says whose record it is.
            if record.chat != chat || record.operation.to_string() != operation {
                return Err(failed(&format!(
                    "launch record {} does not belong to Chat {chat}",
                    name.to_string_lossy()
                )));
            }
            records.push(record);
        }
        Ok(records)
    }

    fn chats(&self) -> Result<Vec<Uuid>, HostError> {
        let entries = match std::fs::read_dir(self.directory.join(CHATS_DIRECTORY)) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(failed(&error)),
        };
        let mut chats = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| failed(&error))?;
            // Only the name a Chat's directory is written under counts, so listing and access
            // always mean the same directory.
            if let Some(chat) = entry.file_name().to_str().and_then(|name| {
                name.parse::<Uuid>()
                    .ok()
                    .filter(|chat| chat.to_string() == name)
            }) {
                chats.push(chat);
            }
        }
        Ok(chats)
    }
}

impl RootRegistry {
    /// Holds `chat`'s lifecycle until the turn drops.
    pub async fn lifecycle(&self, chat: Uuid) -> LifecycleTurn<'_> {
        let lock = {
            let mut locks = self
                .lifecycles
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let lifecycle = locks.entry(chat).or_default();
            lifecycle.claims += 1;
            lifecycle.lock.clone()
        };
        // Counted before it waits, so a caller that gives up waiting still releases its claim.
        let mut turn = LifecycleTurn {
            locks: &self.lifecycles,
            chat,
            held: None,
        };
        turn.held = Some(lock.lock_owned().await);
        turn
    }

    async fn on_files<T: Send + 'static>(
        &self,
        work: impl FnOnce(&RegistryFiles) -> Result<T, HostError> + Send + 'static,
    ) -> Result<T, HostError> {
        let files = self.files.clone();
        tokio::task::spawn_blocking(move || work(&files))
            .await
            .map_err(|error| failed(&error))?
    }

    /// The root's registry revision: it advances with every owner end and snapshot, so the host
    /// can tell a later decision from an earlier one.
    pub async fn revision(&self) -> Result<u64, HostError> {
        self.on_files(RegistryFiles::revision).await
    }

    /// Advances and durably records the revision, returning the new one.
    pub async fn next_revision(&self) -> Result<u64, HostError> {
        self.on_files(RegistryFiles::next_revision).await
    }

    pub async fn owner_state(&self, chat: Uuid) -> Result<OwnerState, HostError> {
        self.on_files(move |files| files.owner_state(chat)).await
    }

    pub async fn set_owner_state(&self, chat: Uuid, state: &OwnerState) -> Result<(), HostError> {
        let state = state.clone();
        self.on_files(move |files| files.set_owner_state(chat, &state))
            .await
    }

    pub async fn record_launch(&self, record: &LaunchRecord) -> Result<(), HostError> {
        let record = record.clone();
        self.on_files(move |files| files.record_launch(record))
            .await
    }

    /// See [`RegistryFiles::transition_launch`].
    pub async fn transition_launch(
        &self,
        chat: Uuid,
        operation: Uuid,
        change: impl FnOnce(LaunchRecord) -> Transition + Send + 'static,
    ) -> Result<bool, HostError> {
        self.on_files(move |files| files.transition_launch(chat, operation, change))
            .await
    }

    pub async fn remove_launch(&self, chat: Uuid, operation: Uuid) -> Result<(), HostError> {
        self.on_files(move |files| files.remove_launch(chat, operation))
            .await
    }

    /// The launch records of one Chat, in no particular order.
    pub async fn launches(&self, chat: Uuid) -> Result<Vec<LaunchRecord>, HostError> {
        self.on_files(move |files| files.launches(chat)).await
    }

    /// Every Chat with a registry directory, whether or not it still has records.
    pub async fn chats(&self) -> Result<Vec<Uuid>, HostError> {
        self.on_files(RegistryFiles::chats).await
    }

    /// Reconciles this root with the host: records are settled against what the host runs,
    /// decided ends are carried out, and the host gets a complete snapshot of the Chats that
    /// still own Ticks, so it can reap any other. Runs before anything else is served for the
    /// root.
    pub async fn reconcile_with_host(
        &self,
        controller: &HostController,
        root: &str,
    ) -> Result<(), HostError> {
        let host_epoch = controller.host_epoch().await?;
        let mut owners = Vec::new();
        for chat in self.chats().await? {
            let owner = TickOwner {
                root: root.to_owned(),
                chat,
            };
            // A Chat whose records cannot be read keeps its Ticks and never holds up the others.
            if self
                .reconcile_chat(controller, &owner)
                .await
                .unwrap_or(true)
            {
                owners.push(chat);
            }
        }
        if host_epoch.is_none() {
            return Ok(());
        }
        let revision = self.next_revision().await?;
        controller.reconcile_owners(root, revision, &owners).await?;
        Ok(())
    }

    /// Settles one Chat's records and decided end; returns whether it still owns Ticks.
    async fn reconcile_chat(
        &self,
        controller: &HostController,
        owner: &TickOwner,
    ) -> Result<bool, HostError> {
        let chat = owner.chat;
        if !self.launches(chat).await?.is_empty() {
            let listing = controller.owner_listing(owner).await?;
            self.settle_launches(controller, owner, &listing).await?;
        }
        match self.owner_state(chat).await? {
            OwnerState::Active => Ok(true),
            OwnerState::Stopping { operation } => {
                self.end_stopped(controller, owner, operation).await?;
                Ok(false)
            }
            OwnerState::Stopped { .. } => Ok(false),
        }
    }

    /// Settles `owner`'s records against the host. A live instance its host still runs stays;
    /// one that host no longer runs, or whose host exited after all its Ticks ended, exited,
    /// idled out or was closed and leaves no record; one whose host died with it is Lost and
    /// can be reopened. An open whose answer was lost settles from its receipt once it can no
    /// longer be in progress. A record written after the listing began is newer than it and
    /// waits for the next one.
    pub async fn settle_launches(
        &self,
        controller: &HostController,
        owner: &TickOwner,
        listing: &OwnerListing,
    ) -> Result<(), HostError> {
        let chat = owner.chat;
        for record in self.launches(chat).await? {
            if record.updated_ms >= listing.started_ms {
                continue;
            }
            let mut next = record.clone();
            let transition = match &record.state {
                LaunchState::Opening => {
                    let age = unix_time_milliseconds().saturating_sub(record.updated_ms);
                    if age < LIFECYCLE_TIMEOUT.as_millis() as u64 {
                        continue;
                    }
                    match controller.receipt(record.operation, owner).await? {
                        Some(Receipt::Opened(opened)) => {
                            next.state = LaunchState::Live {
                                instance: opened.instance,
                                host_epoch: opened.terminal.epoch,
                            };
                            Transition::Write(Box::new(next))
                        }
                        Some(Receipt::Failed(_)) | None => Transition::Remove,
                        Some(_) => continue,
                    }
                }
                LaunchState::Live {
                    instance,
                    host_epoch,
                } => match &listing.running {
                    Some(running) if running.epoch == *host_epoch => {
                        if running.live.contains(instance) {
                            continue;
                        }
                        Transition::Remove
                    }
                    _ if controller.was_emptied(*host_epoch) => Transition::Remove,
                    _ => {
                        next.state = LaunchState::Lost {
                            instance: instance.clone(),
                            host_epoch: *host_epoch,
                        };
                        Transition::Write(Box::new(next))
                    }
                },
                LaunchState::Lost { .. } | LaunchState::Reopened { .. } => continue,
            };
            self.transition_launch(chat, record.operation, move |current| {
                settled_launch_transition(record, current, transition)
            })
            .await?;
        }
        Ok(())
    }

    /// Ends the Ticks of a child Chat StopSubagent stopped. The decision is durable before the host
    /// is asked, so a lost answer is carried out by the next reconcile. Idempotent.
    pub async fn end_stopped(
        &self,
        controller: &HostController,
        owner: &TickOwner,
        operation: Uuid,
    ) -> Result<(), HostError> {
        let _lifecycle = self.lifecycle(owner.chat).await;
        self.finish_stop(controller, owner, operation).await
    }

    /// Ends the Ticks of `owner`, stopped by `operation`, and forgets their records. The caller
    /// holds the Chat's lifecycle lock.
    pub async fn finish_stop(
        &self,
        controller: &HostController,
        owner: &TickOwner,
        operation: Uuid,
    ) -> Result<(), HostError> {
        self.set_owner_state(owner.chat, &OwnerState::Stopping { operation })
            .await?;
        controller
            .end_owner(owner, self.revision().await?, operation)
            .await?;
        for record in self.launches(owner.chat).await? {
            self.remove_launch(owner.chat, record.operation).await?;
        }
        self.set_owner_state(owner.chat, &OwnerState::Stopped { operation })
            .await
    }
}

/// A registry file's bytes, refusing a symbolic link in its place and a file over one record,
/// which is never read whole; `None` when it is missing.
fn read_unlinked(path: &Path) -> Result<Option<Vec<u8>>, HostError> {
    use std::io::Read;
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(failed(&error)),
    };
    let mut bytes = Vec::new();
    file.take(SHARD_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| failed(&error))?;
    within_shard(
        format_args!("registry file {}", path.display()),
        bytes.len(),
    )?;
    Ok(Some(bytes))
}

/// Refuses `what` when it takes `size` bytes, more than one record may.
pub(crate) fn within_shard(what: impl std::fmt::Display, size: usize) -> Result<(), HostError> {
    if size > SHARD_BYTES {
        return Err(HostError::before(
            RECORD_TOO_LARGE,
            format!("{what} takes more than the {SHARD_BYTES} bytes one record may hold"),
        ));
    }
    Ok(())
}

fn unavailable(message: String) -> HostError {
    HostError::before("RegistryUnavailable", message)
}

fn failed(error: &dyn std::fmt::Display) -> HostError {
    HostError::before("RegistryUnavailable", error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(chat: Uuid, state: LaunchState) -> LaunchRecord {
        let mut record = LaunchRecord::opening(
            Uuid::new_v4(),
            chat,
            TickAdapter::Terminal,
            vec!["/usr/bin/true".into()],
            "/".into(),
            Some("counter".into()),
        )
        .unwrap();
        record.state = state;
        record
    }

    #[tokio::test]
    async fn a_settlement_does_not_replace_a_newer_launch_lifecycle() {
        let directory = tempfile::tempdir().unwrap();
        let root = Registry::default().root(directory.path()).await.unwrap();
        let chat = Uuid::new_v4();
        let instance = InstanceRef::hosted(Uuid::new_v4());
        let host_epoch = Uuid::new_v4();
        let live = record(
            chat,
            LaunchState::Live {
                instance: instance.clone(),
                host_epoch,
            },
        );
        root.record_launch(&live).await.unwrap();
        let observed = root.launches(chat).await.unwrap().pop().unwrap();
        let reopened = LaunchState::Reopened {
            instance: instance.clone(),
            host_epoch,
            by: Uuid::new_v4(),
        };
        let committed = reopened.clone();
        root.transition_launch(chat, observed.operation, move |mut current| {
            current.state = committed;
            Transition::Write(Box::new(current))
        })
        .await
        .unwrap();
        let mut lost = observed.clone();
        lost.state = LaunchState::Lost {
            instance,
            host_epoch,
        };
        let changed = root
            .transition_launch(chat, observed.operation, move |current| {
                settled_launch_transition(observed, current, Transition::Write(Box::new(lost)))
            })
            .await
            .unwrap();
        assert!(
            !changed,
            "an old listing must not undo a new lifecycle transition"
        );
        assert_eq!(
            root.launches(chat).await.unwrap().pop().unwrap().state,
            reopened
        );
    }

    #[tokio::test]
    async fn records_survive_a_new_daemon_and_a_root_has_one_daemon_at_a_time() {
        let directory = tempfile::tempdir().unwrap();
        let chat = Uuid::new_v4();
        let instance = InstanceRef::hosted(Uuid::new_v4());
        let opened = record(
            chat,
            LaunchState::Live {
                instance: instance.clone(),
                host_epoch: Uuid::new_v4(),
            },
        );
        {
            let registry = Registry::default();
            let root = registry.root(directory.path()).await.unwrap();
            root.record_launch(&opened).await.unwrap();
            root.set_owner_state(chat, &OwnerState::Active)
                .await
                .unwrap();
            let second = Registry::default().root(directory.path()).await;
            assert_eq!(
                second.err().unwrap().code,
                "RegistryUnavailable",
                "a second daemon never writes the same root"
            );
        }
        let registry = Registry::default();
        let root = registry.root(directory.path()).await.unwrap();
        let launches = root.launches(chat).await.unwrap();
        assert_eq!(launches.len(), 1);
        assert_eq!(launches[0].instance(), Some(&instance));
        assert!(launches[0].updated_ms > 0);
        assert_eq!(root.chats().await.unwrap(), vec![chat]);
        assert_eq!(root.revision().await.unwrap(), 0);
        assert_eq!(root.next_revision().await.unwrap(), 1);
        assert_eq!(root.next_revision().await.unwrap(), 2);
        root.remove_launch(chat, opened.operation).await.unwrap();
        assert!(root.launches(chat).await.unwrap().is_empty());
        assert_eq!(
            root.chats().await.unwrap(),
            vec![chat],
            "an empty Chat keeps its owner state"
        );
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(directory.path().join(REGISTRY_DIRECTORY))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    /// One validator decides which files are records: a launch file whose name or content names
    /// another operation or Chat is refused, a Chat directory not named by its canonical id is
    /// not a Chat, and a symbolic link is never followed.
    #[tokio::test]
    async fn only_canonical_unlinked_records_of_their_own_chat_are_read() {
        let directory = tempfile::tempdir().unwrap();
        let root = Registry::default().root(directory.path()).await.unwrap();
        let chats = directory
            .path()
            .join(REGISTRY_DIRECTORY)
            .join(CHATS_DIRECTORY);
        let (chat, other) = (Uuid::new_v4(), Uuid::new_v4());
        let foreign = record(other, LaunchState::Opening);
        let chat_directory = chats.join(chat.to_string());
        std::fs::create_dir_all(&chat_directory).unwrap();
        std::fs::write(
            chat_directory.join(format!(
                "{LAUNCH_PREFIX}{}{RECORD_EXTENSION}",
                foreign.operation
            )),
            serde_json::to_vec(&foreign).unwrap(),
        )
        .unwrap();
        assert!(
            root.launches(chat).await.is_err(),
            "a record naming another Chat is refused"
        );

        let uppercase = Uuid::new_v4();
        std::fs::create_dir_all(chats.join(uppercase.to_string().to_uppercase())).unwrap();
        assert!(
            !root.chats().await.unwrap().contains(&uppercase),
            "only a canonical id names a Chat"
        );

        let linked = Uuid::new_v4();
        let elsewhere = directory.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, chats.join(linked.to_string())).unwrap();
        assert!(
            root.launches(linked).await.is_err(),
            "a linked Chat directory is refused"
        );
        assert!(
            root.record_launch(&record(linked, LaunchState::Opening))
                .await
                .is_err()
        );
        assert_eq!(
            std::fs::read_dir(&elsewhere).unwrap().count(),
            0,
            "nothing written through it"
        );

        let target = Uuid::new_v4();
        let real = record(target, LaunchState::Opening);
        root.record_launch(&real).await.unwrap();
        let path = chats.join(target.to_string()).join(format!(
            "{LAUNCH_PREFIX}{}{RECORD_EXTENSION}",
            real.operation
        ));
        let decoy = directory.path().join("decoy.json");
        std::fs::rename(&path, &decoy).unwrap();
        std::os::unix::fs::symlink(&decoy, &path).unwrap();
        assert!(
            root.launches(target).await.is_err(),
            "a linked record is refused"
        );
    }

    /// A record this build cannot read is dropped alone; the Chat's other records stay readable.
    #[tokio::test]
    async fn an_unreadable_record_is_dropped_and_the_others_are_read() {
        let directory = tempfile::tempdir().unwrap();
        let root = Registry::default().root(directory.path()).await.unwrap();
        let chat = Uuid::new_v4();
        let kept = record(chat, LaunchState::Opening);
        root.record_launch(&kept).await.unwrap();
        let mut old_format = serde_json::to_value(record(chat, LaunchState::Opening)).unwrap();
        old_format.as_object_mut().unwrap().remove("adapter");
        let old_path = directory
            .path()
            .join(REGISTRY_DIRECTORY)
            .join(CHATS_DIRECTORY)
            .join(chat.to_string())
            .join(format!(
                "{LAUNCH_PREFIX}{}{RECORD_EXTENSION}",
                old_format["operation"].as_str().unwrap()
            ));
        std::fs::write(&old_path, serde_json::to_vec(&old_format).unwrap()).unwrap();
        let launches = root.launches(chat).await.unwrap();
        assert_eq!(
            launches
                .iter()
                .map(|record| record.operation)
                .collect::<Vec<_>>(),
            [kept.operation]
        );
        assert!(!old_path.exists(), "the unreadable record is gone");
    }

    /// A record is changed only as it is now: a change decided on an earlier reading of a record
    /// that was removed or changed meanwhile writes nothing.
    #[tokio::test]
    async fn a_transition_applies_only_to_the_record_as_it_is_now() {
        let directory = tempfile::tempdir().unwrap();
        let root = Registry::default().root(directory.path()).await.unwrap();
        let chat = Uuid::new_v4();
        let instance = InstanceRef::hosted(Uuid::new_v4());
        let live = record(
            chat,
            LaunchState::Live {
                instance: instance.clone(),
                host_epoch: Uuid::new_v4(),
            },
        );
        root.record_launch(&live).await.unwrap();
        root.remove_launch(chat, live.operation).await.unwrap();
        let changed = root
            .transition_launch(chat, live.operation, |mut current| {
                current.title = Some("renamed".into());
                Transition::Write(Box::new(current))
            })
            .await
            .unwrap();
        assert!(!changed, "a removed record is not brought back");
        assert!(root.launches(chat).await.unwrap().is_empty());
    }

    /// A Chat's lifecycle lock is kept only while someone holds or awaits it, whether a waiter
    /// gets its turn or gives up, and turns stay exclusive throughout.
    #[tokio::test]
    async fn a_chats_lifecycle_lock_lasts_only_while_it_is_held_or_awaited() {
        let directory = tempfile::tempdir().unwrap();
        let root = Registry::default().root(directory.path()).await.unwrap();
        let chat = Uuid::new_v4();
        let locked = |root: &RootRegistry| root.lifecycles.lock().unwrap().len();

        let first = root.lifecycle(chat).await;
        assert_eq!(locked(&root), 1);
        let gave_up =
            tokio::time::timeout(std::time::Duration::from_millis(50), root.lifecycle(chat)).await;
        assert!(gave_up.is_err(), "a held turn is exclusive");
        let (acquired, is_acquired) = tokio::sync::oneshot::channel();
        let (release, is_released) = tokio::sync::oneshot::channel::<()>();
        let waiter = tokio::spawn({
            let root = root.clone();
            async move {
                let _turn = root.lifecycle(chat).await;
                acquired.send(()).unwrap();
                is_released.await.unwrap();
            }
        });
        tokio::task::yield_now().await;
        drop(first);
        is_acquired.await.unwrap();
        assert_eq!(locked(&root), 1, "the waiter holds its turn");
        release.send(()).unwrap();
        waiter.await.unwrap();
        assert_eq!(locked(&root), 0, "nobody holds or awaits it");

        let held = root.lifecycle(chat).await;
        let gave_up =
            tokio::time::timeout(std::time::Duration::from_millis(50), root.lifecycle(chat)).await;
        assert!(gave_up.is_err());
        drop(held);
        assert_eq!(locked(&root), 0, "a waiter that gave up leaves nothing");
    }

    #[tokio::test]
    async fn a_stopped_owner_state_is_durable() {
        let directory = tempfile::tempdir().unwrap();
        let chat = Uuid::new_v4();
        let stopped = OwnerState::Stopped {
            operation: Uuid::new_v4(),
        };
        Registry::default()
            .root(directory.path())
            .await
            .unwrap()
            .set_owner_state(chat, &stopped)
            .await
            .unwrap();
        let root = Registry::default().root(directory.path()).await.unwrap();
        assert_eq!(root.owner_state(chat).await.unwrap(), stopped);
        assert_eq!(
            root.owner_state(Uuid::new_v4()).await.unwrap(),
            OwnerState::Active
        );
    }
}

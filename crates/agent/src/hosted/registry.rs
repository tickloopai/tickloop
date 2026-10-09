//! The owner daemon's durable record of hosted Tick ownership. Each config root keeps its own
//! private `tick-host/` directory: a lease lock held for the owner's life, and per Chat its owner
//! state and one file per launch record. Only the owner daemon writes it; the files outlive host
//! and owner restarts so ownership, recovery and end events never depend on a live process.

use std::{
    collections::{BTreeSet, HashMap},
    fs::File,
    io,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    sync::Arc,
};

use serde::{Deserialize, Serialize};
use tick_connect::{host_error::HostError, instance::InstanceRef};
use tick_host::host::{Receipt, TickOwner};
use tokio::sync::Mutex;
use uuid::Uuid;

use super::controller::{HostController, LIFECYCLE_TIMEOUT, OwnerListing};

use crate::{
    private_fs::{
        PRIVATE_FILE_MODE, create_private_directory, replace_file_atomically,
        unix_time_milliseconds,
    },
    transcript::retention::RetentionGuard,
};

const REGISTRY_DIRECTORY: &str = "tick-host";
const LEASE_FILE: &str = "registry.lock";
const CHATS_DIRECTORY: &str = "chats";
const OWNER_FILE: &str = "owner.json";
const MANIFEST_FILE: &str = "manifest.json";
const LAUNCH_PREFIX: &str = "launch-";
const RECORD_EXTENSION: &str = ".json";
/// How often the owner checks whether a cleanup still holds a Chat it is deleting.
const DELETION_SETTLE_POLL: std::time::Duration = std::time::Duration::from_secs(1);
/// The host's answer for an instance or owner it no longer runs.
pub const TARGET_GONE: &str = "TargetGone";

/// Whether a Chat may own hosted Ticks. Retention deletion of its Transcript and an explicit
/// StopAgent end it; nothing else does. A deleted Chat leaves no state behind once its Ticks end.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum OwnerState {
    #[default]
    Active,
    /// A cleanup holding the Chat's retention lock is about to remove its Transcript. The Chat
    /// keeps its Ticks until the outcome is known; it opens none meanwhile.
    Deleting { operation: Uuid },
    /// StopAgent stopped this child Chat; its host cleanup is not yet confirmed.
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
    pub command: Vec<String>,
    pub cwd: PathBuf,
    pub identity: LaunchIdentity,
    pub title: Option<String>,
    #[serde(flatten)]
    pub state: LaunchState,
    /// The approved Watches that keep this instance from idling, as the Chat last reported them.
    pub watches: BTreeSet<Uuid>,
    pub updated_ms: u64,
}

impl LaunchRecord {
    /// The record of an open about to be asked of the host.
    pub fn opening(
        operation: Uuid,
        chat: Uuid,
        command: Vec<String>,
        cwd: PathBuf,
        title: Option<String>,
    ) -> Result<Self, HostError> {
        let identity = LaunchIdentity::of(&command, &cwd)
            .map_err(|error| HostError::before("LaunchFailed", error))?;
        Ok(Self {
            operation,
            chat,
            command,
            cwd,
            identity,
            title,
            state: LaunchState::Opening,
            watches: BTreeSet::new(),
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

/// The registries of every config root this owner serves, opened on first use.
#[derive(Default)]
pub struct Registry {
    roots: Mutex<HashMap<PathBuf, Arc<RootRegistry>>>,
}

impl Registry {
    /// The registry of `config_root`, taking its lease on first use. A root whose lease another
    /// owner holds is unavailable rather than shared.
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
            intents: Mutex::new(()),
            lifecycles: std::sync::Mutex::new(HashMap::new()),
        });
        roots.insert(canonical, root.clone());
        Ok(root)
    }
}

/// One config root's registry. Holding it holds the root's lease. Its files are read and
/// written on blocking threads, never on the owner's async workers.
pub struct RootRegistry {
    files: Arc<RegistryFiles>,
    /// Keeps each Watch intent change and its delivery to the host in arrival order.
    intents: Mutex<()>,
    /// Per Chat: opens, reopens and ends take turns, so an end can never fall between an
    /// open's revision and its launch.
    lifecycles: std::sync::Mutex<HashMap<Uuid, Arc<Mutex<()>>>>,
}

struct RegistryFiles {
    config_root: PathBuf,
    directory: PathBuf,
    _lease: File,
    writes: std::sync::Mutex<()>,
}

impl RegistryFiles {
    fn open(config_root: &Path) -> io::Result<Self> {
        let directory = config_root.join(REGISTRY_DIRECTORY);
        create_private_directory(&directory)?;
        create_private_directory(&directory.join(CHATS_DIRECTORY))?;
        let lease = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(directory.join(LEASE_FILE))?;
        if unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "another Agent owner holds this config root's hosted Tick registry",
            ));
        }
        Ok(Self {
            config_root: config_root.to_path_buf(),
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
        let parent = path.parent().expect("registry files live in a directory");
        create_private_directory(parent).map_err(|error| failed(&error))?;
        let bytes = serde_json::to_vec(value).map_err(|error| failed(&error))?;
        replace_file_atomically(path, &bytes, PRIVATE_FILE_MODE).map_err(|error| failed(&error))
    }

    fn revision(&self) -> Result<u64, HostError> {
        match std::fs::read(self.directory.join(MANIFEST_FILE)) {
            Ok(bytes) => serde_json::from_slice::<Manifest>(&bytes)
                .map(|manifest| manifest.revision)
                .map_err(|error| failed(&error)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(0),
            Err(error) => Err(failed(&error)),
        }
    }

    fn next_revision(&self) -> Result<u64, HostError> {
        let _written = self.written();
        let revision = self.revision()? + 1;
        self.write(&self.directory.join(MANIFEST_FILE), &Manifest { revision })?;
        Ok(revision)
    }

    fn owner_state(&self, chat: Uuid) -> Result<OwnerState, HostError> {
        match std::fs::read(self.chat_directory(chat).join(OWNER_FILE)) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| failed(&error)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(OwnerState::Active),
            Err(error) => Err(failed(&error)),
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
        match std::fs::remove_file(self.launch_path(chat, operation)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(failed(&error)),
        }
    }

    fn remove_chat(&self, chat: Uuid) -> Result<(), HostError> {
        let _written = self.written();
        match std::fs::remove_dir_all(self.chat_directory(chat)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(failed(&error)),
        }
    }

    fn launches(&self, chat: Uuid) -> Result<Vec<LaunchRecord>, HostError> {
        let entries = match std::fs::read_dir(self.chat_directory(chat)) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(failed(&error)),
        };
        let mut records = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| failed(&error))?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if !(name.starts_with(LAUNCH_PREFIX) && name.ends_with(RECORD_EXTENSION)) {
                continue;
            }
            let bytes = std::fs::read(entry.path()).map_err(|error| failed(&error))?;
            records.push(serde_json::from_slice(&bytes).map_err(|error| failed(&error))?);
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
            if let Some(chat) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse().ok())
            {
                chats.push(chat);
            }
        }
        Ok(chats)
    }

    /// Whether the cleanup that announced `chat`'s deletion is done with it: it no longer holds
    /// the Chat's retention lock, and no runtime holds it either.
    fn deletion_finished(&self, chat: Uuid) -> bool {
        RetentionGuard::exclusive(&self.config_root, chat).is_ok()
    }

    fn has_transcript(&self, chat: Uuid) -> Result<bool, HostError> {
        crate::transcript::locate(&self.config_root, chat)
            .map(|transcript| transcript.is_some())
            .map_err(|error| failed(&error))
    }
}

impl RootRegistry {
    /// Holds `chat`'s lifecycle until the guard drops.
    pub async fn lifecycle(&self, chat: Uuid) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = self
            .lifecycles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(chat)
            .or_default()
            .clone();
        lock.lock_owned().await
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
    /// still own Ticks, so it can reap any other, and their committed Watch intents. Runs before
    /// anything else is served for the root.
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
            if !self.launches(chat).await?.is_empty() {
                let listing = controller.owner_listing(&owner).await?;
                self.settle_launches(controller, &owner, &listing).await?;
            }
            match self.owner_state(chat).await? {
                OwnerState::Active => owners.push(chat),
                OwnerState::Deleting { operation } => {
                    // A cleanup still holding the retention lock has not finished; until it
                    // has, the Chat keeps its Ticks.
                    let finished = self
                        .on_files(move |files| Ok(files.deletion_finished(chat)))
                        .await?;
                    if !finished || !self.settle_deletion(controller, &owner, operation).await? {
                        owners.push(chat);
                    }
                }
                OwnerState::Stopping { operation } => {
                    self.end_stopped(controller, &owner, operation).await?;
                }
                OwnerState::Stopped { .. } => {}
            }
        }
        let Some(host_epoch) = host_epoch else {
            return Ok(());
        };
        let revision = self.next_revision().await?;
        controller.reconcile_owners(root, revision, &owners).await?;
        for chat in owners {
            let owner = TickOwner {
                root: root.to_owned(),
                chat,
            };
            for record in self.launches(chat).await? {
                if let LaunchState::Live {
                    instance,
                    host_epoch: epoch,
                } = &record.state
                    && *epoch == host_epoch
                {
                    match controller
                        .set_intents(&owner, instance, &record.watches)
                        .await
                    {
                        // It ended since it was listed; the next listing settles its record.
                        Err(error) if error.code == TARGET_GONE => {}
                        result => result?,
                    }
                }
            }
        }
        Ok(())
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
        for mut record in self.launches(chat).await? {
            if record.updated_ms >= listing.started_ms {
                continue;
            }
            match &record.state {
                LaunchState::Opening => {
                    let age = unix_time_milliseconds().saturating_sub(record.updated_ms);
                    if age < LIFECYCLE_TIMEOUT.as_millis() as u64 {
                        continue;
                    }
                    match controller.receipt(record.operation, owner).await? {
                        Some(Receipt::Opened(opened)) => {
                            record.state = LaunchState::Live {
                                instance: opened.instance,
                                host_epoch: opened.terminal.epoch,
                            };
                            self.record_launch(&record).await?;
                        }
                        Some(Receipt::Failed(_)) | None => {
                            self.remove_launch(chat, record.operation).await?;
                        }
                        Some(_) => {}
                    }
                }
                LaunchState::Live {
                    instance,
                    host_epoch,
                } => match &listing.running {
                    Some(running) if running.epoch == *host_epoch => {
                        if !running.live.contains(instance) {
                            self.remove_launch(chat, record.operation).await?;
                        }
                    }
                    _ if controller.was_emptied(*host_epoch) => {
                        self.remove_launch(chat, record.operation).await?;
                    }
                    _ => {
                        record.state = LaunchState::Lost {
                            instance: instance.clone(),
                            host_epoch: *host_epoch,
                        };
                        self.record_launch(&record).await?;
                    }
                },
                LaunchState::Lost { .. } | LaunchState::Reopened { .. } => {}
            }
        }
        Ok(())
    }

    /// Records the approved Watches of one of `owner`'s live instances, then tells the host.
    pub async fn set_watches(
        &self,
        controller: &HostController,
        owner: &TickOwner,
        instance: &InstanceRef,
        watches: BTreeSet<Uuid>,
    ) -> Result<(), HostError> {
        let _intents = self.intents.lock().await;
        let mut record = self
            .launches(owner.chat)
            .await?
            .into_iter()
            .find(|record| {
                matches!(&record.state, LaunchState::Live { instance: live, .. } if live == instance)
            })
            .ok_or_else(|| {
                HostError::before(TARGET_GONE, "this Chat has no such live hosted Tick")
            })?;
        record.watches = watches;
        self.record_launch(&record).await?;
        controller
            .set_intents(owner, instance, &record.watches)
            .await
    }

    /// Records that a cleanup holding `chat`'s retention lock is about to remove its Transcript.
    pub async fn begin_deletion(&self, chat: Uuid, operation: Uuid) -> Result<(), HostError> {
        let _lifecycle = self.lifecycle(chat).await;
        match self.owner_state(chat).await? {
            OwnerState::Active | OwnerState::Deleting { .. } => {
                self.set_owner_state(chat, &OwnerState::Deleting { operation })
                    .await
            }
            OwnerState::Stopping { .. } | OwnerState::Stopped { .. } => Ok(()),
        }
    }

    /// Settles deletion `operation` of a Chat no runtime can load meanwhile: a Transcript still
    /// present makes the Chat Active again; a removed one ends its Ticks and drops its state.
    /// Returns whether the Chat ended. Any other operation or state is left as it is.
    pub async fn settle_deletion(
        &self,
        controller: &HostController,
        owner: &TickOwner,
        operation: Uuid,
    ) -> Result<bool, HostError> {
        let chat = owner.chat;
        let _lifecycle = self.lifecycle(chat).await;
        if self.owner_state(chat).await? != (OwnerState::Deleting { operation }) {
            return Ok(false);
        }
        if self
            .on_files(move |files| files.has_transcript(chat))
            .await?
        {
            self.set_owner_state(chat, &OwnerState::Active).await?;
            return Ok(false);
        }
        controller
            .end_owner(owner, self.revision().await?, operation)
            .await?;
        self.on_files(move |files| files.remove_chat(chat)).await?;
        Ok(true)
    }

    /// Settles deletion `operation` as soon as the cleanup that announced it lets go of the
    /// Chat, whether or not its finish answer arrives; stops once the Chat is no longer being
    /// deleted by it.
    pub async fn settle_deletion_once_released(
        self: Arc<Self>,
        controller: Arc<HostController>,
        owner: TickOwner,
        operation: Uuid,
    ) {
        let chat = owner.chat;
        loop {
            tokio::time::sleep(DELETION_SETTLE_POLL).await;
            match self.owner_state(chat).await {
                Ok(OwnerState::Deleting { operation: current }) if current == operation => {}
                _ => return,
            }
            let released = self
                .on_files(move |files| Ok(files.deletion_finished(chat)))
                .await
                .unwrap_or(false);
            if released {
                let _ = self.settle_deletion(&controller, &owner, operation).await;
                return;
            }
        }
    }

    /// Ends the Ticks of a child Chat StopAgent stopped. The decision is durable before the host
    /// is asked, so a lost answer is carried out by the next reconcile. Idempotent.
    pub async fn end_stopped(
        &self,
        controller: &HostController,
        owner: &TickOwner,
        operation: Uuid,
    ) -> Result<(), HostError> {
        let _lifecycle = self.lifecycle(owner.chat).await;
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
            vec!["/usr/bin/true".into()],
            "/".into(),
            Some("counter".into()),
        )
        .unwrap();
        record.state = state;
        record
    }

    #[tokio::test]
    async fn records_survive_a_new_owner_and_a_root_has_one_owner_at_a_time() {
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
                "a second owner never writes the same root"
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

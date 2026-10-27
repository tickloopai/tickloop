use std::{
    fs::File,
    io::{self, Read, Seek, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::fs::MetadataExt,
    },
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, atomic::Ordering},
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tickloop_code_client::{LocalAgentClient, ReconnectFactory, RecoveringAgentClient};

use tickloop_code_access::{
    AccessError, AgentAccess, CommandEnvelope, CommandReceipt, DisplayReceipt, DisplayUpdate,
    LaunchSpec, Observation, ObservationRequest, RUNTIME_HANDOFF_VERSION, RuntimeBinaryInfo,
    RuntimeVersion,
};

use tickloop_code_local_environment::hosted::worker_control::WorkerControlGrant;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    process::Command,
    sync::Mutex,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{
    ManagedWorker, ProcessWorkerFactory, STARTUP_DEADLINE,
    daemon_server::{DaemonCheckpoint, DaemonServer},
};

const CONNECT_RETRY: std::time::Duration = std::time::Duration::from_secs(1);
const DISCOVERY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
const PENDING_DISCOVERY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
const HANDOFF_MAX_BYTES: u64 = 128 * 1024 * 1024;
const PREPARED: u8 = 1;
const COMMITTED: u8 = 2;
const ADOPTED: u8 = 3;
const RELEASED: u8 = 4;
const ACTIVATED: u8 = 5;

pub(crate) struct WorkerAddress {
    pub endpoint: PathBuf,
    pub pid: u32,
}

pub(crate) fn worker_access(
    client: LocalAgentClient,
    endpoint: PathBuf,
    pid: u32,
) -> (Arc<dyn AgentAccess>, Arc<std::sync::RwLock<WorkerAddress>>) {
    let address = Arc::new(std::sync::RwLock::new(WorkerAddress { endpoint, pid }));
    let reconnect_address = address.clone();
    let epoch = client.daemon_epoch();
    let reconnect: ReconnectFactory = Arc::new(move || {
        let (endpoint, pid) = {
            let address = reconnect_address.read().unwrap();
            (address.endpoint.clone(), address.pid)
        };
        Box::pin(async move {
            if !process_exists(pid) {
                return Err(AccessError::unavailable("Agent Worker exited"));
            }
            let client =
                tokio::time::timeout(STARTUP_DEADLINE, LocalAgentClient::connect(&endpoint))
                    .await
                    .map_err(|_| AccessError::unavailable("Agent Worker reconnect timed out"))??;
            if client.daemon_epoch() != epoch {
                return Err(AccessError::unavailable("Agent Worker epoch changed"));
            }
            Ok(client)
        })
    });
    (
        Arc::new(RecoveringAgentClient::new(client, reconnect)),
        address,
    )
}

#[derive(Default)]
pub(crate) struct UpgradeState {
    target: Option<PathBuf>,
    error: Option<String>,
    discovery_error: Option<String>,
    last_discovery: Option<std::time::Instant>,
    upgrading: bool,
    target_lease: Option<tickhost_connect::executable_lease::ExecutableLease>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct DaemonHandoff {
    version: u16,
    transfer_id: Uuid,
    pub epoch: Uuid,
    pub listener: RawFd,
    pub listener_identity: (u64, u64),
    pub control_identity: (u64, u64),
    pub control_listener: RawFd,
    pub handshake: RawFd,
    workers: Vec<WorkerHandoff>,
    daemon: DaemonCheckpoint,
    ingress: crate::ingress::IngressCheckpoint,
}

#[derive(Serialize, Deserialize)]
struct WorkerHandoff {
    endpoint: PathBuf,
    pid: u32,
    build_id: Option<String>,
    lifeline: RawFd,
    grant: WorkerControlGrant,
    launch: LaunchSpec,
    executable: PathBuf,
    #[serde(default)]
    upgrade_error: Option<String>,
}

pub(crate) fn duplicate(fd: RawFd) -> io::Result<OwnedFd> {
    let copied = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
    if copied < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(copied) })
    }
}

fn close_on_exec(fd: RawFd) -> Result<(), String> {
    if fd < 3 || unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
        Err("invalid inherited upgrade descriptor".into())
    } else {
        Ok(())
    }
}

pub(crate) fn read_handoff(fd: RawFd) -> Result<DaemonHandoff, String> {
    close_on_exec(fd)?;
    let mut file = unsafe { File::from_raw_fd(fd) };
    tickloop_code_client::daemon_endpoint::validate_file(&file, "daemon handoff")?;
    if file.metadata().map_err(|error| error.to_string())?.len() > HANDOFF_MAX_BYTES {
        return Err("daemon handoff is too large".into());
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    let handoff: DaemonHandoff =
        serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    if handoff.version != RUNTIME_HANDOFF_VERSION {
        return Err("daemon handoff format is incompatible".into());
    }
    let mut descriptors = vec![
        handoff.listener,
        handoff.control_listener,
        handoff.handshake,
    ];
    descriptors.extend(handoff.workers.iter().map(|worker| worker.lifeline));
    descriptors.sort_unstable();
    if descriptors.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err("daemon handoff repeats a descriptor".into());
    }
    for descriptor in descriptors {
        close_on_exec(descriptor)?;
    }
    Ok(handoff)
}

pub(crate) fn validate_inherited_endpoint(
    endpoint: &Path,
    identity: (u64, u64),
) -> Result<(), String> {
    tickloop_code_client::daemon_endpoint::validate_socket(endpoint)?;
    let metadata = std::fs::symlink_metadata(endpoint).map_err(|error| error.to_string())?;
    if (metadata.dev(), metadata.ino()) != identity {
        return Err("daemon endpoint changed while preparing its successor".into());
    }
    Ok(())
}

pub(crate) fn inherited_listener(fd: RawFd) -> Result<UnixListener, String> {
    let listener = unsafe { std::os::unix::net::UnixListener::from_raw_fd(fd) };
    listener
        .set_nonblocking(true)
        .map_err(|error| error.to_string())?;
    UnixListener::from_std(listener).map_err(|error| error.to_string())
}

pub(crate) fn inherited_stream(fd: RawFd) -> Result<UnixStream, String> {
    let stream = unsafe { std::os::unix::net::UnixStream::from_raw_fd(fd) };
    stream
        .set_nonblocking(true)
        .map_err(|error| error.to_string())?;
    UnixStream::from_std(stream).map_err(|error| error.to_string())
}

pub(crate) async fn wait_for_commit(stream: &mut UnixStream) -> Result<(), String> {
    stream
        .write_u8(PREPARED)
        .await
        .map_err(|error| error.to_string())?;
    let commit = tokio::time::timeout(STARTUP_DEADLINE, stream.read_u8())
        .await
        .map_err(|_| "daemon handoff commit timed out")?
        .map_err(|error| error.to_string())?;
    if commit != COMMITTED {
        return Err("daemon handoff was not committed".into());
    }
    stream
        .write_u8(ADOPTED)
        .await
        .map_err(|error| error.to_string())?;
    let release = tokio::time::timeout(STARTUP_DEADLINE, stream.read_u8())
        .await
        .map_err(|_| "daemon activation timed out")?
        .map_err(|error| error.to_string())?;
    if release != RELEASED {
        return Err("daemon handoff was not activated".into());
    }
    stream
        .write_u8(ACTIVATED)
        .await
        .map_err(|error| error.to_string())
}

impl ProcessWorkerFactory {
    pub(crate) async fn select_upgrade(&self, executable: &Path) -> Result<(), AccessError> {
        let (executable, binary) = verify_successor(executable).await?;
        let lease = retain_executable(&executable)
            .map_err(|error| AccessError::unavailable(error.to_string()))?;
        let current = std::env::current_exe()
            .and_then(|path| path.canonicalize())
            .map_err(|error| AccessError::unavailable(error.to_string()))?;
        if executable == current {
            return if Some(binary.build_id.as_str()) == tickloop_code_access::build_identity() {
                Ok(())
            } else {
                Err(AccessError::unavailable(
                    "The running executable was overwritten; use an immutable `make dev` bundle for automatic activation",
                ))
            };
        }
        let mut state = self.upgrade.lock().await;
        if state.target.as_ref() == Some(&executable) && state.error.is_none() {
            return Ok(());
        }
        state.target = Some(executable);
        state.target_lease = lease;
        state.error = None;
        self.upgrade_requested.notify_one();
        Ok(())
    }

    pub(crate) async fn record_daemon_upgrade_error(&self, error: String) {
        self.upgrade.lock().await.error = Some(error);
    }

    pub(crate) async fn version(
        &self,
        routes: &[(tickloop_code_access::ExecutionRef, Arc<dyn AgentAccess>)],
    ) -> RuntimeVersion {
        let (requested_executable, upgrading, error) = {
            let state = self.upgrade.lock().await;
            (
                state.target.clone(),
                state.upgrading,
                state
                    .error
                    .clone()
                    .or_else(|| state.discovery_error.clone()),
            )
        };
        let executable = std::env::current_exe().unwrap_or_default();
        let workers = self
            .workers
            .lock()
            .await
            .iter()
            .map(|worker| tickloop_code_access::WorkerRuntimeVersion {
                process_id: worker.pid,
                executable: worker.executable.clone(),
                build_id: worker.build_id.clone(),
                upgrading: worker.preparation_uncertain || worker.executable != executable,
                executions: routes
                    .iter()
                    .filter(|(_, access)| Arc::ptr_eq(access, &worker.access))
                    .map(|(execution, _)| *execution)
                    .collect(),
                error: worker.upgrade_error.clone(),
            })
            .collect();
        RuntimeVersion {
            executable,
            build_id: tickloop_code_access::build_identity()
                .unwrap_or_default()
                .into(),
            requested_executable,
            upgrading,
            error,
            workers,
        }
    }

    pub(crate) async fn restore_workers(
        &self,
        handoff: DaemonHandoff,
        daemon: &DaemonServer,
    ) -> Result<(), AccessError> {
        self.ingress
            .restore(handoff.ingress)
            .map_err(AccessError::invalid)?;
        let mut restored = Vec::with_capacity(handoff.workers.len());
        for worker in handoff.workers {
            let (access, link): (Arc<dyn AgentAccess>, _) =
                match LocalAgentClient::connect(&worker.endpoint).await {
                    Ok(client)
                        if client.daemon_epoch() == handoff.epoch
                            && client.server_process_id() == worker.pid =>
                    {
                        worker_access(client, worker.endpoint.clone(), worker.pid)
                    }
                    Ok(_) => return Err(AccessError::invalid("worker handoff epoch mismatch")),
                    Err(_) if !process_exists(worker.pid) => (
                        Arc::new(ExitedWorker),
                        Arc::new(std::sync::RwLock::new(WorkerAddress {
                            endpoint: worker.endpoint.clone(),
                            pid: worker.pid,
                        })),
                    ),
                    Err(error) => return Err(error),
                };
            self.control
                .restore_grant(
                    &worker.grant,
                    access.clone(),
                    worker.launch.config_root.as_deref(),
                )
                .await;
            restored.push(ManagedWorker {
                access,
                process: Arc::new(Mutex::new(None)),
                lifeline: unsafe { OwnedFd::from_raw_fd(worker.lifeline) },
                pid: worker.pid,
                build_id: worker.build_id,
                endpoint: worker.endpoint,
                grant: worker.grant,
                launch: worker.launch,
                executable_lease: retain_executable(&worker.executable)
                    .map_err(|error| AccessError::unavailable(error.to_string()))?,
                executable: worker.executable,
                link,
                preparation_uncertain: false,
                upgrade_error: worker.upgrade_error,
            });
        }
        let access = restored
            .iter()
            .map(|worker| worker.access.clone())
            .collect::<Vec<_>>();
        daemon.restore(handoff.daemon, &access).await?;
        *self.workers.lock().await = restored;
        Ok(())
    }
}

pub(crate) fn process_exists(pid: u32) -> bool {
    pid != 0 && unsafe { libc::kill(pid as i32, 0) } == 0
}

pub(crate) fn retain_executable(
    executable: &Path,
) -> io::Result<Option<tickhost_connect::executable_lease::ExecutableLease>> {
    let Some(directory) = executable
        .parent()
        .filter(|directory| directory.join("installation.json").is_file())
    else {
        return Ok(None);
    };
    tickhost_connect::executable_lease::ExecutableLease::acquire(
        directory,
        &tickhost_connect::executable_lease::lease_directory()?,
    )
    .map(Some)
}

async fn verify_successor(executable: &Path) -> Result<(PathBuf, RuntimeBinaryInfo), AccessError> {
    if !executable.is_absolute() {
        return Err(AccessError::invalid("upgrade executable must be absolute"));
    }
    let executable = executable
        .canonicalize()
        .map_err(|error| AccessError::invalid(error.to_string()))?;
    let metadata =
        std::fs::metadata(&executable).map_err(|error| AccessError::invalid(error.to_string()))?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o022 != 0
        || metadata.mode() & 0o111 == 0
    {
        return Err(AccessError::invalid(
            "upgrade executable must be a private executable owned by this user",
        ));
    }
    let running_executable =
        std::env::current_exe().map_err(|error| AccessError::unavailable(error.to_string()))?;
    let candidate = executable.clone();
    tokio::task::spawn_blocking(move || {
        tickloop_code_updater::ensure_same_runtime_source(&running_executable, &candidate)
    })
    .await
    .map_err(|error| AccessError::unavailable(error.to_string()))?
    .map_err(|error| AccessError::invalid(format!("{error:#}")))?;
    let output = tokio::time::timeout(
        STARTUP_DEADLINE,
        Command::new(&executable)
            .arg("--runtime-info")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| AccessError::unavailable("upgrade executable preflight timed out"))?
    .map_err(|error| AccessError::unavailable(error.to_string()))?;
    if !output.status.success() || output.stdout.len() > 64 * 1024 {
        return Err(AccessError::invalid("upgrade executable preflight failed"));
    }
    let info: RuntimeBinaryInfo = serde_json::from_slice(&output.stdout)
        .map_err(|_| AccessError::invalid("upgrade executable has no supported runtime handoff"))?;
    let running_version = semver::Version::parse(env!("CARGO_PKG_VERSION"))
        .map_err(|error| AccessError::invalid(error.to_string()))?;
    let candidate_version = semver::Version::parse(&info.product_version).map_err(|_| {
        AccessError::invalid("upgrade executable returned an invalid product version")
    })?;
    if candidate_version < running_version {
        return Err(AccessError::invalid(
            "runtime downgrade is not an automatic upgrade",
        ));
    }
    if info.handoff_version != RUNTIME_HANDOFF_VERSION
        || info.protocol_major != tickloop_code_access::PROTOCOL_MAJOR
    {
        return Err(AccessError::invalid(
            "upgrade executable uses an incompatible runtime protocol",
        ));
    }
    Ok((executable, info))
}

pub(crate) async fn drive_daemon_upgrade(
    factory: Arc<ProcessWorkerFactory>,
    daemon: Arc<DaemonServer>,
    lock: &File,
    listener: &OwnedFd,
    control_listener: &OwnedFd,
    shutdown: &CancellationToken,
) -> Option<DaemonTransfer> {
    loop {
        tokio::select! {
            () = shutdown.cancelled() => return None,
            () = factory.upgrade_requested.notified() => {},
            () = tokio::time::sleep(DISCOVERY_INTERVAL) => {}
        }
        let _ = factory.control.host_controller().maintain_upgrade().await;
        let should_discover = {
            let mut state = factory.upgrade.lock().await;
            let interval = if state.target.is_some() {
                PENDING_DISCOVERY_INTERVAL
            } else {
                DISCOVERY_INTERVAL
            };
            let due = state
                .last_discovery
                .is_none_or(|last| last.elapsed() >= interval);
            if due {
                state.last_discovery = Some(std::time::Instant::now());
            }
            due
        };
        if should_discover {
            let selected =
                match tokio::task::spawn_blocking(tickloop_code_updater::selected_runtime).await {
                    Ok(Ok(selected)) => {
                        factory.upgrade.lock().await.discovery_error = None;
                        selected
                    }
                    Ok(Err(error)) => {
                        factory.upgrade.lock().await.discovery_error = Some(error.to_string());
                        continue;
                    }
                    Err(error) => {
                        factory.upgrade.lock().await.discovery_error = Some(error.to_string());
                        continue;
                    }
                };
            if let Some(selected) = selected {
                let current_target = factory.upgrade.lock().await.target.clone();
                if current_target.as_ref() != Some(&selected.executable)
                    && let Err(error) = factory.select_upgrade(&selected.executable).await
                {
                    factory.record_daemon_upgrade_error(error.to_string()).await;
                    continue;
                }
            }
        }
        if factory.upgrade.lock().await.discovery_error.is_some() {
            continue;
        }
        let Some(target) = factory.upgrade.lock().await.target.clone() else {
            continue;
        };
        factory.upgrade.lock().await.upgrading = true;
        match transfer_daemon(&factory, &daemon, &target, lock, listener, control_listener).await {
            Ok(transfer) => return Some(transfer),
            Err(error) => {
                let mut state = factory.upgrade.lock().await;
                state.upgrading = false;
                state.error = Some(error);
            }
        }
    }
}

pub(crate) struct DaemonTransfer {
    quiescence: crate::ingress::Quiescence,
    successor_process_id: u32,
}

impl DaemonTransfer {
    pub(crate) async fn finish(self) {
        self.quiescence.finish(self.successor_process_id).await;
    }
}

async fn transfer_daemon(
    factory: &ProcessWorkerFactory,
    daemon: &DaemonServer,
    executable: &Path,
    lock: &File,
    listener: &OwnedFd,
    control_listener: &OwnedFd,
) -> Result<DaemonTransfer, String> {
    let Some(_daemon_admission) = daemon.try_pause() else {
        return Err("waiting for the current daemon command to finish".into());
    };
    let Some(_control_admission) = factory.control.try_pause() else {
        return Err("waiting for the current Tickhost request to finish".into());
    };
    let transfer_id = tickloop_ids::generate();
    let mut quiescence = factory.ingress.quiesce(transfer_id).await?;
    let workers = factory.workers.lock().await;
    if workers.iter().any(|worker| worker.preparation_uncertain) {
        return Err("a Worker ownership transfer must settle before upgrading its daemon".into());
    }
    let access = workers
        .iter()
        .map(|worker| worker.access.clone())
        .collect::<Vec<_>>();
    let saved_daemon = daemon
        .checkpoint(&access)
        .await
        .map_err(|error| error.to_string())?;
    let (parent_channel, child_channel) =
        std::os::unix::net::UnixStream::pair().map_err(|error| error.to_string())?;
    parent_channel
        .set_nonblocking(true)
        .map_err(|error| error.to_string())?;
    drop(parent_channel);
    let mut descriptors = vec![
        duplicate(lock.as_raw_fd()),
        duplicate(listener.as_raw_fd()),
        duplicate(control_listener.as_raw_fd()),
        duplicate(child_channel.as_raw_fd()),
    ]
    .into_iter()
    .collect::<io::Result<Vec<_>>>()
    .map_err(|error| error.to_string())?;
    let mut saved_workers = Vec::new();
    for worker in workers.iter() {
        let lifeline = duplicate(worker.lifeline.as_raw_fd()).map_err(|error| error.to_string())?;
        saved_workers.push(WorkerHandoff {
            endpoint: worker.endpoint.clone(),
            pid: worker.pid,
            build_id: worker.build_id.clone(),
            lifeline: lifeline.as_raw_fd(),
            grant: worker.grant.clone(),
            launch: worker.launch.clone(),
            executable: worker.executable.clone(),
            upgrade_error: worker.upgrade_error.clone(),
        });
        descriptors.push(lifeline);
    }
    let mut handoff = DaemonHandoff {
        version: RUNTIME_HANDOFF_VERSION,
        transfer_id,
        epoch: daemon.daemon_epoch(),
        listener: descriptors[1].as_raw_fd(),
        listener_identity: factory.listener_identity,
        control_identity: factory.control_identity,
        control_listener: descriptors[2].as_raw_fd(),
        handshake: descriptors[3].as_raw_fd(),
        workers: saved_workers,
        daemon: saved_daemon,
        ingress: quiescence.checkpoint(),
    };
    let checkpoint_path = factory
        .directory
        .join(format!("daemon-upgrade-{}.checkpoint", handoff.transfer_id));
    let record_path = factory
        .directory
        .join(format!("daemon-upgrade-{}.json", handoff.transfer_id));
    let mut transfer_files = PreparedDaemonFiles {
        checkpoint: checkpoint_path.clone(),
        record: record_path.clone(),
        committed: false,
    };
    let mut checkpoint =
        tempfile::NamedTempFile::new_in(&factory.directory).map_err(|error| error.to_string())?;
    serde_json::to_writer(&mut checkpoint, &handoff).map_err(|error| error.to_string())?;
    checkpoint
        .flush()
        .and_then(|()| checkpoint.as_file().sync_all())
        .map_err(|error| error.to_string())?;
    let mut checkpoint = checkpoint
        .persist_noclobber(&checkpoint_path)
        .map_err(|error| error.to_string())?;
    checkpoint.rewind().map_err(|error| error.to_string())?;
    let mut record = DaemonTransferRecord {
        transfer_id: handoff.transfer_id,
        epoch: handoff.epoch,
        phase: "prepared".into(),
        executable: executable.to_path_buf(),
        checkpoint: checkpoint_path.clone(),
        successor_pid: None,
    };
    persist_record(&record_path, &record)?;
    let mut committed = false;
    loop {
        let candidate = async {
            let (parent, child) =
                std::os::unix::net::UnixStream::pair().map_err(|error| error.to_string())?;
            parent
                .set_nonblocking(true)
                .map_err(|error| error.to_string())?;
            let mut channel = UnixStream::from_std(parent).map_err(|error| error.to_string())?;
            descriptors[3] = duplicate(child.as_raw_fd()).map_err(|error| error.to_string())?;
            handoff.handshake = descriptors[3].as_raw_fd();
            checkpoint
                .set_len(0)
                .and_then(|()| checkpoint.rewind())
                .map_err(|error| error.to_string())?;
            serde_json::to_writer(&mut checkpoint, &handoff).map_err(|error| error.to_string())?;
            checkpoint
                .flush()
                .and_then(|()| checkpoint.sync_all())
                .and_then(|()| checkpoint.rewind())
                .map_err(|error| error.to_string())?;
            let mut inherited = descriptors
                .iter()
                .map(AsRawFd::as_raw_fd)
                .collect::<Vec<_>>();
            inherited.push(checkpoint.as_raw_fd());
            let mut command = Command::new(executable);
            command
                .arg("daemon")
                .arg("run")
                .arg(&factory.directory)
                .arg(descriptors[0].as_raw_fd().to_string())
                .arg(checkpoint.as_raw_fd().to_string())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true);
            unsafe {
                command.pre_exec(move || {
                    for descriptor in &inherited {
                        if libc::fcntl(*descriptor, libc::F_SETFD, 0) == -1 {
                            return Err(io::Error::last_os_error());
                        }
                    }
                    Ok(())
                });
            }
            let mut successor = command.spawn().map_err(|error| error.to_string())?;
            record.successor_pid = successor.id();
            if let Err(error) = persist_record(&record_path, &record) {
                let _ = successor.start_kill();
                let _ = successor.wait().await;
                return Err(error);
            }
            let ready = tokio::time::timeout(STARTUP_DEADLINE, channel.read_u8()).await;
            if !matches!(ready, Ok(Ok(PREPARED))) {
                let _ = successor.start_kill();
                let _ = successor.wait().await;
                return Err("successor daemon did not become ready".into());
            }
            Ok::<_, String>((successor, channel))
        }
        .await;
        let (mut successor, mut channel) = match candidate {
            Ok(candidate) => candidate,
            Err(error) if !committed => return Err(error),
            Err(error) => {
                factory
                    .record_daemon_upgrade_error(format!(
                        "recovering committed daemon transfer: {error}"
                    ))
                    .await;
                tokio::time::sleep(CONNECT_RETRY).await;
                continue;
            }
        };
        if !committed {
            factory.lifecycle.begin_handoff()?;
            record.phase = "committed".into();
            if let Err(error) = persist_record(&record_path, &record) {
                factory.lifecycle.cancel_handoff();
                return Err(error);
            }
            transfer_files.committed = true;
            quiescence.commit();
            daemon.retire();
            factory.control.retire();
            factory.shutting_down.store(true, Ordering::SeqCst);
            committed = true;
        }
        let commit = channel.write_u8(COMMITTED).await;
        let adopted = if commit.is_ok() {
            matches!(
                tokio::time::timeout(STARTUP_DEADLINE, channel.read_u8()).await,
                Ok(Ok(ADOPTED))
            )
        } else {
            false
        };
        if !adopted {
            // No RELEASE has been sent, so this candidate has never served or dispatched work.
            let _ = successor.start_kill();
            let _ = successor.wait().await;
            tokio::time::sleep(CONNECT_RETRY).await;
            continue;
        }
        record.phase = "activated".into();
        if let Err(error) = persist_record(&record_path, &record) {
            let _ = successor.start_kill();
            let _ = successor.wait().await;
            record.phase = "committed".into();
            factory
                .record_daemon_upgrade_error(format!("cannot persist daemon activation: {error}"))
                .await;
            tokio::time::sleep(CONNECT_RETRY).await;
            continue;
        }
        if let Err(error) = factory.lifecycle.begin_activation() {
            let _ = successor.start_kill();
            let _ = successor.wait().await;
            factory.lifecycle.shutdown().cancelled().await;
            return Err(error);
        }
        let released = channel.write_u8(RELEASED).await;
        if released.is_ok()
            && matches!(
                tokio::time::timeout(STARTUP_DEADLINE, channel.read_u8()).await,
                Ok(Ok(ACTIVATED))
            )
        {
            let _ = std::fs::remove_file(&record_path);
            let _ = std::fs::remove_file(&checkpoint_path);
        }
        // An uncertain activation never permits restoring the previous controller or killing its successor.
        if successor.try_wait().ok().flatten().is_none() {
            std::mem::forget(successor);
        }
        let successor_process_id = record
            .successor_pid
            .expect("a prepared successor has a process identity");
        return Ok(DaemonTransfer {
            quiescence,
            successor_process_id,
        });
    }
}

struct PreparedDaemonFiles {
    checkpoint: PathBuf,
    record: PathBuf,
    committed: bool,
}

impl Drop for PreparedDaemonFiles {
    fn drop(&mut self) {
        if !self.committed {
            let _ = std::fs::remove_file(&self.checkpoint);
            let _ = std::fs::remove_file(&self.record);
        }
    }
}

#[derive(Serialize)]
struct DaemonTransferRecord {
    transfer_id: Uuid,
    epoch: Uuid,
    phase: String,
    executable: PathBuf,
    checkpoint: PathBuf,
    successor_pid: Option<u32>,
}

fn persist_record(path: &Path, record: &DaemonTransferRecord) -> Result<(), String> {
    let directory = path
        .parent()
        .ok_or("daemon handoff record has no directory")?;
    let mut file = tempfile::NamedTempFile::new_in(directory).map_err(|error| error.to_string())?;
    serde_json::to_writer(&mut file, record).map_err(|error| error.to_string())?;
    file.flush()
        .and_then(|()| file.as_file().sync_all())
        .map_err(|error| error.to_string())?;
    file.persist(path).map_err(|error| error.to_string())?;
    File::open(directory)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| error.to_string())
}

struct ExitedWorker;
#[async_trait]
impl AgentAccess for ExitedWorker {
    async fn observe(&self, _request: ObservationRequest) -> Result<Observation, AccessError> {
        Err(AccessError::unavailable("worker exited"))
    }
    async fn execute(&self, _command: CommandEnvelope) -> Result<CommandReceipt, AccessError> {
        Err(AccessError::unavailable("worker exited"))
    }
    async fn show_view(&self, _update: DisplayUpdate) -> Result<DisplayReceipt, AccessError> {
        Err(AccessError::unavailable("worker exited"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EmbeddedAgentAccess, WorkerFactory};

    use tickloop_code_access::{AccessErrorCode, AgentCommand, CommandResult, PersistenceState};

    use tickloop_harness::runtime::Agent;

    struct OneWorker(Arc<dyn AgentAccess>);
    #[async_trait]
    impl WorkerFactory for OneWorker {
        async fn start(
            &self,
            _: &LaunchSpec,
            _: Uuid,
        ) -> Result<Arc<dyn AgentAccess>, AccessError> {
            Ok(self.0.clone())
        }
        async fn exited(&self, _: &Arc<dyn AgentAccess>) -> bool {
            false
        }
    }

    fn create(epoch: Uuid, workdir: PathBuf) -> CommandEnvelope {
        CommandEnvelope {
            daemon_epoch: epoch,
            command_id: Uuid::new_v4(),
            command: AgentCommand::Create {
                launch_rules: Vec::new(),
                launch: LaunchSpec::embedded(workdir),
                initial: None,
                origin: None,
            },
        }
    }

    fn runtime() -> Agent {
        Agent::new_embedded(
            tickloop_harness::test_support::config(),
            Arc::new(tickloop_harness::test_support::ScriptedModel::new(vec![])),
        )
    }

    #[tokio::test]
    async fn successor_rejects_a_replaced_published_endpoint() {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let endpoint = directory.path().join("daemon.sock");
        let replacement = directory.path().join("replacement.sock");
        let (_old, old_identity) = crate::private_listener(&endpoint).unwrap();
        let (_new, new_identity) = crate::private_listener(&replacement).unwrap();
        crate::remove_owned_socket(&endpoint, old_identity).unwrap();
        std::fs::rename(&replacement, &endpoint).unwrap();
        assert!(validate_inherited_endpoint(&endpoint, old_identity).is_err());
        validate_inherited_endpoint(&endpoint, new_identity).unwrap();
        crate::remove_owned_socket(&endpoint, new_identity).unwrap();
    }

    #[tokio::test]
    async fn daemon_handoff_preserves_receipt_digests_execution_and_control_fence() {
        let root = tempfile::tempdir().unwrap();
        let epoch = Uuid::new_v4();
        let runtime = runtime();
        let worker: Arc<dyn AgentAccess> =
            Arc::new(EmbeddedAgentAccess::with_epoch(runtime.clone(), epoch));
        let factory = Arc::new(OneWorker(worker.clone()));
        let old = DaemonServer::new(epoch, factory.clone());
        let command = create(epoch, root.path().to_path_buf());
        let receipt = old.execute(command.clone()).await.unwrap();
        let pause = old.pause().await;
        let checkpoint = old.checkpoint(std::slice::from_ref(&worker)).await.unwrap();
        let bytes = serde_json::to_vec(&checkpoint).unwrap();
        let replacement = DaemonServer::new(epoch, factory);
        replacement
            .restore(
                serde_json::from_slice(&bytes).unwrap(),
                std::slice::from_ref(&worker),
            )
            .await
            .unwrap();
        old.retire();
        drop(pause);
        assert_eq!(
            old.execute(command.clone()).await.unwrap_err().code,
            AccessErrorCode::HandoffNotAccepted
        );
        let replay = replacement.execute(command.clone()).await.unwrap();
        assert_eq!(
            serde_json::to_value(&receipt).unwrap(),
            serde_json::to_value(&replay).unwrap()
        );
        let execution = match receipt.result.unwrap() {
            CommandResult::Created { execution, .. } => execution,
            other => panic!("unexpected result {other:?}"),
        };
        assert!(
            replacement
                .verify_worker_chat(
                    match replay.result.unwrap() {
                        CommandResult::Created { snapshot, .. } => snapshot.conversation.id.into(),
                        _ => unreachable!(),
                    },
                    &worker
                )
                .await
                .is_ok_and(|current| current == execution)
        );
        let mut conflicting = command;
        conflicting.command = AgentCommand::FlushTranscripts;
        assert_eq!(
            replacement.execute(conflicting).await.unwrap_err().code,
            AccessErrorCode::CommandIdConflict
        );
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn receipt_handoff_retains_unknown_outcomes_without_redispatch() {
        let root = tempfile::tempdir().unwrap();
        let epoch = Uuid::new_v4();
        let store = crate::receipts::ReceiptStore::new(epoch);
        let command = create(epoch, root.path().to_path_buf());
        assert!(store.begin(&command).await.unwrap().is_none());
        assert!(store.checkpoint().await.is_err());
        let unknown = store
            .finish(
                command.command_id,
                Err(AccessError::new(
                    AccessErrorCode::OutcomeUnknown,
                    "dispatched before transport closed",
                )),
                PersistenceState::BestEffortPending,
            )
            .await;
        let restored = crate::receipts::ReceiptStore::new(epoch);
        restored
            .restore(store.checkpoint().await.unwrap())
            .await
            .unwrap();
        let replay = restored.begin(&command).await.unwrap().unwrap();
        assert_eq!(
            serde_json::to_value(unknown).unwrap(),
            serde_json::to_value(replay).unwrap()
        );
    }

    #[tokio::test]
    async fn worker_handoff_keeps_execution_and_receipts_until_explicit_release() {
        let directory = tempfile::tempdir().unwrap();
        let epoch = Uuid::new_v4();
        let old = EmbeddedAgentAccess::with_epoch(runtime(), epoch);
        let command = create(epoch, directory.path().to_path_buf());
        let receipt = old.execute(command.clone()).await.unwrap();
        let checkpoint = old
            .prepare_upgrade()
            .await
            .unwrap()
            .expect("idle family can transfer");
        let serialized = serde_json::to_vec(&checkpoint).unwrap();
        let new = EmbeddedAgentAccess::with_epoch(runtime(), epoch);
        new.restore_upgrade(serde_json::from_slice(&serialized).unwrap())
            .await
            .unwrap();
        new.activate_upgrade().await.unwrap();
        assert_eq!(
            new.execute(command.clone()).await.unwrap_err().code,
            AccessErrorCode::Unavailable
        );
        new.release_upgrade().await.unwrap();
        let replay = new.execute(command).await.unwrap();
        assert_eq!(
            serde_json::to_value(receipt).unwrap(),
            serde_json::to_value(replay).unwrap()
        );
        old.shutdown_for_upgrade().await;
        new.shutdown().await;
    }
}

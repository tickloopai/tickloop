use std::{
    ffi::OsString,
    fs::File,
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::ffi::OsStringExt,
    },
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use serde::{Deserialize, Serialize};
use tickhost_connect::hosted::{peer_uid, same_secret};
use tickloop_agent_client::daemon_endpoint::check_socket_length;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    process::{Child, Command},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    EmbeddedAgentAccess, ProcessWorkerFactory, WorkerBootstrap, daemon_server::DaemonServer,
    embedded::EmbeddedCheckpoint, upgrade,
};

const MAX_CHECKPOINT_BYTES: usize = 128 * 1024 * 1024;
const HANDOFF_TIMEOUT: Duration = Duration::from_secs(30);
const WORKER_UPGRADE_INTERVAL: Duration = Duration::from_secs(1);
const PREPARED: u8 = 1;
const COMMITTED: u8 = 2;
const ADOPTED: u8 = 3;
const RELEASED: u8 = 4;
const ACTIVATED: u8 = 5;

#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
enum Phase {
    Prepared,
    Committed,
    Activated,
}

#[derive(Serialize, Deserialize)]
struct TransferRecord {
    version: u16,
    transfer_id: Uuid,
    epoch: Uuid,
    phase: Phase,
    previous_endpoint: PathBuf,
    checkpoint: PathBuf,
    executable: PathBuf,
    successor_endpoint: PathBuf,
    successor_pid: Option<u32>,
}

struct PendingUpgrade {
    record: TransferRecord,
    record_path: PathBuf,
    candidate: Option<Candidate>,
}

struct Candidate {
    process: Child,
    lifeline: OwnedFd,
    channel: UnixStream,
    commit_sent: bool,
    adopted: bool,
    release_sent: bool,
}

impl PendingUpgrade {
    fn persist(&self) -> Result<(), String> {
        let directory = self
            .record_path
            .parent()
            .ok_or("handoff record has no directory")?;
        let mut temporary =
            tempfile::NamedTempFile::new_in(directory).map_err(|error| error.to_string())?;
        serde_json::to_writer(&mut temporary, &self.record).map_err(|error| error.to_string())?;
        temporary
            .flush()
            .and_then(|()| temporary.as_file().sync_all())
            .map_err(|error| error.to_string())?;
        temporary
            .persist(&self.record_path)
            .map_err(|error| error.to_string())?;
        File::open(directory)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| error.to_string())
    }

    fn cleanup(&self) {
        let _ = std::fs::remove_file(&self.record_path);
        let _ = std::fs::remove_file(&self.record.checkpoint);
    }
}

#[derive(Serialize, Deserialize)]
struct Request {
    capability: String,
    operation: Operation,
}
#[derive(Serialize, Deserialize)]
enum Operation {
    Prepare,
    Cancel,
    Retire,
}
#[derive(Serialize, Deserialize)]
enum Reply {
    Prepared(Option<Box<EmbeddedCheckpoint>>),
    Done,
    Error(String),
}

pub(crate) fn endpoint(worker: &Path) -> PathBuf {
    worker.with_extension("upgrade")
}

async fn read<T: serde::de::DeserializeOwned>(stream: &mut UnixStream) -> Result<T, String> {
    let length = stream.read_u32().await.map_err(|error| error.to_string())? as usize;
    if length > MAX_CHECKPOINT_BYTES {
        return Err("Worker upgrade frame is too large".into());
    }
    let mut bytes = vec![0; length];
    stream
        .read_exact(&mut bytes)
        .await
        .map_err(|error| error.to_string())?;
    serde_json::from_slice(&bytes).map_err(|error| error.to_string())
}
async fn write<T: Serialize>(stream: &mut UnixStream, value: &T) -> Result<(), String> {
    let bytes = serde_json::to_vec(value).map_err(|error| error.to_string())?;
    if bytes.len() > MAX_CHECKPOINT_BYTES {
        return Err("Worker upgrade checkpoint is too large".into());
    }
    stream
        .write_u32(bytes.len() as u32)
        .await
        .map_err(|error| error.to_string())?;
    stream
        .write_all(&bytes)
        .await
        .map_err(|error| error.to_string())
}

pub(crate) async fn serve(
    listener: UnixListener,
    access: Arc<EmbeddedAgentAccess>,
    capability: String,
    retired: CancellationToken,
) {
    loop {
        let Ok((mut stream, _)) = listener.accept().await else {
            return;
        };
        if peer_uid(&stream).ok() != Some(unsafe { libc::geteuid() }) {
            continue;
        }
        let request = tokio::time::timeout(HANDOFF_TIMEOUT, read::<Request>(&mut stream)).await;
        let Ok(Ok(request)) = request else {
            continue;
        };
        if !same_secret(&request.capability, &capability) {
            continue;
        }
        let should_retire = matches!(request.operation, Operation::Retire);
        let reply = match request.operation {
            Operation::Prepare => match access.prepare_upgrade().await {
                Ok(checkpoint) => Reply::Prepared(checkpoint.map(Box::new)),
                Err(error) => Reply::Error(error.to_string()),
            },
            Operation::Cancel => {
                access.cancel_upgrade().await;
                Reply::Done
            }
            Operation::Retire => {
                access.shutdown_for_upgrade().await;
                Reply::Done
            }
        };
        let written = write(&mut stream, &reply).await;
        if should_retire {
            retired.cancel();
            return;
        }
        if written.is_err() {
            access.cancel_upgrade().await;
        }
    }
}

async fn request(worker: &Path, capability: &str, operation: Operation) -> Result<Reply, String> {
    tokio::time::timeout(HANDOFF_TIMEOUT, async {
        let mut stream = UnixStream::connect(endpoint(worker))
            .await
            .map_err(|error| error.to_string())?;
        write(
            &mut stream,
            &Request {
                capability: capability.into(),
                operation,
            },
        )
        .await?;
        read(&mut stream).await
    })
    .await
    .map_err(|_| "Worker upgrade request timed out".to_string())?
}

pub(crate) fn read_checkpoint(fd: i32) -> Result<EmbeddedCheckpoint, String> {
    if fd < 3 || unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
        return Err("invalid Worker checkpoint descriptor".into());
    }
    let mut file = unsafe { File::from_raw_fd(fd) };
    tickloop_agent_client::daemon_endpoint::validate_file(&file, "Worker checkpoint")?;
    if file.metadata().map_err(|error| error.to_string())?.len() > MAX_CHECKPOINT_BYTES as u64 {
        return Err("Worker checkpoint is too large".into());
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    serde_json::from_slice(&bytes).map_err(|error| error.to_string())
}

pub(crate) async fn activate(access: &EmbeddedAgentAccess, fd: i32) -> Result<(), String> {
    if fd < 3 || unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
        return Err("invalid Worker commit descriptor".into());
    }
    let mut channel = upgrade::inherited_stream(fd)?;
    channel
        .write_u8(PREPARED)
        .await
        .map_err(|error| error.to_string())?;
    let commit = tokio::time::timeout(HANDOFF_TIMEOUT, channel.read_u8())
        .await
        .map_err(|_| "Worker handoff commit timed out")?
        .map_err(|error| error.to_string())?;
    if commit != COMMITTED {
        return Err("Worker handoff was not committed".into());
    }
    access.activate_upgrade().await?;
    channel
        .write_u8(ADOPTED)
        .await
        .map_err(|error| error.to_string())?;
    let release = tokio::time::timeout(HANDOFF_TIMEOUT, channel.read_u8())
        .await
        .map_err(|_| "Worker handoff activation timed out")?
        .map_err(|error| error.to_string())?;
    if release != RELEASED {
        return Err("Worker handoff was not activated".into());
    }
    access.release_upgrade().await?;
    channel
        .write_u8(ACTIVATED)
        .await
        .map_err(|error| error.to_string())
}

pub(crate) async fn drive(
    factory: Arc<ProcessWorkerFactory>,
    daemon: Arc<DaemonServer>,
    shutdown: CancellationToken,
) {
    let Ok(executable) = std::env::current_exe().and_then(|path| path.canonicalize()) else {
        return;
    };
    let mut interval = tokio::time::interval(WORKER_UPGRADE_INTERVAL);
    loop {
        tokio::select! { () = shutdown.cancelled() => return, _ = interval.tick() => {} }
        let candidates = factory
            .workers
            .lock()
            .await
            .iter()
            .filter(|worker| worker.executable != executable)
            .map(|worker| worker.endpoint.clone())
            .collect::<Vec<_>>();
        for endpoint in candidates {
            if shutdown.is_cancelled() {
                return;
            }
            rotate(&factory, &daemon, &endpoint, &executable).await;
        }
    }
}

async fn rotate(
    factory: &ProcessWorkerFactory,
    daemon: &DaemonServer,
    endpoint: &Path,
    executable: &Path,
) {
    let Some(_daemon_admission) = daemon.try_pause() else {
        return;
    };
    let mut workers = factory.workers.lock().await;
    let Some(worker) = workers
        .iter_mut()
        .find(|worker| worker.endpoint == endpoint)
    else {
        return;
    };
    if let Err(error) = rotate_worker(factory, worker, endpoint, executable).await {
        worker.upgrade_error = Some(error);
    }
}

async fn rotate_worker(
    factory: &ProcessWorkerFactory,
    worker: &mut crate::ManagedWorker,
    endpoint: &Path,
    executable: &Path,
) -> Result<(), String> {
    if worker.preparation_uncertain {
        if !cancel_preparation(endpoint, &worker.grant.capability, worker.pid).await {
            return Err("waiting to cancel an unconfirmed Worker preparation".into());
        }
        worker.preparation_uncertain = false;
    }
    let mut pending = {
        worker.preparation_uncertain = true;
        let prepared = request(endpoint, &worker.grant.capability, Operation::Prepare).await;
        let checkpoint = match prepared {
            Ok(Reply::Prepared(Some(checkpoint))) => {
                worker.upgrade_error = None;
                checkpoint
            }
            Ok(Reply::Prepared(None)) => {
                worker.preparation_uncertain = false;
                return Ok(());
            }
            response => {
                if cancel_preparation(endpoint, &worker.grant.capability, worker.pid).await {
                    worker.preparation_uncertain = false;
                }
                return Err(match response {
                    Ok(Reply::Error(error)) | Err(error) => error,
                    _ => "Worker returned an invalid prepare reply".into(),
                });
            }
        };
        match prepare_record(factory, worker, executable, &checkpoint) {
            Ok(pending) => {
                worker.preparation_uncertain = false;
                pending
            }
            Err(error) => {
                worker.preparation_uncertain =
                    !cancel_preparation(endpoint, &worker.grant.capability, worker.pid).await;
                return Err(error);
            }
        }
    };
    loop {
        match advance(factory, worker, &mut pending).await {
            Ok(true) => {
                pending.cleanup();
                return Ok(());
            }
            Ok(false) => {}
            Err(error) if pending.record.phase == Phase::Prepared => {
                if let Some(candidate) = pending.candidate.as_mut() {
                    let _ = candidate.process.start_kill();
                }
                worker.preparation_uncertain =
                    !cancel_preparation(endpoint, &worker.grant.capability, worker.pid).await;
                pending.cleanup();
                return Err(error);
            }
            Err(error)
                if pending.record.phase == Phase::Activated && pending.candidate.is_none() =>
            {
                return Err(error);
            }
            Err(error) => worker.upgrade_error = Some(error),
        }
        // Inputs remain at the admission barrier until this committed ownership transfer settles.
        tokio::time::sleep(WORKER_UPGRADE_INTERVAL).await;
    }
}

async fn cancel_preparation(endpoint: &Path, capability: &str, pid: u32) -> bool {
    loop {
        if !upgrade::process_exists(pid)
            || matches!(
                request(endpoint, capability, Operation::Cancel).await,
                Ok(Reply::Done)
            )
        {
            return true;
        }
        // A lost Cancel reply does not prove the original Worker resumed; keep its inputs queued.
        tokio::time::sleep(WORKER_UPGRADE_INTERVAL).await;
    }
}

fn prepare_record(
    factory: &ProcessWorkerFactory,
    worker: &crate::ManagedWorker,
    executable: &Path,
    checkpoint: &EmbeddedCheckpoint,
) -> Result<PendingUpgrade, String> {
    let transfer_id = checkpoint.runtime.transfer_id();
    let checkpoint_path = factory
        .directory
        .join(format!("upgrade-{transfer_id}.checkpoint"));
    let mut file =
        tempfile::NamedTempFile::new_in(&factory.directory).map_err(|error| error.to_string())?;
    serde_json::to_writer(&mut file, checkpoint).map_err(|error| error.to_string())?;
    file.flush()
        .and_then(|()| file.as_file().sync_all())
        .map_err(|error| error.to_string())?;
    file.persist_noclobber(&checkpoint_path)
        .map_err(|error| error.to_string())?;
    let pending = PendingUpgrade {
        record: TransferRecord {
            version: tickloop_agent_protocol::RUNTIME_HANDOFF_VERSION,
            transfer_id,
            epoch: worker.grant.daemon_epoch,
            phase: Phase::Prepared,
            previous_endpoint: worker.endpoint.clone(),
            checkpoint: checkpoint_path,
            executable: executable.to_path_buf(),
            successor_endpoint: factory
                .directory
                .join(format!("w-{}.sock", tickloop_ids::generate())),
            successor_pid: None,
        },
        record_path: factory
            .directory
            .join(format!("upgrade-{transfer_id}.json")),
        candidate: None,
    };
    if let Err(error) = pending.persist() {
        pending.cleanup();
        return Err(error);
    }
    Ok(pending)
}

async fn advance(
    factory: &ProcessWorkerFactory,
    worker: &mut crate::ManagedWorker,
    pending: &mut PendingUpgrade,
) -> Result<bool, String> {
    if let Some(candidate) = pending.candidate.as_mut()
        && candidate
            .process
            .try_wait()
            .map_err(|error| error.to_string())?
            .is_some()
    {
        if pending.record.phase == Phase::Activated {
            let failed = pending.candidate.take().unwrap();
            let pid = pending
                .record
                .successor_pid
                .ok_or("activated Worker has no process identity")?;
            {
                let mut address = worker.link.write().unwrap();
                address.endpoint = pending.record.successor_endpoint.clone();
                address.pid = pid;
            }
            let _ = request(
                &worker.endpoint,
                &worker.grant.capability,
                Operation::Retire,
            )
            .await;
            drop(std::mem::replace(&mut worker.lifeline, failed.lifeline));
            worker.process.lock().await.replace(failed.process);
            worker.endpoint = pending.record.successor_endpoint.clone();
            worker.pid = pid;
            worker.executable = pending.record.executable.clone();
            worker.build_id = tickloop_agent_protocol::build_identity().map(str::to_owned);
            return Err("upgraded Worker exited after activation; the committed execution is lost and is not replayed".into());
        }
        pending.candidate.take();
        pending.record.successor_endpoint = factory
            .directory
            .join(format!("w-{}.sock", tickloop_ids::generate()));
    }
    if pending.candidate.is_none() {
        pending.candidate = Some(spawn_replacement(factory, worker, &pending.record).await?);
        pending.record.successor_pid = pending
            .candidate
            .as_ref()
            .and_then(|candidate| candidate.process.id());
        pending.persist()?;
    }
    if pending.record.phase == Phase::Prepared {
        pending.record.phase = Phase::Committed;
        if let Err(error) = pending.persist() {
            pending.record.phase = Phase::Prepared;
            return Err(error);
        }
    }
    let candidate = pending.candidate.as_mut().unwrap();
    if !candidate.commit_sent {
        candidate.commit_sent = true;
        candidate
            .channel
            .write_u8(COMMITTED)
            .await
            .map_err(|error| error.to_string())?;
    }
    if pending.record.phase == Phase::Committed && !candidate.adopted {
        let adopted = tokio::time::timeout(HANDOFF_TIMEOUT, candidate.channel.read_u8()).await;
        match adopted {
                Ok(Ok(ADOPTED)) => candidate.adopted = true,
                Err(_) => return Ok(false),
                _ => return Err("Worker ownership adoption was not confirmed; retaining the frozen execution for recovery".into()),
            }
    }
    let pid = pending
        .record
        .successor_pid
        .ok_or("successor Worker has no process identity")?;
    {
        let mut address = worker.link.write().unwrap();
        address.endpoint = pending.record.successor_endpoint.clone();
        address.pid = pid;
    }
    if worker.endpoint.exists() {
        match request(
            &worker.endpoint,
            &worker.grant.capability,
            Operation::Retire,
        )
        .await
        {
            Ok(Reply::Done) => {}
            _ if !worker.endpoint.exists() => {}
            _ => return Err("previous Worker has not released its execution leases".into()),
        }
    }
    if pending.record.phase != Phase::Activated {
        pending.record.phase = Phase::Activated;
        if let Err(error) = pending.persist() {
            pending.record.phase = Phase::Committed;
            return Err(error);
        }
    }
    let candidate = pending.candidate.as_mut().unwrap();
    if !candidate.release_sent {
        candidate.release_sent = true;
        candidate
            .channel
            .write_u8(RELEASED)
            .await
            .map_err(|error| error.to_string())?;
    }
    let activated = tokio::time::timeout(HANDOFF_TIMEOUT, candidate.channel.read_u8()).await;
    if !matches!(activated, Ok(Ok(ACTIVATED))) {
        return Err(
            "Worker activation is committed; waiting for the same successor without replay".into(),
        );
    }
    let candidate = pending.candidate.take().unwrap();
    let old_lifeline = std::mem::replace(&mut worker.lifeline, candidate.lifeline);
    let old_process = worker.process.lock().await.replace(candidate.process);
    worker.endpoint = pending.record.successor_endpoint.clone();
    worker.pid = pid;
    worker.build_id = tickloop_agent_protocol::build_identity().map(str::to_owned);
    worker.executable_lease = upgrade::retain_executable(&pending.record.executable)
        .map_err(|error| error.to_string())?;
    worker.executable = pending.record.executable.clone();
    worker.upgrade_error = None;
    drop(old_lifeline);
    if let Some(mut old_process) = old_process {
        let _ = tokio::time::timeout(HANDOFF_TIMEOUT, old_process.wait()).await;
    }
    Ok(true)
}

async fn spawn_replacement(
    factory: &ProcessWorkerFactory,
    worker: &crate::ManagedWorker,
    record: &TransferRecord,
) -> Result<Candidate, String> {
    check_socket_length(&record.successor_endpoint)?;
    check_socket_length(&self::endpoint(&record.successor_endpoint))?;
    let file = File::open(&record.checkpoint).map_err(|error| error.to_string())?;
    tickloop_agent_client::daemon_endpoint::validate_file(&file, "Worker upgrade checkpoint")?;
    let (parent, child) =
        std::os::unix::net::UnixStream::pair().map_err(|error| error.to_string())?;
    parent
        .set_nonblocking(true)
        .map_err(|error| error.to_string())?;
    let mut channel = UnixStream::from_std(parent).map_err(|error| error.to_string())?;
    let inherited = [file.as_raw_fd(), child.as_raw_fd()];
    let mut command = Command::new(&record.executable);
    command
        .arg(crate::WORKER_ARGUMENT)
        .arg(&record.successor_endpoint)
        .current_dir(&worker.launch.folder)
        .env_clear();
    for entry in &worker.launch.environment {
        command.env(
            OsString::from_vec(entry.name.clone()),
            OsString::from_vec(entry.value.clone()),
        );
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        command.pre_exec(move || {
            for descriptor in inherited {
                if libc::fcntl(descriptor, libc::F_SETFD, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    let mut process = command.spawn().map_err(|error| error.to_string())?;
    let mut lifeline = process
        .stdin
        .take()
        .ok_or("cannot open successor Worker lifeline")?;
    let bootstrap = WorkerBootstrap {
        epoch: worker.grant.daemon_epoch,
        launch: worker.launch.clone(),
        control: worker.grant.clone(),
        limits: factory.limits,
        checkpoint: Some(file.as_raw_fd()),
        commit: Some(child.as_raw_fd()),
    };
    let mut bytes = serde_json::to_vec(&bootstrap).map_err(|error| error.to_string())?;
    bytes.push(b'\n');
    lifeline
        .write_all(&bytes)
        .await
        .map_err(|error| error.to_string())?;
    lifeline.flush().await.map_err(|error| error.to_string())?;
    let ready = tokio::time::timeout(HANDOFF_TIMEOUT, channel.read_u8())
        .await
        .map_err(|_| "successor Worker did not become ready")?
        .map_err(|error| error.to_string())?;
    if ready != PREPARED {
        return Err("successor Worker rejected handoff".into());
    }
    Ok(Candidate {
        process,
        lifeline: lifeline
            .into_owned_fd()
            .map_err(|error| error.to_string())?,
        channel,
        commit_sent: false,
        adopted: false,
        release_sent: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tickloop_agent_protocol::{AccessErrorCode, AgentAccess, AgentCommand, CommandEnvelope};
    use tickloop_agent_runtime::Agent;
    use tokio::sync::{Mutex, Notify};

    #[tokio::test]
    async fn worker_blockers_survive_busy_retries_and_clear_only_for_the_worker_that_recovers() {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let epoch = Uuid::new_v4();
        let shutdown = CancellationToken::new();
        let control = Arc::new(crate::worker_control::WorkerControl::new(
            directory.path(),
            epoch,
            Arc::new(
                tickloop_agent_runtime::hosted::controller::HostController::in_process(
                    directory.path(),
                    tickhost::paths::HostPaths {
                        runtime: directory.path().join("host"),
                    },
                    epoch,
                    tickhost::host::DEFAULT_IDLE_TIMEOUT,
                ),
            ),
        ));
        let mut workers = Vec::new();
        let mut services = Vec::new();
        let mut states = Vec::new();
        let mut runtimes = Vec::new();
        for label in ["alpha", "beta"] {
            let worker_endpoint = directory.path().join(format!("{label}.sock"));
            let (listener, _) = crate::private_listener(&endpoint(&worker_endpoint)).unwrap();
            let busy = Arc::new(AtomicBool::new(false));
            let attempts = Arc::new(AtomicUsize::new(0));
            states.push((busy.clone(), attempts.clone()));
            services.push(tokio::spawn(async move {
                loop {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let request: Request = read(&mut stream).await.unwrap();
                    let reply = match request.operation {
                        Operation::Prepare => {
                            attempts.fetch_add(1, Ordering::SeqCst);
                            if busy.load(Ordering::SeqCst) {
                                Reply::Prepared(None)
                            } else {
                                Reply::Error(format!("{label} cannot transfer its retained Shell"))
                            }
                        }
                        Operation::Cancel => Reply::Done,
                        Operation::Retire => {
                            panic!("the fixture has no running execution to retire")
                        }
                    };
                    write(&mut stream, &reply).await.unwrap();
                }
            }));
            let runtime = Agent::new_embedded(
                tickloop_agent_test_support::config(),
                Arc::new(tickloop_agent_test_support::ScriptedModel::new(Vec::new())),
            );
            runtimes.push(runtime.clone());
            workers.push(crate::ManagedWorker {
                access: Arc::new(EmbeddedAgentAccess::with_epoch(runtime, epoch)),
                process: Arc::new(Mutex::new(None)),
                lifeline: File::open("/dev/null").unwrap().into(),
                pid: std::process::id(),
                build_id: Some("previous-build".into()),
                endpoint: worker_endpoint.clone(),
                grant: control.grant(),
                launch: tickloop_agent_protocol::LaunchSpec::embedded(directory.path().into()),
                executable: directory.path().join("previous-runtime"),
                link: Arc::new(std::sync::RwLock::new(upgrade::WorkerAddress {
                    endpoint: worker_endpoint,
                    pid: std::process::id(),
                })),
                preparation_uncertain: false,
                upgrade_error: None,
                executable_lease: None,
            });
        }
        let execution = || tickloop_agent_protocol::ExecutionRef {
            daemon_epoch: epoch,
            agent_id: Uuid::new_v4(),
            generation: 1,
        };
        let root = execution();
        let child = execution();
        let unrelated = execution();
        let routes = vec![
            (root, workers[0].access.clone()),
            (child, workers[0].access.clone()),
            (unrelated, workers[1].access.clone()),
        ];
        let factory = Arc::new(ProcessWorkerFactory {
            directory: directory.path().into(),
            listener_identity: (0, 0),
            control_identity: (0, 0),
            limits: tickloop_agent_test_support::TRANSPORT_LIMITS,
            workers: Mutex::new(workers),
            shutting_down: AtomicBool::new(false),
            control,
            upgrade: Mutex::new(upgrade::UpgradeState::default()),
            upgrade_requested: Notify::new(),
            lifecycle: Arc::new(crate::lifecycle::DaemonLifecycle::new(shutdown.clone())),
            ingress: Arc::new(crate::ingress::DaemonIngress::new(
                epoch,
                tickloop_agent_test_support::TRANSPORT_LIMITS,
            )),
        });
        let daemon = Arc::new(DaemonServer::new(epoch, factory.clone()));
        let driver = tokio::spawn(drive(factory.clone(), daemon, shutdown.clone()));
        tokio::time::timeout(Duration::from_secs(3), async {
            while states
                .iter()
                .any(|(_, attempts)| attempts.load(Ordering::SeqCst) == 0)
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let version = factory.version(&routes).await;
        assert!(
            version.error.is_none(),
            "Worker errors must not become global notices"
        );
        assert!(
            version.workers[0]
                .error
                .as_deref()
                .unwrap()
                .contains("alpha")
        );
        assert!(
            version.workers[1]
                .error
                .as_deref()
                .unwrap()
                .contains("beta")
        );
        assert_eq!(version.workers[0].executions, [root, child]);
        assert_eq!(version.workers[1].executions, [unrelated]);
        assert!(
            factory
                .version(&[])
                .await
                .workers
                .iter()
                .all(|worker| worker.executions.is_empty())
        );

        let before_busy = states[0].1.load(Ordering::SeqCst);
        states[0].0.store(true, Ordering::SeqCst);
        tokio::time::timeout(Duration::from_secs(3), async {
            while states[0].1.load(Ordering::SeqCst) <= before_busy {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let version = factory.version(&routes).await;
        assert!(version.error.is_none());
        assert!(
            version.workers[0]
                .error
                .as_deref()
                .unwrap()
                .contains("alpha"),
            "busy is not proof the blocker was resolved"
        );
        assert!(
            version.workers[1]
                .error
                .as_deref()
                .unwrap()
                .contains("beta")
        );
        shutdown.cancel();
        driver.await.unwrap();

        let (channel, mut peer) = UnixStream::pair().unwrap();
        let activation = tokio::spawn(async move {
            assert_eq!(peer.read_u8().await.unwrap(), RELEASED);
            peer.write_u8(ACTIVATED).await.unwrap();
        });
        let process = Command::new("/bin/sleep")
            .arg("30")
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut workers = factory.workers.lock().await;
        let mut pending = PendingUpgrade {
            record: TransferRecord {
                version: tickloop_agent_protocol::RUNTIME_HANDOFF_VERSION,
                transfer_id: Uuid::new_v4(),
                epoch,
                phase: Phase::Activated,
                previous_endpoint: workers[0].endpoint.clone(),
                checkpoint: directory.path().join("checkpoint.json"),
                executable: std::env::current_exe().unwrap(),
                successor_endpoint: directory.path().join("alpha-successor.sock"),
                successor_pid: process.id(),
            },
            record_path: directory.path().join("handoff.json"),
            candidate: Some(Candidate {
                process,
                lifeline: File::open("/dev/null").unwrap().into(),
                channel,
                commit_sent: true,
                adopted: true,
                release_sent: false,
            }),
        };
        assert!(
            advance(&factory, &mut workers[0], &mut pending)
                .await
                .unwrap()
        );
        drop(workers);
        activation.await.unwrap();
        let version = factory.version(&routes).await;
        assert!(version.error.is_none());
        assert!(version.workers[0].error.is_none());
        assert!(
            version.workers[1]
                .error
                .as_deref()
                .unwrap()
                .contains("beta"),
            "recovering alpha must not clear beta's blocker"
        );
        assert_eq!(version.workers[0].executions, [root, child]);
        assert!(!version.workers[0].upgrading);
        factory.workers.lock().await.remove(1);
        assert!(
            factory
                .version(&routes)
                .await
                .workers
                .iter()
                .all(|worker| worker.error.is_none()),
            "removed Worker blockers must not remain in runtime status"
        );
        factory.workers.lock().await.clear();
        for service in services {
            service.abort();
        }
        for runtime in runtimes {
            runtime.shutdown().await;
        }
    }

    #[tokio::test]
    async fn a_lost_preparation_reply_is_cancelled_before_retrying() {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let worker = directory.path().join("worker.sock");
        let epoch = Uuid::new_v4();
        let agent = Agent::new_embedded(
            tickloop_agent_test_support::config(),
            Arc::new(tickloop_agent_test_support::ScriptedModel::new(Vec::new())),
        );
        let access = Arc::new(EmbeddedAgentAccess::with_epoch(agent, epoch));
        let (listener, _) = crate::private_listener(&endpoint(&worker)).unwrap();
        let service = tokio::spawn(serve(
            listener,
            access.clone(),
            "private-handoff".into(),
            CancellationToken::new(),
        ));
        let mut lost = UnixStream::connect(endpoint(&worker)).await.unwrap();
        write(
            &mut lost,
            &Request {
                capability: "private-handoff".into(),
                operation: Operation::Prepare,
            },
        )
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let observed = access
                    .execute(CommandEnvelope {
                        daemon_epoch: epoch,
                        command_id: Uuid::new_v4(),
                        command: AgentCommand::FlushTranscripts,
                    })
                    .await;
                if matches!(observed, Err(error) if error.code == AccessErrorCode::Unavailable) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        drop(lost);
        assert!(cancel_preparation(&worker, "private-handoff", std::process::id()).await);
        access
            .execute(CommandEnvelope {
                daemon_epoch: epoch,
                command_id: Uuid::new_v4(),
                command: AgentCommand::FlushTranscripts,
            })
            .await
            .unwrap()
            .into_result()
            .unwrap();
        assert!(cancel_preparation(&worker, "private-handoff", std::process::id()).await);
        service.abort();
        access.shutdown().await;
    }
}

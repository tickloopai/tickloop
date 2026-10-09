//! The daemon and worker processes: the daemon supervises workers and serves AgentAccess to
//! Views, and each worker serves one embedded Agent runtime to its daemon.

use std::{
    ffi::OsString,
    fs::File,
    io,
    os::{
        fd::FromRawFd,
        unix::{
            ffi::OsStringExt,
            fs::{MetadataExt, PermissionsExt},
        },
    },
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixListener,
    process::{Child, ChildStdin, Command},
    sync::Mutex,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use tickloop_agent_client::{
    LocalAgentClient,
    daemon_endpoint::{
        CONNECT_INTERVAL, LOCK_NAME, SOCKET_NAME, STARTUP_DEADLINE, check_socket_length,
        remove_stale_socket, validate_directory, validate_file,
    },
    worker_link,
};
use tickloop_agent_config::UserSettings;
use tickloop_agent_model::{ModelClient, cards::launch_model, model_client};
use tickloop_agent_protocol::{
    AccessError, AccessErrorCode, AccessLimits, AgentAccess, LaunchSpec,
};
use tickloop_agent_runtime::Agent;

mod chunked;
pub mod daemon_control;
mod daemon_management;
mod daemon_server;
mod embedded;
mod ipc;
mod receipts;
mod summary;
mod views;
pub mod worker_control;

pub use daemon_server::{DaemonServer, WorkerFactory};
pub use embedded::{EmbeddedAgentAccess, ModelClientFactory};
pub use ipc::{serve_connection, serve_daemon_listener, serve_listener, serve_worker_listener};

/// How often a daemon checks that clients can still reach it through its runtime directory.
const RUNTIME_CHECK_INTERVAL: Duration = Duration::from_secs(1);

/// The hidden process entry of a worker: it does not appear in `--help`, and names its role in
/// `ps`.
#[doc(hidden)]
pub const WORKER_ARGUMENT: &str = "worker";
/// A bootstrap line carries the launch environment as JSON byte arrays, up to four characters
/// for each byte of an environment the operating system already bounds.
#[doc(hidden)]
pub const WORKER_BOOTSTRAP_MAX_BYTES: usize = 16 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
struct WorkerBootstrap {
    epoch: Uuid,
    launch: LaunchSpec,
    /// The worker's private grant for hosted Tick requests to its daemon.
    control: tickloop_agent_runtime::hosted::worker_control::WorkerControlGrant,
    /// The transport limits the worker serves its daemon with.
    limits: AccessLimits,
}

struct ManagedWorker {
    access: Arc<dyn AgentAccess>,
    process: Arc<Mutex<Child>>,
    lifeline: ChildStdin,
}

struct ProcessWorkerFactory {
    directory: PathBuf,
    limits: AccessLimits,
    workers: Mutex<Vec<ManagedWorker>>,
    shutting_down: AtomicBool,
    control: Arc<worker_control::WorkerControl>,
}

impl ProcessWorkerFactory {
    async fn shutdown(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        let workers = std::mem::take(&mut *self.workers.lock().await);
        let mut exits = tokio::task::JoinSet::new();
        for worker in workers {
            drop(worker.lifeline);
            exits.spawn(async move {
                let mut process = worker.process.lock().await;
                if tokio::time::timeout(STARTUP_DEADLINE, process.wait())
                    .await
                    .is_err()
                {
                    let _ = process.start_kill();
                    let _ = process.wait().await;
                }
            });
        }
        while exits.join_next().await.is_some() {}
    }
}

#[async_trait]
impl WorkerFactory for ProcessWorkerFactory {
    async fn start(
        &self,
        launch: &LaunchSpec,
        daemon_epoch: Uuid,
    ) -> Result<Arc<dyn AgentAccess>, AccessError> {
        if self.shutting_down.load(Ordering::SeqCst) {
            return Err(AccessError::unavailable("Agent daemon is shutting down"));
        }
        let endpoint = self.directory.join(format!("w-{}.sock", Uuid::new_v4()));
        check_socket_length(&endpoint).map_err(AccessError::unavailable)?;
        let executable =
            std::env::current_exe().map_err(|error| AccessError::unavailable(error.to_string()))?;
        let mut command = Command::new(executable);
        command
            .arg(WORKER_ARGUMENT)
            .arg(&endpoint)
            .current_dir(&launch.workdir)
            .env_clear();
        for entry in &launch.environment {
            command.env(
                OsString::from_vec(entry.name.clone()),
                OsString::from_vec(entry.value.clone()),
            );
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut process = command
            .spawn()
            .map_err(|error| AccessError::unavailable(error.to_string()))?;
        let mut lifeline = process
            .stdin
            .take()
            .ok_or_else(|| AccessError::unavailable("cannot open worker bootstrap pipe"))?;
        let grant = self.control.grant();
        let mut bootstrap = serde_json::to_vec(&WorkerBootstrap {
            epoch: daemon_epoch,
            launch: launch.clone(),
            control: grant.clone(),
            limits: self.limits,
        })
        .map_err(|error| AccessError::invalid(error.to_string()))?;
        bootstrap.push(b'\n');
        lifeline
            .write_all(&bootstrap)
            .await
            .map_err(|error| AccessError::unavailable(error.to_string()))?;
        lifeline
            .flush()
            .await
            .map_err(|error| AccessError::unavailable(error.to_string()))?;
        let deadline = Instant::now() + STARTUP_DEADLINE;
        loop {
            if let Some(status) = process
                .try_wait()
                .map_err(|error| AccessError::unavailable(error.to_string()))?
            {
                return Err(AccessError::unavailable(format!(
                    "Agent worker exited during startup ({status})"
                )));
            }
            if endpoint.exists() {
                match tokio::time::timeout(STARTUP_DEADLINE, LocalAgentClient::connect(&endpoint))
                    .await
                {
                    Ok(Ok(client)) => {
                        if client.daemon_epoch() != daemon_epoch {
                            return Err(AccessError::unavailable(
                                "worker belongs to another daemon epoch",
                            ));
                        }
                        let mut workers = self.workers.lock().await;
                        if self.shutting_down.load(Ordering::SeqCst) {
                            drop(lifeline);
                            let _ = tokio::time::timeout(STARTUP_DEADLINE, process.wait()).await;
                            return Err(AccessError::unavailable(
                                "Agent daemon stopped during worker startup",
                            ));
                        }
                        let process = Arc::new(Mutex::new(process));
                        let running = process.clone();
                        // A worker that shutdown is waiting on counts as stopped, so no link is rebuilt.
                        let access: Arc<dyn AgentAccess> =
                            Arc::new(worker_link(client, endpoint, move || {
                                running
                                    .try_lock()
                                    .is_ok_and(|mut process| matches!(process.try_wait(), Ok(None)))
                            }));
                        workers.push(ManagedWorker {
                            access: access.clone(),
                            process,
                            lifeline,
                        });
                        self.control
                            .register(&grant, access.clone(), launch.config_root.as_deref())
                            .await;
                        return Ok(access);
                    }
                    Ok(Err(error)) if error.code == AccessErrorCode::VersionMismatch => {
                        return Err(error);
                    }
                    Ok(Err(error)) if Instant::now() >= deadline => return Err(error),
                    Err(_) => {
                        return Err(AccessError::unavailable("Agent worker handshake timed out"));
                    }
                    _ => {}
                }
            }
            if Instant::now() >= deadline {
                return Err(AccessError::unavailable(
                    "Agent worker did not become ready",
                ));
            }
            tokio::time::sleep(CONNECT_INTERVAL).await;
        }
    }
    async fn exited(&self, worker: &Arc<dyn AgentAccess>) -> bool {
        let process = self
            .workers
            .lock()
            .await
            .iter()
            .find(|managed| Arc::ptr_eq(&managed.access, worker))
            .map(|managed| managed.process.clone());
        let exited = match process {
            Some(process) => matches!(process.lock().await.try_wait(), Ok(Some(_))),
            None => false,
        };
        if exited {
            self.control.revoke(worker).await;
        }
        exited
    }
}

pub async fn run_daemon(arguments: &[OsString]) -> Result<(), String> {
    let [directory, descriptor] = arguments else {
        return Err("invalid private daemon invocation".into());
    };
    let directory = PathBuf::from(directory);
    validate_directory(&directory)?;
    let descriptor = descriptor
        .to_str()
        .and_then(|value| value.parse::<i32>().ok())
        .filter(|value| *value >= 3)
        .ok_or("invalid daemon lock descriptor")?;
    let lock = unsafe { File::from_raw_fd(descriptor) };
    validate_file(&lock, "Agent daemon lock")?;
    let lock_path = directory.join(LOCK_NAME);
    validate_daemon_lock_path(&lock, &lock_path)?;
    if unsafe { libc::flock(descriptor, libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err("Agent daemon lock is not held by this process".into());
    }
    if unsafe { libc::fcntl(descriptor, libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
        return Err("cannot isolate daemon lock from workers".into());
    }
    let endpoint = directory.join(SOCKET_NAME);
    let (listener, socket_identity) = private_listener(&endpoint)?;
    let epoch = Uuid::new_v4();
    let tickhost = std::env::current_exe()
        .and_then(|executable| executable.canonicalize())
        .map_err(|error| error.to_string())?
        .with_file_name(tickloop_agent_runtime::hosted::controller::TICKHOST_EXECUTABLE);
    let controller = Arc::new(
        tickloop_agent_runtime::hosted::controller::HostController::new(
            &directory,
            tickhost::paths::HostPaths::from_environment(),
            epoch,
            tickhost,
            tickhost::host::DEFAULT_IDLE_TIMEOUT,
        ),
    );
    let control = Arc::new(worker_control::WorkerControl::new(
        &directory, epoch, controller,
    ));
    let control_endpoint = control.endpoint().to_path_buf();
    // The daemon lock proves no other daemon serves this endpoint; a socket left by a daemon that
    // was killed is stale.
    remove_stale_socket(&control_endpoint)?;
    let (control_listener, control_identity) = private_listener(&control_endpoint)?;
    let limits = AccessLimits::DEFAULT;
    let factory = Arc::new(ProcessWorkerFactory {
        directory: directory.clone(),
        limits,
        workers: Mutex::new(Vec::new()),
        shutting_down: AtomicBool::new(false),
        control: control.clone(),
    });
    let daemon = Arc::new(DaemonServer::new(epoch, factory.clone()));
    control.attach_daemon((*daemon).clone());
    let control_service = tokio::spawn(control.clone().serve(control_listener));
    let shutdown = CancellationToken::new();
    let service = async {
        let result = serve_daemon_listener(listener, daemon, epoch, limits, shutdown.clone()).await;
        shutdown.cancel();
        result
    };
    let (result, step_down) = tokio::join!(
        service,
        step_down_when_unreachable(&endpoint, socket_identity, &lock, &lock_path, &shutdown)
    );
    factory.shutdown().await;
    control_service.abort();
    let cleanup = validate_daemon_lock_path(&lock, &lock_path)
        .and_then(|()| remove_owned_socket(&endpoint, socket_identity))
        .and_then(|()| remove_owned_socket(&control_endpoint, control_identity));
    drop(lock);
    if let Some(reason) = step_down {
        return Err(reason);
    }
    cleanup?;
    result.map_err(|error| error.to_string())
}

/// Waits until clients can no longer reach this daemon, then shuts it down as `ShutdownDaemon`
/// would and returns why. Returns `None` once the daemon shuts down for another reason.
async fn step_down_when_unreachable(
    endpoint: &Path,
    socket_identity: (u64, u64),
    lock: &File,
    lock_path: &Path,
    shutdown: &CancellationToken,
) -> Option<String> {
    loop {
        tokio::select! {
            () = shutdown.cancelled() => return None,
            () = tokio::time::sleep(RUNTIME_CHECK_INTERVAL) => {}
        }
        if let Some(reason) = unreachable_reason(endpoint, socket_identity, lock, lock_path) {
            shutdown.cancel();
            return Some(reason);
        }
    }
}

/// A daemon is unreachable once its socket is gone or replaced, or once another daemon replaced
/// its lock. A lock that is only missing keeps the daemon serving, so a temporary-file cleaner
/// that removed it does not end running Agents.
fn unreachable_reason(
    endpoint: &Path,
    socket_identity: (u64, u64),
    lock: &File,
    lock_path: &Path,
) -> Option<String> {
    let socket_is_published = std::fs::symlink_metadata(endpoint)
        .is_ok_and(|metadata| (metadata.dev(), metadata.ino()) == socket_identity);
    if !socket_is_published {
        return Some(
            "Agent daemon endpoint changed or was removed; the daemon stepped down.".into(),
        );
    }
    let lock_is_missing = matches!(
        std::fs::symlink_metadata(lock_path),
        Err(error) if error.kind() == io::ErrorKind::NotFound
    );
    if lock_is_missing {
        return None;
    }
    validate_daemon_lock_path(lock, lock_path)
        .is_err()
        .then(|| "Agent daemon lock changed; the daemon stepped down.".into())
}

pub async fn run_worker(arguments: &[OsString]) -> Result<(), String> {
    let [endpoint] = arguments else {
        return Err("invalid private worker invocation".into());
    };
    let endpoint = PathBuf::from(endpoint);
    let directory = endpoint.parent().ok_or("worker socket has no directory")?;
    validate_directory(directory)?;
    let mut stdin = BufReader::new(tokio::io::stdin());
    let mut bytes = Vec::new();
    (&mut stdin)
        .take(WORKER_BOOTSTRAP_MAX_BYTES as u64 + 1)
        .read_until(b'\n', &mut bytes)
        .await
        .map_err(|error| error.to_string())?;
    if bytes.len() > WORKER_BOOTSTRAP_MAX_BYTES {
        return Err("worker bootstrap is too large".into());
    }
    let bootstrap: WorkerBootstrap =
        serde_json::from_slice(&bytes).map_err(|_| "invalid worker bootstrap")?;
    let agent = build_runtime(&bootstrap.launch, Some(bootstrap.control.clone())).await?;
    let access = Arc::new(EmbeddedAgentAccess::for_worker(
        agent.clone(),
        bootstrap.epoch,
    ));
    let (listener, socket_identity) = private_listener(&endpoint)?;
    let mut probe = [0u8; 1];
    let result = tokio::select! {
        result = serve_worker_listener(listener, access, bootstrap.epoch, bootstrap.limits) => {
            result.map_err(|error| error.to_string())
        }
        result = stdin.read(&mut probe) => result.map(|_| ()).map_err(|error| error.to_string()),
    };
    agent.shutdown().await;
    remove_owned_socket(&endpoint, socket_identity)?;
    result
}

pub async fn build_runtime(
    launch: &LaunchSpec,
    control: Option<tickloop_agent_runtime::hosted::worker_control::WorkerControlGrant>,
) -> Result<Agent, String> {
    let model = launch_model(launch);
    let client =
        model_client(model, launch.config_root.as_deref()).map_err(|error| error.to_string())?;
    build_runtime_with_client(launch, client, control).await
}

/// Build the same version-owned runtime with a caller-supplied model transport.
/// Integration fixtures use this seam without adding definition-file loading to the CLI.
#[doc(hidden)]
pub async fn build_runtime_with_client(
    launch: &LaunchSpec,
    client: Arc<dyn ModelClient>,
    control: Option<tickloop_agent_runtime::hosted::worker_control::WorkerControlGrant>,
) -> Result<Agent, String> {
    let model = launch_model(launch);
    let settings = launch
        .config_root
        .as_ref()
        .map(|root| UserSettings::in_root(root));
    let agent = if launch.session_persistence {
        let root = launch
            .config_root
            .as_ref()
            .ok_or("saved sessions require a config root")?;
        Agent::open_with_user_settings_and_options(
            root.clone(),
            model,
            client,
            settings.expect("settings root"),
            launch.host_options,
        )
    } else if let Some(settings) = settings {
        Ok(Agent::new_with_user_settings_and_options(
            model,
            client,
            settings,
            launch.host_options,
        ))
    } else {
        Ok(Agent::new_with_options(model, client, launch.host_options))
    }
    .map_err(|error| error.to_string())?;
    use tickloop_agent_runtime::tick_adapter::TickAdapters;
    let access = launch.host_work_context.as_ref().map(|work| {
        Arc::new(tickhost_connect::access::TickAccess::new(
            tickhost_connect::herdr::HerdrContext {
                socket: work.socket.clone(),
                caller_pane: String::new(),
            },
        ))
    });
    if let (Some(grant), Some(_)) = (&control, &launch.config_root) {
        agent.install_stopped_child_hook(Arc::new(
            tickloop_agent_runtime::hosted::lifecycle::DaemonLifecycle::new(grant.clone()),
        ));
    }
    // Every worker the daemon granted a Tickhost opens hidden Ticks there, with its host's own
    // authority; a worker in a Herdr View also keeps Herdr Workspaces and visible Panes. Each
    // Tick tool call is routed by its target to one of the two.
    let hosted = control.map(|grant| {
        Arc::new(tickloop_agent_runtime::hosted::tools::HostedTickTools::new(
            tickloop_agent_runtime::hosted::worker_control::DaemonControl::new(grant),
            launch
                .environment
                .iter()
                .filter_map(|entry| {
                    Some((
                        String::from_utf8(entry.name.clone()).ok()?,
                        String::from_utf8(entry.value.clone()).ok()?,
                    ))
                })
                .collect(),
        ))
    });
    let herdr = access.as_ref().map(|access| {
        Arc::new(tickloop_agent_runtime::native_host::NativeHostToolProvider::new(access.clone()))
    });
    let native_host = (herdr.is_some() || hosted.is_some()).then(|| {
        Arc::new(
            tickloop_agent_runtime::tick_tool_router::TickToolRouter::new(herdr, hosted.clone()),
        ) as Arc<dyn tickloop_agent_runtime::host_tools::HostToolProvider>
    });
    if let Some(hosted) = &hosted {
        agent.install_hosted_ticks(hosted.clone());
    }
    let adapters = Arc::new(TickAdapters::builtin(
        agent.clone(),
        launch.config_root.clone(),
        access,
        hosted,
    ));
    agent.install_tick_stream(adapters.clone()).await;
    if let Some(root) = launch.config_root.as_ref()
        && let Ok(artifact) = tickloop_agent_artifacts::ArtifactClient::new(root)
    {
        agent
            .install_host_tools(Arc::new(
                tickloop_agent_runtime::mcp_tools::McpHostToolProvider::new(
                    artifact,
                    native_host.clone(),
                ),
            ))
            .map_err(|error| format!("cannot install MCP tools: {error}"))?;
    } else if let Some(native_host) = native_host {
        agent
            .install_host_tools(native_host)
            .map_err(|error| format!("cannot install Tick tools: {error}"))?;
    }
    Ok(agent)
}

fn validate_daemon_lock_path(lock: &File, path: &Path) -> Result<(), String> {
    let inherited = lock.metadata().map_err(|error| error.to_string())?;
    let current = std::fs::symlink_metadata(path).map_err(|error| {
        format!("Agent daemon lock path changed; endpoint was preserved: {error}")
    })?;
    if !current.file_type().is_file()
        || current.uid() != unsafe { libc::geteuid() }
        || current.mode() & 0o777 != 0o600
        || inherited.dev() != current.dev()
        || inherited.ino() != current.ino()
    {
        return Err("Agent daemon lock changed; endpoint was preserved.".into());
    }
    Ok(())
}

fn remove_owned_socket(endpoint: &Path, identity: (u64, u64)) -> Result<(), String> {
    let metadata = match std::fs::symlink_metadata(endpoint) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("Cannot inspect owned Agent endpoint: {error}")),
    };
    if (metadata.dev(), metadata.ino()) != identity {
        return Err("Agent endpoint changed during cleanup; replacement was preserved.".into());
    }
    std::fs::remove_file(endpoint)
        .map_err(|error| format!("Cannot remove owned Agent endpoint: {error}"))
}

fn private_listener(endpoint: &Path) -> Result<(UnixListener, (u64, u64)), String> {
    const TEMPORARY_SOCKET_NAME_LENGTH: usize = 8;
    check_socket_length(endpoint)?;
    let temporary = endpoint
        .with_file_name(&Uuid::new_v4().simple().to_string()[..TEMPORARY_SOCKET_NAME_LENGTH]);
    let listener = UnixListener::bind(&temporary).map_err(|error| error.to_string())?;
    let metadata = std::fs::symlink_metadata(&temporary).map_err(|error| error.to_string())?;
    let identity = (metadata.dev(), metadata.ino());
    // Publish only after permissions are private; linking never overwrites a replacement.
    let published = std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600))
        .and_then(|()| std::fs::hard_link(&temporary, endpoint));
    remove_owned_socket(&temporary, identity)?;
    published.map_err(|error| error.to_string())?;
    Ok((listener, identity))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tickloop_agent_client::daemon_endpoint::validate_socket;

    #[tokio::test]
    async fn a_published_listener_is_private_and_accepts_connections() {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let endpoint = directory.path().join("daemon.sock");
        let (listener, identity) = private_listener(&endpoint).unwrap();
        validate_socket(&endpoint).unwrap();
        let client = tokio::net::UnixStream::connect(&endpoint).await.unwrap();
        let (mut server, _) = listener.accept().await.unwrap();
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut client = client;
        client.write_all(b"private").await.unwrap();
        let mut received = [0; 7];
        server.read_exact(&mut received).await.unwrap();
        assert_eq!(&received, b"private");
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
        remove_owned_socket(&endpoint, identity).unwrap();
    }

    #[tokio::test]
    async fn publishing_a_listener_preserves_an_existing_endpoint() {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let endpoint = directory.path().join("daemon.sock");
        std::fs::write(&endpoint, "replacement").unwrap();
        assert!(private_listener(&endpoint).is_err());
        assert_eq!(std::fs::read_to_string(&endpoint).unwrap(), "replacement");
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }
}

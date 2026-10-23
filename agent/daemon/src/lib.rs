//! The daemon and worker processes: the daemon supervises workers and serves AgentAccess to
//! Views, and each worker serves one embedded Agent runtime to its daemon.

use std::{
    ffi::OsString,
    fs::File,
    io,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
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
    process::{Child, Command},
    sync::{Mutex, Notify},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use tickloop_agent_client::{
    LocalAgentClient,
    daemon_endpoint::{
        CONNECT_INTERVAL, LOCK_NAME, SOCKET_NAME, STARTUP_DEADLINE, check_socket_length,
        remove_stale_socket, validate_directory, validate_file,
    },
};
use tickloop_agent_config::UserSettings;
use tickloop_agent_model::{LocalModelClients, ModelClients, cards::launch_model};
use tickloop_agent_protocol::{
    AccessError, AccessErrorCode, AccessLimits, AgentAccess, LaunchSpec, OtherTickhost,
};
use tickloop_agent_runtime::{
    Agent, HarnessOptions, HarnessSeams, ToolPolicy, environment::LocalEnvironment,
};

mod chunked;
pub mod daemon_control;
mod daemon_management;
mod daemon_server;
mod embedded;
mod ingress;
mod ipc;
mod lifecycle;
mod receipts;
#[cfg(test)]
mod status_cost;
mod summary;
mod upgrade;
mod views;
pub mod worker_control;
mod worker_upgrade;

pub use daemon_server::{DaemonServer, WorkerFactory};
pub use embedded::EmbeddedAgentAccess;
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
    #[serde(default)]
    checkpoint: Option<i32>,
    #[serde(default)]
    commit: Option<i32>,
}

struct ManagedWorker {
    access: Arc<dyn AgentAccess>,
    process: Arc<Mutex<Option<Child>>>,
    lifeline: OwnedFd,
    pid: u32,
    build_id: Option<String>,
    endpoint: PathBuf,
    grant: tickloop_agent_runtime::hosted::worker_control::WorkerControlGrant,
    launch: LaunchSpec,
    executable: PathBuf,
    link: Arc<std::sync::RwLock<upgrade::WorkerAddress>>,
    preparation_uncertain: bool,
    upgrade_error: Option<String>,
    executable_lease: Option<tickhost_connect::executable_lease::ExecutableLease>,
}

struct ProcessWorkerFactory {
    directory: PathBuf,
    listener_identity: (u64, u64),
    control_identity: (u64, u64),
    limits: AccessLimits,
    workers: Mutex<Vec<ManagedWorker>>,
    shutting_down: AtomicBool,
    control: Arc<worker_control::WorkerControl>,
    upgrade: Mutex<upgrade::UpgradeState>,
    upgrade_requested: Notify,
    lifecycle: Arc<lifecycle::DaemonLifecycle>,
    ingress: Arc<ingress::DaemonIngress>,
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
                if let Some(process) = process.as_mut()
                    && tokio::time::timeout(STARTUP_DEADLINE, process.wait())
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
    async fn request_upgrade(&self, executable: &Path) -> Result<(), AccessError> {
        self.select_upgrade(executable).await
    }

    async fn runtime_version(
        &self,
        routes: &[(tickloop_agent_protocol::ExecutionRef, Arc<dyn AgentAccess>)],
    ) -> tickloop_agent_protocol::RuntimeVersion {
        self.version(routes).await
    }
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
        check_socket_length(&worker_upgrade::endpoint(&endpoint))
            .map_err(AccessError::unavailable)?;
        let executable = std::env::current_exe()
            .and_then(|path| path.canonicalize())
            .map_err(|error| AccessError::unavailable(error.to_string()))?;
        let mut command = Command::new(&executable);
        command
            .arg(WORKER_ARGUMENT)
            .arg(&endpoint)
            .current_dir(&launch.folder)
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
            .stderr(Stdio::null());
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
            checkpoint: None,
            commit: None,
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
                        let pid = process.id().ok_or_else(|| {
                            AccessError::unavailable("worker exited during startup")
                        })?;
                        let process = Arc::new(Mutex::new(Some(process)));
                        let build_id = client.protocol().build.clone();
                        let (access, link) = upgrade::worker_access(client, endpoint.clone(), pid);
                        workers.push(ManagedWorker {
                            access: access.clone(),
                            process,
                            lifeline: lifeline
                                .into_owned_fd()
                                .map_err(|error| AccessError::unavailable(error.to_string()))?,
                            pid,
                            build_id,
                            endpoint,
                            grant: grant.clone(),
                            launch: launch.clone(),
                            executable_lease: upgrade::retain_executable(&executable)
                                .map_err(|error| AccessError::unavailable(error.to_string()))?,
                            executable,
                            link,
                            preparation_uncertain: false,
                            upgrade_error: None,
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
            .map(|managed| (managed.process.clone(), managed.pid));
        let exited = match process {
            Some((process, pid)) => match process.lock().await.as_mut() {
                Some(process) => matches!(process.try_wait(), Ok(Some(_))),
                None => !upgrade::process_exists(pid),
            },
            None => false,
        };
        if exited {
            self.control.revoke(worker).await;
        }
        exited
    }

    async fn other_tickhost(&self) -> Result<Option<OtherTickhost>, AccessError> {
        self.control
            .host_controller()
            .other_tickhost()
            .await
            .map_err(|error| AccessError::unavailable(error.to_string()))
    }

    async fn replace_tickhost(&self) -> Result<(), AccessError> {
        self.control
            .host_controller()
            .replace_other_tickhost()
            .await
            .map_err(|error| AccessError::unavailable(error.to_string()))
    }
}

pub async fn run_daemon(arguments: &[OsString]) -> Result<(), String> {
    if !(2..=3).contains(&arguments.len()) {
        return Err("invalid private daemon invocation".into());
    }
    let directory = PathBuf::from(&arguments[0]);
    let descriptor = &arguments[1];
    let handoff = arguments
        .get(2)
        .map(|value| {
            let descriptor = value
                .to_str()
                .and_then(|value| value.parse::<i32>().ok())
                .filter(|value| *value >= 3)
                .ok_or("invalid daemon handoff descriptor")?;
            upgrade::read_handoff(descriptor)
        })
        .transpose()?;
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
    let (listener, socket_identity) = if let Some(handoff) = &handoff {
        let listener = upgrade::inherited_listener(handoff.listener)?;
        upgrade::validate_inherited_endpoint(&endpoint, handoff.listener_identity)?;
        (listener, handoff.listener_identity)
    } else {
        private_listener(&endpoint)?
    };
    let listener_copy =
        upgrade::duplicate(listener.as_raw_fd()).map_err(|error| error.to_string())?;
    let epoch = handoff
        .as_ref()
        .map_or_else(Uuid::new_v4, |handoff| handoff.epoch);
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
    let (control_listener, control_identity) = if let Some(handoff) = &handoff {
        let listener = upgrade::inherited_listener(handoff.control_listener)?;
        upgrade::validate_inherited_endpoint(&control_endpoint, handoff.control_identity)?;
        (listener, handoff.control_identity)
    } else {
        remove_stale_socket(&control_endpoint)?;
        private_listener(&control_endpoint)?
    };
    let control_listener_copy =
        upgrade::duplicate(control_listener.as_raw_fd()).map_err(|error| error.to_string())?;
    let limits = AccessLimits::DEFAULT;
    let shutdown = CancellationToken::new();
    let lifecycle = Arc::new(lifecycle::DaemonLifecycle::new(shutdown.clone()));
    let ingress = Arc::new(ingress::DaemonIngress::new(epoch, limits));
    let factory = Arc::new(ProcessWorkerFactory {
        directory: directory.clone(),
        listener_identity: socket_identity,
        control_identity,
        limits,
        workers: Mutex::new(Vec::new()),
        shutting_down: AtomicBool::new(false),
        control: control.clone(),
        upgrade: Mutex::new(upgrade::UpgradeState::default()),
        upgrade_requested: Notify::new(),
        lifecycle: lifecycle.clone(),
        ingress: ingress.clone(),
    });
    let daemon = Arc::new(DaemonServer::new(epoch, factory.clone()));
    control.attach_daemon((*daemon).clone());
    if let Some(handoff) = handoff {
        let mut channel = upgrade::inherited_stream(handoff.handshake)?;
        factory
            .restore_workers(handoff, &daemon)
            .await
            .map_err(|error| error.to_string())?;
        control
            .host_controller()
            .prepare_daemon_handoff()
            .await
            .map_err(|error| error.to_string())?;
        upgrade::wait_for_commit(&mut channel).await?;
    }
    let control_service = tokio::spawn(control.clone().serve(control_listener));
    let mut service = tokio::spawn(ipc::serve_daemon_listener_with_lifecycle(
        listener,
        daemon.clone(),
        epoch,
        limits,
        lifecycle,
        ingress,
    ));
    let worker_upgrades = tokio::spawn(worker_upgrade::drive(
        factory.clone(),
        daemon.clone(),
        shutdown.clone(),
    ));
    let mut transferred = None;
    let result = tokio::select! {
        result = &mut service => result.map_err(|error| error.to_string())?.map_err(|error| error.to_string()),
        reason = step_down_when_unreachable(&endpoint, socket_identity, &lock, &lock_path, &shutdown) => {
            reason.map_or(Ok(()), Err)
        }
        upgraded = upgrade::drive_daemon_upgrade(factory.clone(), daemon, &lock, &listener_copy, &control_listener_copy, &shutdown) => {
            transferred = upgraded;
            Ok(())
        }
    };
    service.abort();
    control_service.abort();
    let _ = control_service.await;
    worker_upgrades.abort();
    if let Some(transfer) = transferred {
        factory.workers.lock().await.clear();
        control.host_controller().relinquish_daemon().await;
        drop(lock);
        drop(listener_copy);
        drop(control_listener_copy);
        transfer.finish().await;
        control.drain().await;
        shutdown.cancel();
        return result;
    }
    shutdown.cancel();
    factory.shutdown().await;
    let cleanup = validate_daemon_lock_path(&lock, &lock_path)
        .and_then(|()| remove_owned_socket(&endpoint, socket_identity))
        .and_then(|()| remove_owned_socket(&control_endpoint, control_identity));
    drop(lock);
    cleanup?;
    result
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
    // One set of model clients per Worker: their private-history provenance lives as long as it.
    run_worker_with_models(arguments, |config_root| {
        Ok(Arc::new(LocalModelClients::new(config_root)))
    })
    .await
}

/// Runs a private Worker whose model clients `models` makes from the launch's config root.
/// Integration fixtures use this seam to keep their own model transport.
#[doc(hidden)]
pub async fn run_worker_with_models(
    arguments: &[OsString],
    models: impl FnOnce(Option<PathBuf>) -> Result<Arc<dyn ModelClients>, String>,
) -> Result<(), String> {
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
    let models = models(bootstrap.launch.config_root.clone())?;
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    let agent = build_runtime_with_models(
        &bootstrap.launch,
        models,
        Some(bootstrap.control.clone()),
        &executable,
    )
    .await?;
    let access = Arc::new(EmbeddedAgentAccess::for_worker(
        agent.clone(),
        bootstrap.epoch,
    ));
    if let Some(checkpoint) = bootstrap.checkpoint {
        access
            .restore_upgrade(worker_upgrade::read_checkpoint(checkpoint)?)
            .await?;
    }
    let (listener, socket_identity) = private_listener(&endpoint)?;
    let upgrade_endpoint = worker_upgrade::endpoint(&endpoint);
    let (upgrade_listener, upgrade_identity) = private_listener(&upgrade_endpoint)?;
    let retired = CancellationToken::new();
    let mut worker_service = tokio::spawn(serve_worker_listener(
        listener,
        access.clone(),
        bootstrap.epoch,
        bootstrap.limits,
    ));
    if let Some(commit) = bootstrap.commit {
        let mut probe = [0u8; 1];
        let activation = tokio::select! {
            activation = worker_upgrade::activate(&access, commit) => activation,
            ended = stdin.read(&mut probe) => {
                agent.shutdown().await;
                ended.map_err(|error| error.to_string()).and_then(|_| Err("daemon ended before Worker activation".into()))
            }
        };
        if let Err(error) = activation {
            worker_service.abort();
            remove_owned_socket(&endpoint, socket_identity)?;
            remove_owned_socket(&upgrade_endpoint, upgrade_identity)?;
            return Err(error);
        }
    }
    let upgrades = tokio::spawn(worker_upgrade::serve(
        upgrade_listener,
        access.clone(),
        bootstrap.control.capability,
        retired.clone(),
    ));
    let mut probe = [0u8; 1];
    let result = tokio::select! {
        result = &mut worker_service => {
            result.map_err(|error| error.to_string())?.map_err(|error| error.to_string())
        }
        result = stdin.read(&mut probe) => result.map(|_| ()).map_err(|error| error.to_string()),
        () = retired.cancelled() => Ok(()),
    };
    upgrades.abort();
    worker_service.abort();
    if retired.is_cancelled() {
        access.shutdown_for_upgrade().await;
    } else {
        agent.shutdown().await;
    }
    remove_owned_socket(&endpoint, socket_identity)?;
    remove_owned_socket(&upgrade_endpoint, upgrade_identity)?;
    result
}

pub async fn build_runtime(
    launch: &LaunchSpec,
    control: Option<tickloop_agent_runtime::hosted::worker_control::WorkerControlGrant>,
) -> Result<Agent, String> {
    // One set of model clients per Worker: their private-history provenance lives as long as it.
    let models = Arc::new(LocalModelClients::new(launch.config_root.clone()));
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    build_runtime_with_models(launch, models, control, &executable).await
}

/// Build the same version-owned runtime with caller-supplied model clients. `executable` is the
/// `tickloop` whose attach client a Herdr Pane showing a Tick runs.
/// Integration fixtures use this seam without adding definition-file loading to the CLI.
#[doc(hidden)]
pub async fn build_runtime_with_models(
    launch: &LaunchSpec,
    models: Arc<dyn ModelClients>,
    control: Option<tickloop_agent_runtime::hosted::worker_control::WorkerControlGrant>,
    executable: &Path,
) -> Result<Agent, String> {
    let model = launch_model(launch);
    let settings = launch
        .config_root
        .as_ref()
        .map(|root| UserSettings::in_root(root));
    let transcripts = if launch.session_persistence {
        let root = launch
            .config_root
            .clone()
            .ok_or("saved sessions require a config root")?;
        let store = tickloop_agent_runtime::transcript::JsonlTranscriptStore::open(root)
            .map_err(|error| format!("storage failure: {error}"))?;
        Some(Arc::new(store)
            as Arc<
                dyn tickloop_agent_runtime::transcript::TranscriptStore,
            >)
    } else {
        None
    };
    // Every worker the daemon granted a Tickhost runs its shell commands and Ticks there, under
    // spawns and views the daemon grants and with its host's own authority; a worker in a Herdr
    // View can also show them in Herdr Panes.
    let views = launch.host_work_context.as_ref().map(|work| {
        tickloop_agent_runtime::hosted::views::HerdrViews::new(
            tickhost_connect::layout::HerdrLayout::new(
                tickhost_connect::herdr::HerdrContext::without_view(work.socket.clone()),
            ),
            executable,
        )
    });
    let hosted = control.as_ref().map(|grant| {
        let tools = tickloop_agent_runtime::hosted::tools::HostedTickTools::new(
            tickloop_agent_runtime::hosted::worker_control::DaemonControl::new(grant.clone()),
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
        );
        // Opens find the Tick packages `tickloop tick install` placed in this View's config root.
        let tools = match &launch.config_root {
            Some(root) => {
                tools.with_tick_packages(tick_packages::PackageStore::in_config_root(root))
            }
            None => tools,
        };
        Arc::new(match views {
            Some(views) => tools.with_herdr_views(views),
            None => tools,
        })
    });
    let environment = match (&control, &hosted) {
        (Some(grant), Some(hosted)) => LocalEnvironment::with(
            Arc::new(
                tickloop_agent_runtime::hosted::worker_control::DaemonControl::new(grant.clone()),
            ),
            hosted.clone(),
        ),
        _ => LocalEnvironment::bare(),
    };
    let agent = Agent::with_seams(
        HarnessSeams {
            transcripts,
            environment: Some(Arc::new(environment)),
            models,
        },
        HarnessOptions {
            search: Default::default(),
            model,
            settings,
            host: launch.host_options,
            tools: ToolPolicy::local(),
            host_rules: Vec::new(),
            task_tools: None,
            waits: tickloop_agent_runtime::WaitHosting::Process,
            citation_style: Default::default(),
            system_prompt_addition: None,
            request_gates: Vec::new(),
            time_zone: None,
        },
    )
    .map_err(|error| error.to_string())?;
    use tickloop_agent_runtime::tick_adapter::TickAdapters;
    let tick_tools = hosted
        .clone()
        .map(|hosted| hosted as Arc<dyn tickloop_agent_runtime::host_tools::HostToolProvider>);
    let adapters = Arc::new(TickAdapters::builtin(agent.clone(), hosted));
    agent.install_tick_stream(adapters.clone()).await;
    if let Some(root) = launch.config_root.as_ref()
        && let Ok(artifact) = tickloop_agent_artifacts::ArtifactClient::new(root)
    {
        agent
            .install_host_tools(Arc::new(
                tickloop_agent_runtime::mcp_tools::McpHostToolProvider::new(
                    artifact,
                    tick_tools.clone(),
                ),
            ))
            .map_err(|error| format!("cannot install MCP tools: {error}"))?;
    } else if let Some(tick_tools) = tick_tools {
        agent
            .install_host_tools(tick_tools)
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

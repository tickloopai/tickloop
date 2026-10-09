use std::{
    ffi::OsString,
    fs::{File, OpenOptions},
    io,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            ffi::OsStringExt,
            fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
            process::CommandExt,
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
use uuid::Uuid;

use crate::{
    agent_access::{
        AccessError, AgentAccess, DaemonServer, EmbeddedAgentAccess, LaunchSpec, LocalAgentClient,
        WorkerFactory, serve_worker_listener, worker_link,
    },
    model::{ModelClient, ModelConfig, cards::ModelCatalog, model_client},
    protocol::AccessLimits,
    runtime::Agent,
    settings::UserSettings,
};

mod daemon_control;

const ARTIFACT_EVENT_BRIDGE_URI: &str = "tickloop://event-bridge/v2";
const ARTIFACT_EVENT_BRIDGE_FALLBACK_URI: &str = "tickloop://event-bridge/v1";

const STARTUP_DEADLINE: Duration = Duration::from_secs(5);
const CONNECT_INTERVAL: Duration = Duration::from_millis(25);
const SOCKET_NAME: &str = "daemon.sock";
const LOCK_NAME: &str = "daemon.lock";
const DAEMON_COMMAND: &str = "daemon";
const RUN_SUBCOMMAND: &str = "run";
/// Hidden process entries: none appears in `--help`, and each names its role in `ps`.
#[doc(hidden)]
pub const DAEMON_RUN_ARGUMENTS: [&str; 2] = [DAEMON_COMMAND, RUN_SUBCOMMAND];
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
    control: crate::hosted::worker_control::WorkerControlGrant,
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
    control: Arc<crate::hosted::worker_control::WorkerControl>,
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
                    Ok(Err(error))
                        if error.code == crate::agent_access::AccessErrorCode::VersionMismatch =>
                    {
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

pub(crate) fn runtime_directory() -> PathBuf {
    std::env::var_os("TICKLOOP_DAEMON_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(format!("/tmp/tickloop-daemon-{}", unsafe {
                libc::geteuid()
            }))
        })
}

pub(crate) async fn connect_or_start() -> Result<LocalAgentClient, String> {
    let directory = runtime_directory();
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    connect_or_start_at(directory, executable).await
}

#[doc(hidden)]
pub async fn connect_or_start_at(
    directory: PathBuf,
    executable: PathBuf,
) -> Result<LocalAgentClient, String> {
    validate_directory(&directory)?;
    let directory = directory
        .canonicalize()
        .map_err(|error| error.to_string())?;
    let executable = executable
        .canonicalize()
        .map_err(|error| error.to_string())?;
    let endpoint = directory.join(SOCKET_NAME);
    check_socket_length(&endpoint)?;
    if let Some(client) = connect_existing(&endpoint, &executable, false).await? {
        return Ok(client);
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(directory.join(LOCK_NAME))
        .map_err(|error| format!("cannot open Agent daemon lock: {error}"))?;
    validate_file(&lock, "Agent daemon lock")?;
    let deadline = Instant::now() + STARTUP_DEADLINE;
    let mut spawned_daemon = None;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        if let Some(client) = connect_existing(&endpoint, &executable, false).await? {
            return Ok(client);
        }
        remove_stale_socket(&endpoint)?;
        let lock_fd = lock.as_raw_fd();
        let mut command = std::process::Command::new(&executable);
        command
            .args(DAEMON_RUN_ARGUMENTS)
            .arg(&directory)
            .arg(lock_fd.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // Only the winner's already-validated lock is inherited by the detached daemon.
        unsafe {
            command.pre_exec(move || {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                if libc::fcntl(lock_fd, libc::F_SETFD, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        spawned_daemon = Some(
            command
                .spawn()
                .map_err(|error| format!("cannot start local Agent daemon: {error}"))?,
        );
    } else {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::WouldBlock {
            return Err(format!("cannot arbitrate Agent daemon startup: {error}"));
        }
    }
    drop(lock);
    loop {
        let own_startup = spawned_daemon
            .as_mut()
            .is_some_and(|process| matches!(process.try_wait(), Ok(None)));
        if let Some(client) = connect_existing(&endpoint, &executable, own_startup).await? {
            return Ok(client);
        }
        if Instant::now() >= deadline {
            return Err("local Agent daemon did not become ready within 5 seconds".into());
        }
        tokio::time::sleep(CONNECT_INTERVAL).await;
    }
}

async fn connect_existing(
    endpoint: &Path,
    executable: &Path,
    own_startup: bool,
) -> Result<Option<LocalAgentClient>, String> {
    if !endpoint.exists() {
        return Ok(None);
    }
    validate_socket(endpoint)?;
    match tokio::time::timeout(STARTUP_DEADLINE, LocalAgentClient::connect(endpoint)).await {
        Ok(Ok(client)) => Ok(Some(client)),
        Ok(Err(error)) if error.code == crate::agent_access::AccessErrorCode::VersionMismatch => {
            Err(incompatible_daemon_guidance(
                endpoint,
                executable,
                &error,
                own_startup,
            ))
        }
        Ok(Err(error)) => {
            if matches!(
                error.code,
                crate::agent_access::AccessErrorCode::Unavailable
            ) && matches!(
                std::os::unix::net::UnixStream::connect(endpoint)
                    .err()
                    .map(|e| e.kind()),
                Some(io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound)
            ) {
                Ok(None)
            } else {
                Err(error.to_string())
            }
        }
        Err(_) => Err("existing Agent daemon is not responding; it was left running".into()),
    }
}

fn incompatible_daemon_guidance(
    endpoint: &Path,
    executable: &Path,
    error: &AccessError,
    own_startup: bool,
) -> String {
    let runtime = endpoint
        .parent()
        .expect("daemon endpoint has a runtime directory");
    let title = if own_startup {
        "Agent executable changed during startup"
    } else {
        "Agent daemon version mismatch"
    };
    let details = format!(
        "{title} (client {}.{}).\nProtocol: {}\nRuntime: {}\nExecutable: {}",
        crate::agent_access::PROTOCOL_MAJOR,
        crate::agent_access::PROTOCOL_MINOR,
        error.message,
        runtime.display(),
        executable.display(),
    );
    let (Some(runtime), Some(executable)) = (runtime.to_str(), executable.to_str()) else {
        return format!(
            "{details}\n\nRun this build with daemon stop, then relaunch.\nThis stops running Agents. Saved history is kept."
        );
    };
    let stop = format!(
        "TICKLOOP_DAEMON_RUNTIME_DIR={} {}",
        shell_words::quote(runtime),
        shell_words::quote(executable)
    );
    let mut launch = format!(
        "TICKLOOP_DAEMON_RUNTIME_DIR={} ",
        shell_words::quote(runtime)
    );
    if let Some(config) = std::env::var_os("TICKLOOP_CONFIG_DIR")
        && let Some(config) = config.to_str()
    {
        launch.push_str(&format!(
            "TICKLOOP_CONFIG_DIR={} ",
            shell_words::quote(config)
        ));
    }
    launch.push_str(&shell_words::quote(executable));
    let startup_note = if own_startup {
        "\nUse a pinned executable for CLI, daemon, and workers before relaunching."
    } else {
        ""
    };
    format!(
        "{details}{startup_note}\n\nStop the daemon and relaunch:\n  {stop} daemon stop\n  {launch}\n\nThis stops running Agents. Saved history is kept."
    )
}

pub async fn run_internal(arguments: &[OsString]) -> Option<Result<(), String>> {
    let command = arguments.first()?.to_str()?;
    let subcommand = arguments.get(1).and_then(|value| value.to_str());
    match (command, subcommand) {
        (DAEMON_COMMAND, Some(RUN_SUBCOMMAND)) => Some(run_daemon(&arguments[2..]).await),
        (DAEMON_COMMAND, _) => Some(daemon_control::run(&arguments[1..]).await),
        (WORKER_ARGUMENT, _) => Some(run_worker(&arguments[1..]).await),
        _ if arguments == crate::hosted::controller::TICKHOST_RUN_ARGUMENTS => {
            Some(run_tickhost().await)
        }
        ("monitor", _) => Some(crate::monitor_command::run(&arguments[1..]).await),
        _ => None,
    }
}

async fn run_daemon(arguments: &[OsString]) -> Result<(), String> {
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
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    let controller = Arc::new(crate::hosted::controller::HostController::new(
        &directory,
        tickhost::paths::HostPaths::from_environment(),
        epoch,
        executable,
        tickhost::host::DEFAULT_IDLE_TIMEOUT,
    ));
    let control = Arc::new(crate::hosted::worker_control::WorkerControl::new(
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
    let result = crate::agent_access::serve_daemon_listener(listener, daemon, epoch, limits)
        .await
        .map_err(|error| error.to_string());
    factory.shutdown().await;
    control_service.abort();
    let cleanup = validate_daemon_lock_path(&lock, &lock_path)
        .and_then(|()| remove_owned_socket(&endpoint, socket_identity))
        .and_then(|()| remove_owned_socket(&control_endpoint, control_identity));
    drop(lock);
    cleanup?;
    result
}

/// The Tickhost process the daemon starts on demand. It runs only the host library.
async fn run_tickhost() -> Result<(), String> {
    let config =
        tickhost::host::read_config(std::io::stdin().lock()).map_err(|error| error.to_string())?;
    tickhost::host::serve(tickhost::paths::HostPaths::from_environment(), config)
        .await
        .map_err(|error| format!("Tickhost: {error}"))
}

async fn run_worker(arguments: &[OsString]) -> Result<(), String> {
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
    let mut execution_events = agent.subscribe();
    let mut shell_events = agent.subscribe_user_shells();
    let access = Arc::new(EmbeddedAgentAccess::for_worker(
        agent.clone(),
        bootstrap.epoch,
    ));
    let (listener, socket_identity) = private_listener(&endpoint)?;
    let mut housekeeping = bootstrap.launch.config_root.as_ref().map(|root| {
        crate::housekeeping::HousekeepingSchedule::new(
            root.clone(),
            crate::housekeeping::UserActivity::process().clone(),
            Arc::new(crate::hosted::lifecycle::DaemonLifecycle::new(
                bootstrap.control.clone(),
            )),
        )
    });
    let mut probe = [0u8; 1];
    let service = serve_worker_listener(listener, access, bootstrap.epoch, bootstrap.limits);
    tokio::pin!(service);
    let result = loop {
        tokio::select! {
            result=&mut service=>break result.map_err(|error|error.to_string()),
            result=stdin.read(&mut probe)=>break result.map(|_|()).map_err(|error|error.to_string()),
            Ok(event)=execution_events.recv()=>{
                if matches!(event.kind,crate::runtime::EventKind::Started)
                    && let Some(schedule)=housekeeping.as_mut() {schedule.start();}
            },
            Ok(_)=shell_events.recv()=>{
                if let Some(schedule)=housekeeping.as_mut() {schedule.start();}
            },
        }
    };
    agent.shutdown().await;
    remove_owned_socket(&endpoint, socket_identity)?;
    result
}

/// The model a worker for `launch` starts with.
pub(crate) fn launch_model(launch: &LaunchSpec) -> ModelConfig {
    let catalog = ModelCatalog::defaults();
    launch
        .model
        .as_ref()
        .and_then(|model| catalog.get(&model.provider_id, &model.model_id))
        .unwrap_or_else(|| {
            catalog
                .get("deepseek-api", "deepseek-flash")
                .expect("bundled default model")
        })
}

pub(crate) async fn build_runtime(
    launch: &LaunchSpec,
    control: Option<crate::hosted::worker_control::WorkerControlGrant>,
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
    control: Option<crate::hosted::worker_control::WorkerControlGrant>,
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
    use crate::tick_adapter::TickAdapters;
    let access = launch.host_work_context.as_ref().map(|work| {
        Arc::new(tick_connect::access::TickAccess::new(
            tick_connect::herdr::HerdrContext {
                socket: work.socket.clone(),
                caller_pane: String::new(),
            },
        ))
    });
    if let (Some(grant), Some(_)) = (&control, &launch.config_root) {
        agent.install_stopped_child_hook(Arc::new(crate::hosted::lifecycle::DaemonLifecycle::new(
            grant.clone(),
        )));
    }
    // Every worker the daemon granted a Tickhost opens hidden Ticks there, with its host's own
    // authority; a worker in a Herdr View also keeps Herdr Workspaces and visible Panes. Each
    // Tick tool call is routed by its target to one of the two.
    let hosted = control.map(|grant| {
        Arc::new(crate::hosted::tools::HostedTickTools::new(
            crate::hosted::worker_control::DaemonControl::new(grant),
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
        Arc::new(crate::native_host::NativeHostToolProvider::new(
            access.clone(),
        ))
    });
    let native_host = (herdr.is_some() || hosted.is_some()).then(|| {
        Arc::new(crate::tick_tool_router::TickToolRouter::new(
            herdr,
            hosted.clone(),
        )) as Arc<dyn crate::host_tools::HostToolProvider>
    });
    if let Some(hosted) = &hosted {
        agent.install_hosted_ticks(hosted.clone());
    }
    let adapters = Arc::new(TickAdapters::builtin(
        Some(agent.clone()),
        launch.config_root.clone(),
        access,
        hosted,
    ));
    agent.install_tick_stream(adapters.clone()).await;
    if let Some(root) = launch.config_root.as_ref()
        && let Ok(artifact) = crate::artifacts::ArtifactClient::new(root)
    {
        agent
            .install_host_tools(Arc::new(
                crate::mcp_tools::McpHostToolProvider::new(artifact, native_host.clone())
                    .with_event_bridge(ARTIFACT_EVENT_BRIDGE_URI)
                    .with_event_bridge_fallback(ARTIFACT_EVENT_BRIDGE_FALLBACK_URI),
            ))
            .map_err(|error| format!("cannot install MCP tools: {error}"))?;
    } else if let Some(native_host) = native_host {
        agent
            .install_host_tools(native_host)
            .map_err(|error| format!("cannot install Tick tools: {error}"))?;
    }
    Ok(agent)
}

fn validate_directory(directory: &Path) -> Result<(), String> {
    if !directory.exists() {
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(directory)
            .map_err(|error| format!("cannot create private Agent runtime directory: {error}"))?;
    }
    let metadata = std::fs::symlink_metadata(directory).map_err(|error| error.to_string())?;
    if !metadata.file_type().is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o777 != 0o700
    {
        return Err("Agent runtime directory must be a non-symlink directory owned by this user with mode 0700".into());
    }
    Ok(())
}

fn validate_file(file: &File, label: &str) -> Result<(), String> {
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    if !metadata.file_type().is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o777 != 0o600
    {
        return Err(format!(
            "{label} must be a regular file owned by this user with mode 0600"
        ));
    }
    Ok(())
}

fn validate_socket(endpoint: &Path) -> Result<(), String> {
    use std::os::unix::fs::FileTypeExt;
    let metadata = std::fs::symlink_metadata(endpoint).map_err(|error| error.to_string())?;
    if !metadata.file_type().is_socket()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o777 != 0o600
    {
        return Err("Agent endpoint must be a socket owned by this user with mode 0600".into());
    }
    Ok(())
}

fn remove_stale_socket(endpoint: &Path) -> Result<(), String> {
    if std::fs::symlink_metadata(endpoint).is_ok() {
        validate_socket(endpoint)?;
        std::fs::remove_file(endpoint).map_err(|error| error.to_string())?;
    }
    Ok(())
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

fn check_socket_length(endpoint: &Path) -> Result<(), String> {
    use std::os::unix::ffi::OsStrExt;
    if endpoint.as_os_str().as_bytes().len() >= 104 {
        return Err("Agent runtime directory is too long for a Unix socket path".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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

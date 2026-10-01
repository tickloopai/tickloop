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
        AccessError, AgentAccess, EmbeddedAgentAccess, LaunchSpec, LocalAgentClient, OwnerServer,
        WorkerFactory, serve_worker_listener, worker_link,
    },
    model::{ModelConfig, cards::ModelCatalog, model_client},
    runtime::Agent,
    settings::UserSettings,
};

mod service_control;

const ARTIFACT_EVENT_BRIDGE_URI: &str = "tickloop://event-bridge/v2";
const ARTIFACT_EVENT_BRIDGE_FALLBACK_URI: &str = "tickloop://event-bridge/v1";

const STARTUP_DEADLINE: Duration = Duration::from_secs(5);
const CONNECT_INTERVAL: Duration = Duration::from_millis(25);
const SOCKET_NAME: &str = "owner.sock";
const LOCK_NAME: &str = "owner.lock";

#[derive(Serialize, Deserialize)]
struct WorkerBootstrap {
    epoch: Uuid,
    launch: LaunchSpec,
}

struct ManagedWorker {
    access: Arc<dyn AgentAccess>,
    process: Arc<Mutex<Child>>,
    lifeline: ChildStdin,
}

struct ProcessWorkerFactory {
    directory: PathBuf,
    workers: Mutex<Vec<ManagedWorker>>,
    shutting_down: AtomicBool,
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
        owner_epoch: Uuid,
    ) -> Result<Arc<dyn AgentAccess>, AccessError> {
        if self.shutting_down.load(Ordering::SeqCst) {
            return Err(AccessError::unavailable("Agent owner is shutting down"));
        }
        let endpoint = self.directory.join(format!("w-{}.sock", Uuid::new_v4()));
        check_socket_length(&endpoint).map_err(AccessError::unavailable)?;
        let executable =
            std::env::current_exe().map_err(|error| AccessError::unavailable(error.to_string()))?;
        let mut command = Command::new(executable);
        command
            .arg("--agent-worker")
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
        let mut bootstrap = serde_json::to_vec(&WorkerBootstrap {
            epoch: owner_epoch,
            launch: launch.clone(),
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
                        if client.owner_epoch() != owner_epoch {
                            return Err(AccessError::unavailable(
                                "worker belongs to another owner epoch",
                            ));
                        }
                        let mut workers = self.workers.lock().await;
                        if self.shutting_down.load(Ordering::SeqCst) {
                            drop(lifeline);
                            let _ = tokio::time::timeout(STARTUP_DEADLINE, process.wait()).await;
                            return Err(AccessError::unavailable(
                                "Agent owner stopped during worker startup",
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
        match process {
            Some(process) => matches!(process.lock().await.try_wait(), Ok(Some(_))),
            None => false,
        }
    }
}

pub(crate) fn runtime_directory() -> PathBuf {
    std::env::var_os("TICKLOOP_AGENT_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(format!("/tmp/tickloop-agent-{}", unsafe {
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
    let endpoint = directory.join(SOCKET_NAME);
    check_socket_length(&endpoint)?;
    if let Some(client) = connect_existing(&endpoint, &executable).await? {
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
        .map_err(|error| format!("cannot open Agent owner lock: {error}"))?;
    validate_file(&lock, "Agent owner lock")?;
    let deadline = Instant::now() + STARTUP_DEADLINE;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        if let Some(client) = connect_existing(&endpoint, &executable).await? {
            return Ok(client);
        }
        remove_stale_socket(&endpoint)?;
        let lock_fd = lock.as_raw_fd();
        let mut command = std::process::Command::new(&executable);
        command
            .arg("--agent-owner")
            .arg(&directory)
            .arg(lock_fd.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // Only the winner's already-validated lock is inherited by the detached owner.
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
        command
            .spawn()
            .map_err(|error| format!("cannot start local Agent owner: {error}"))?;
    } else {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::WouldBlock {
            return Err(format!("cannot arbitrate Agent owner startup: {error}"));
        }
    }
    drop(lock);
    loop {
        if let Some(client) = connect_existing(&endpoint, &executable).await? {
            return Ok(client);
        }
        if Instant::now() >= deadline {
            return Err("local Agent owner did not become ready within 5 seconds".into());
        }
        tokio::time::sleep(CONNECT_INTERVAL).await;
    }
}

async fn connect_existing(
    endpoint: &Path,
    executable: &Path,
) -> Result<Option<LocalAgentClient>, String> {
    if !endpoint.exists() {
        return Ok(None);
    }
    validate_socket(endpoint)?;
    match tokio::time::timeout(STARTUP_DEADLINE, LocalAgentClient::connect(endpoint)).await {
        Ok(Ok(client)) => Ok(Some(client)),
        Ok(Err(error)) if error.code == crate::agent_access::AccessErrorCode::VersionMismatch => {
            Err(incompatible_owner_guidance(executable))
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
        Err(_) => Err("existing Agent owner is not responding; it was left running".into()),
    }
}

fn incompatible_owner_guidance(executable: &Path) -> String {
    let executable = std::env::current_dir()
        .ok()
        .and_then(|directory| executable.strip_prefix(directory).ok())
        .map(|relative| PathBuf::from(".").join(relative))
        .unwrap_or_else(|| executable.to_path_buf());
    let Some(executable) = executable.to_str() else {
        return "Agent service version mismatch\n\nRun this build with service stop, then relaunch.\nThis stops running Agents. Saved history is kept.".into();
    };
    let mut stop = String::new();
    if let Some(directory) = std::env::var_os("TICKLOOP_AGENT_RUNTIME_DIR") {
        let Some(directory) = directory.to_str() else {
            return "Agent service version mismatch\n\nKeep TICKLOOP_AGENT_RUNTIME_DIR unchanged. Run this build with service stop, then relaunch.\nThis stops running Agents. Saved history is kept.".into();
        };
        stop.push_str(&format!(
            "TICKLOOP_AGENT_RUNTIME_DIR={} ",
            shell_words::quote(directory),
        ));
    }
    let mut launch = stop.clone();
    if let Some(config) = std::env::var_os("TICKLOOP_CONFIG_DIR")
        && let Some(config) = config.to_str()
    {
        launch.push_str(&format!(
            "TICKLOOP_CONFIG_DIR={} ",
            shell_words::quote(config),
        ));
    }
    stop.push_str(&shell_words::quote(executable));
    launch.push_str(&shell_words::quote(executable));
    format!(
        "Agent service version mismatch\n\nStop the service and relaunch:\n  {stop} service stop\n  {launch}\n\nThis stops running Agents. Saved history is kept."
    )
}

pub async fn run_internal(arguments: &[OsString]) -> Option<Result<(), String>> {
    match arguments.first().and_then(|value| value.to_str()) {
        Some("--agent-owner") => Some(run_owner(arguments).await),
        Some("--agent-worker") => Some(run_worker(arguments).await),
        Some("service") => Some(service_control::run(&arguments[1..]).await),
        _ => None,
    }
}

async fn run_owner(arguments: &[OsString]) -> Result<(), String> {
    if arguments.len() != 3 {
        return Err("invalid private owner invocation".into());
    }
    let directory = PathBuf::from(&arguments[1]);
    validate_directory(&directory)?;
    let descriptor = arguments[2]
        .to_str()
        .and_then(|value| value.parse::<i32>().ok())
        .filter(|value| *value >= 3)
        .ok_or("invalid owner lock descriptor")?;
    let lock = unsafe { File::from_raw_fd(descriptor) };
    validate_file(&lock, "Agent owner lock")?;
    let inherited = lock.metadata().map_err(|error| error.to_string())?;
    let expected =
        std::fs::symlink_metadata(directory.join(LOCK_NAME)).map_err(|error| error.to_string())?;
    if expected.file_type().is_symlink()
        || inherited.dev() != expected.dev()
        || inherited.ino() != expected.ino()
    {
        return Err("inherited lock does not belong to this Agent owner directory".into());
    }
    if unsafe { libc::flock(descriptor, libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err("Agent owner lock is not held by this process".into());
    }
    if unsafe { libc::fcntl(descriptor, libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
        return Err("cannot isolate owner lock from workers".into());
    }
    let listener = private_listener(&directory.join(SOCKET_NAME))?;
    let epoch = Uuid::new_v4();
    let factory = Arc::new(ProcessWorkerFactory {
        directory: directory.clone(),
        workers: Mutex::new(Vec::new()),
        shutting_down: AtomicBool::new(false),
    });
    let owner = Arc::new(OwnerServer::new(epoch, factory.clone()));
    let result = crate::agent_access::serve_owner_listener(listener, owner, epoch)
        .await
        .map_err(|error| error.to_string());
    factory.shutdown().await;
    let _ = std::fs::remove_file(directory.join(SOCKET_NAME));
    drop(lock);
    result
}

async fn run_worker(arguments: &[OsString]) -> Result<(), String> {
    if arguments.len() != 2 {
        return Err("invalid private worker invocation".into());
    }
    let endpoint = PathBuf::from(&arguments[1]);
    let directory = endpoint.parent().ok_or("worker socket has no directory")?;
    validate_directory(directory)?;
    let mut stdin = BufReader::new(tokio::io::stdin());
    let mut bytes = Vec::new();
    (&mut stdin)
        .take(crate::agent_access::MAX_FRAME_BYTES as u64 + 1)
        .read_until(b'\n', &mut bytes)
        .await
        .map_err(|error| error.to_string())?;
    if bytes.len() > crate::agent_access::MAX_FRAME_BYTES {
        return Err("worker bootstrap is too large".into());
    }
    let bootstrap: WorkerBootstrap =
        serde_json::from_slice(&bytes).map_err(|_| "invalid worker bootstrap")?;
    let agent = build_runtime(&bootstrap.launch).await?;
    let mut execution_events = agent.subscribe();
    let mut shell_events = agent.subscribe_user_shells();
    let access = Arc::new(EmbeddedAgentAccess::for_worker(
        agent.clone(),
        bootstrap.epoch,
    ));
    let listener = private_listener(&endpoint)?;
    let mut housekeeping = bootstrap.launch.config_root.as_ref().map(|root| {
        crate::housekeeping::HousekeepingSchedule::new(
            root.clone(),
            crate::housekeeping::UserActivity::process().clone(),
        )
    });
    let mut probe = [0u8; 1];
    let service = serve_worker_listener(listener, access, bootstrap.epoch);
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
    let _ = std::fs::remove_file(&endpoint);
    result
}

/// The model a worker for `launch` starts with.
pub(crate) fn launch_model(launch: &LaunchSpec) -> ModelConfig {
    launch.model.clone().unwrap_or_else(|| {
        ModelCatalog::defaults()
            .get("deepseek-api", "deepseek-flash-chat-completions")
            .expect("bundled default model")
    })
}

pub(crate) async fn build_runtime(launch: &LaunchSpec) -> Result<Agent, String> {
    let model = launch_model(launch);
    let settings = launch
        .config_root
        .as_ref()
        .map(|root| UserSettings::in_root(root));
    if let (Some(settings), Some(provider)) = (&settings, &launch.transient_provider) {
        settings.set_transient_provider(provider.clone());
    }
    let client = model_client(model.clone(), launch.config_root.as_deref())
        .map_err(|error| error.to_string())?;
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
    let mut native_stream: Option<Arc<dyn crate::tick_stream::TickStreamPort>> = None;
    let mut native_host: Option<Arc<dyn crate::host_tools::HostToolProvider>> = None;
    if let Some(work) = &launch.host_work_context {
        let access = Arc::new(tick_connect::access::TickAccess::new(
            tick_connect::herdr::HerdrContext {
                socket: work.socket.clone(),
                caller_pane: String::new(),
            },
        ));
        native_host = Some(Arc::new(crate::native_host::NativeHostToolProvider::new(
            access.clone(),
        )));
        native_stream = Some(Arc::new(crate::tick_stream::NativeTickStream::new(access)));
    }
    if let Some(root) = launch.config_root.as_ref()
        && let Ok(artifact) = crate::artifacts::ArtifactClient::new(root)
    {
        agent
            .install_tick_stream(Arc::new(crate::tick_stream::ArtifactTickStream::new(
                root.clone(),
                native_stream.clone(),
            )))
            .await;
        agent
            .install_host_tools(Arc::new(
                crate::mcp_tools::McpHostToolProvider::new(artifact, native_host)
                    .with_event_bridge(ARTIFACT_EVENT_BRIDGE_URI)
                    .with_event_bridge_fallback(ARTIFACT_EVENT_BRIDGE_FALLBACK_URI),
            ))
            .map_err(|error| format!("cannot install MCP tools: {error}"))?;
    } else if let Some(native) = native_host {
        if let Some(stream) = native_stream {
            agent.install_tick_stream(stream).await;
        }
        agent
            .install_host_tools(native)
            .map_err(|error| format!("cannot install Herdr tools: {error}"))?;
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

fn private_listener(endpoint: &Path) -> Result<UnixListener, String> {
    check_socket_length(endpoint)?;
    let listener = UnixListener::bind(endpoint).map_err(|error| error.to_string())?;
    std::fs::set_permissions(endpoint, std::fs::Permissions::from_mode(0o600))
        .map_err(|error| error.to_string())?;
    Ok(listener)
}

fn check_socket_length(endpoint: &Path) -> Result<(), String> {
    use std::os::unix::ffi::OsStrExt;
    if endpoint.as_os_str().as_bytes().len() >= 104 {
        return Err("Agent runtime directory is too long for a Unix socket path".into());
    }
    Ok(())
}

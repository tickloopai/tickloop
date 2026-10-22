//! Test executable providing a local model transport to the real CLI/daemon/worker chain.
//! No fixture definition enters the compiled model catalog or saved model selection.
//! `--build-identity` names the build that produced this executable under any file name. Run as
//! `tickhost` it is the Tickhost. Otherwise the arguments pick the process: `--cli` runs the
//! product CLI, a whole product process entry runs as the product starts it, and anything else is
//! a libtest probe of a target without tests.
#[path = "../all/graceful_upgrade.rs"]
mod graceful_upgrade;
#[path = "mod.rs"]
mod support;
use async_trait::async_trait;
use std::{
    ffi::OsString,
    io::Write,
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::ExitCode,
    sync::Arc,
};
use support::fixture_cli::{BUILD_IDENTITY_MODE, CLI_MODE, build_identity};
use tickloop_agent_client::DAEMON_RUN_ARGUMENTS;
use tickloop_agent_daemon::WORKER_ARGUMENT;
use tickloop_agent_model::{LocalModelClients, ModelClients};
use tickloop_agent_protocol::{ATTACH_ARGUMENTS, ModelConfig};
use tickloop_agent_runtime::hosted::controller::TICKHOST_EXECUTABLE;

const TRANSPORT_FILE: &str = "TICKLOOP_TRANSPORT_FIXTURE";
const TRANSPORT_FIXTURE_FLAG: &str = "--transport-fixture";
const LEGACY_DAEMON_FLAG: &str = "--legacy-daemon-fixture";
const LEGACY_CONFIG_FILE: &str = "legacy-fixture.json";

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyFixture {
    version: String,
    management: Option<bool>,
}

fn main() -> ExitCode {
    // Worker stderr is intentionally disconnected. Keep test-only panic evidence beside the
    // isolated transport fixture so a lost command reply can be diagnosed across processes.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Some(path) = std::env::var_os(TRANSPORT_FILE) {
            let path = PathBuf::from(path).with_extension("panic.log");
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .open(path)
            {
                let _ = writeln!(file, "{info}");
            }
        }
        default_hook(info);
    }));
    let arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
    if is_product_process(&arguments)
        && let Some(root) = std::env::var_os("TICKLOOP_CONFIG_DIR")
        && let Ok(file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(PathBuf::from(root).join("fixture-process.log"))
    {
        use std::os::fd::AsRawFd;
        unsafe {
            libc::dup2(file.as_raw_fd(), 2);
        }
    }
    if arguments.first().and_then(|argument| argument.to_str()) == Some("--content-identity") {
        println!("{}", tickhost::host::BUILD_IDENTITY);
        return ExitCode::SUCCESS;
    }
    if arguments.first().and_then(|argument| argument.to_str()) == Some(BUILD_IDENTITY_MODE) {
        println!("{}", build_identity());
        return ExitCode::SUCCESS;
    }
    if arguments.first().and_then(|argument| argument.to_str()) == Some("--runtime-info") {
        println!(
            "{}",
            serde_json::to_string(&tickloop_agent_protocol::RuntimeBinaryInfo {
                product_version: env!("CARGO_PKG_VERSION").into(),
                build_id: env!("TICKLOOP_BUILD_IDENTITY").into(),
                protocol_major: tickloop_agent_protocol::PROTOCOL_MAJOR,
                protocol_minor: tickloop_agent_protocol::PROTOCOL_MINOR,
                handoff_version: tickloop_agent_protocol::RUNTIME_HANDOFF_VERSION,
            })
            .unwrap()
        );
        return ExitCode::SUCCESS;
    }
    if arguments.first().and_then(|argument| argument.to_str())
        == Some(tickloop_agent_daemon::mcp_supervisor::SUPERVISOR_ARGUMENT)
    {
        return match tickloop_agent_daemon::mcp_supervisor::run() {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("{error}");
                ExitCode::FAILURE
            }
        };
    }
    let invoked_as_tickhost = std::env::current_exe()
        .is_ok_and(|executable| executable.file_name() == Some(TICKHOST_EXECUTABLE.as_ref()));
    if invoked_as_tickhost {
        return tickhost::cli::run(arguments);
    }
    let legacy = match legacy_fixture() {
        Ok(legacy) => legacy,
        Err(error) => {
            eprintln!("Error: {error}");
            return ExitCode::FAILURE;
        }
    };
    if let Some((fixture, directory)) = legacy {
        let result = if arguments
            .first()
            .is_some_and(|argument| argument == LEGACY_DAEMON_FLAG)
        {
            match arguments.as_slice() {
                [_, endpoint] if Path::new(endpoint).is_absolute() => {
                    runtime().block_on(run_legacy_daemon(Path::new(endpoint), &directory, &fixture))
                }
                _ => Err("legacy daemon fixture requires one absolute socket path".into()),
            }
        } else if arguments == ["--version"] {
            println!("Tickloop v{}", fixture.version);
            Ok(())
        } else if !is_product_process(&arguments)
            && arguments
                .first()
                .is_none_or(|argument| argument != "probe-upgrade")
        {
            write_legacy_marker(&directory.join("legacy-ui-opened")).map(|()| {
                println!("Tickloop v{}", fixture.version);
            })
        } else {
            return run_fixture_entry(&arguments);
        };
        return match result {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("Error: {error}");
                ExitCode::FAILURE
            }
        };
    }
    run_fixture_entry(&arguments)
}

fn run_fixture_entry(arguments: &[OsString]) -> ExitCode {
    let result = match arguments.first().and_then(|argument| argument.to_str()) {
        Some(CLI_MODE) => run_cli(arguments[1..].to_vec()),
        Some("probe-upgrade") if arguments.len() == 3 => {
            runtime().block_on(graceful_upgrade::run_probe(
                Path::new(&arguments[1]),
                Path::new(&arguments[2]),
            ));
            Ok(())
        }
        _ if is_worker_entry(arguments) && std::env::var_os(TRANSPORT_FILE).is_some() => {
            runtime().block_on(worker(arguments))
        }
        _ if is_product_process(arguments) => run_product_process(arguments),
        // No arguments, `--list`, `--exact`, `--nocapture`, a filter or `--help`.
        _ => return ExitCode::SUCCESS,
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!(
                "Error: {}",
                tickloop_agent_console::terminal::safe_text(&error)
            );
            ExitCode::FAILURE
        }
    }
}

fn legacy_fixture() -> Result<Option<(LegacyFixture, PathBuf)>, String> {
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    let directory = executable
        .parent()
        .ok_or("fixture executable has no directory")?;
    let bytes = match std::fs::read(directory.join(LEGACY_CONFIG_FILE)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    let fixture = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    Ok(Some((fixture, directory.to_path_buf())))
}

fn write_legacy_marker(path: &Path) -> Result<(), String> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .and_then(|mut file| file.write_all(b"observed\n"))
        .map_err(|error| error.to_string())
}

async fn run_legacy_daemon(
    endpoint: &Path,
    directory: &Path,
    fixture: &LegacyFixture,
) -> Result<(), String> {
    use tickloop_agent_client::{daemon_management, frame};
    use tickloop_agent_protocol::{
        AccessLimits, PROTOCOL_MAJOR, ServerHello, ServerMessage,
        daemon_management::{CONTROL_MAJOR, Hello, Reply, Request},
    };
    use tokio::net::UnixListener;
    use tokio_util::sync::CancellationToken;
    use uuid::Uuid;

    let listener = UnixListener::bind(endpoint).map_err(|error| error.to_string())?;
    std::fs::set_permissions(endpoint, std::fs::Permissions::from_mode(0o600))
        .map_err(|error| error.to_string())?;
    let identity = std::fs::symlink_metadata(endpoint).map_err(|error| error.to_string())?;
    let epoch = Uuid::new_v4();
    let stopping = CancellationToken::new();
    let mut connections = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            () = stopping.cancelled() => break,
            accepted = listener.accept() => {
                let (mut stream, _) = accepted.map_err(|error| error.to_string())?;
                let stopping = stopping.clone();
                let marker = directory.join("legacy-stopped");
                let build = format!("{}+legacy", fixture.version);
                let management = fixture.management.unwrap_or(true);
                connections.spawn(async move {
                    frame::verify_peer(&stream).map_err(|error| error.to_string())?;
                    let Some(hello) = daemon_management::read::<_, serde_json::Value>(&mut stream).await? else { return Ok::<(), String>(()); };
                    if management && hello["Hello"]["major"] == CONTROL_MAJOR && hello["Hello"]["minor"] == 0 {
                        daemon_management::write(&mut stream, &Reply::Hello(Hello { major: CONTROL_MAJOR, minor: 0, daemon_epoch: epoch })).await?;
                        if let Some(Request::ShutdownDaemon { request_id, daemon_epoch }) = daemon_management::read(&mut stream).await?
                            && daemon_epoch == epoch {
                            write_legacy_marker(&marker)?;
                            daemon_management::write(&mut stream, &Reply::DaemonStopped { request_id }).await?;
                            stopping.cancel();
                        }
                    } else if hello.get("Hello").is_some() {
                        daemon_management::write(&mut stream, &ServerMessage::Hello(ServerHello {
                            process_id: std::process::id(), major: PROTOCOL_MAJOR - 1, minor: 0,
                            daemon_epoch: epoch, session_id: Uuid::new_v4(), capabilities: vec![],
                            limits: AccessLimits::DEFAULT.into(), build: Some(build), executable: None,
                            upgrade_protocol: None, handoff: None,
                        })).await?;
                    }
                    Ok(())
                });
            }
            Some(completed) = connections.join_next(), if !connections.is_empty() => {
                let _ = completed.map_err(|error| error.to_string())?;
            }
        }
    }
    connections.abort_all();
    drop(listener);
    if std::fs::symlink_metadata(endpoint)
        .is_ok_and(|current| current.dev() == identity.dev() && current.ino() == identity.ino())
    {
        std::fs::remove_file(endpoint).map_err(|error| error.to_string())?;
    }
    Ok(())
}

/// Whether `arguments` are a whole entry the product starts itself, so that a libtest filter
/// such as `daemon` or `worker` stays a probe.
fn is_product_process(arguments: &[OsString]) -> bool {
    let daemon_run = DAEMON_RUN_ARGUMENTS.len();
    is_worker_entry(arguments)
        || (arguments.len() > daemon_run && arguments[..daemon_run] == DAEMON_RUN_ARGUMENTS)
        || arguments == ATTACH_ARGUMENTS
}

fn is_worker_entry(arguments: &[OsString]) -> bool {
    matches!(arguments, [entry, endpoint] if entry == WORKER_ARGUMENT && Path::new(endpoint).is_absolute())
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}

fn run_product_process(arguments: &[OsString]) -> Result<(), String> {
    runtime()
        .block_on(tickloop_agent::run_internal(arguments))
        .expect("run_internal serves every product process entry")
}

fn run_cli(mut arguments: Vec<OsString>) -> Result<(), String> {
    if let Some(index) = arguments
        .iter()
        .position(|argument| argument == TRANSPORT_FIXTURE_FLAG)
    {
        let path = arguments.remove(index + 1);
        arguments.remove(index);
        use_transport_fixture(&path);
    }
    runtime().block_on(async {
        let entry = tickloop_agent::client_entry(&arguments);
        if let Some(code) = tickloop_agent::run_noninteractive(entry, &arguments).await {
            if code != 0 {
                std::process::exit(code);
            }
            return Ok(());
        }
        match tickloop_agent::run_internal(&arguments).await {
            Some(result) => result,
            None => {
                tickloop_agent_console::terminal::run_with_arguments(
                    arguments
                        .into_iter()
                        .map(|argument| argument.into_string().unwrap()),
                )
                .await
            }
        }
    })
}

/// Points this CLI, and through its environment the daemon and workers it starts, at the
/// fixture transport in `path`.
fn use_transport_fixture(path: &OsString) {
    // This standalone test process sets its transport before starting Tokio threads.
    unsafe {
        std::env::set_var(TRANSPORT_FILE, path);
        std::env::set_var("DEEPSEEK_API_KEY", "fixture-compiled-selection-key");
    }
    if let Ok(bytes) = std::fs::read(path)
        && let Ok(transport) = serde_json::from_slice::<ModelConfig>(&bytes)
        && let Some(model) = tickloop_agent_model::cards::ModelCatalog::defaults()
            .get(&transport.provider_id, &transport.model_id)
        && let Some(root) = std::env::var_os("TICKLOOP_CONFIG_DIR")
    {
        let root = PathBuf::from(root);
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("settings.json");
        let mut state = if path.exists() {
            serde_json::from_slice::<serde_json::Value>(&std::fs::read(&path).unwrap()).unwrap()
        } else {
            serde_json::json!({})
        };
        // The fixture replaces worker transport; the View must not query the compiled Provider.
        state["promptSuggestionEnabled"] = serde_json::json!(false);
        state["defaultModel"] =
            serde_json::json!({"provider":model.provider_id,"model":model.model_id});
        std::fs::write(path, serde_json::to_vec(&state).unwrap()).unwrap();
    }
}

async fn worker(arguments: &[OsString]) -> Result<(), String> {
    let fixture = std::fs::read(std::env::var_os(TRANSPORT_FILE).unwrap())
        .map_err(|error| error.to_string())?;
    let transport: ModelConfig =
        serde_json::from_slice(&fixture).map_err(|error| error.to_string())?;
    tickloop_agent::name_this_build();
    tickloop_agent_daemon::run_worker_with_models(&arguments[1..], move |config_root| {
        let local = LocalModelClients::new(config_root);
        let inner = local
            .client(&transport)
            .map_err(|error| error.to_string())?;
        Ok(Arc::new(TransportModels {
            local,
            transport: Arc::new(TransportClient {
                inner,
                config: transport,
            }),
        }))
    })
    .await
}

/// Every model a Chat selects fails where the Provider client would, but keeps the fixture
/// transport.
struct TransportModels {
    local: LocalModelClients,
    transport: Arc<dyn tickloop_agent_model::ModelClient>,
}

impl ModelClients for TransportModels {
    fn client(
        &self,
        config: &ModelConfig,
    ) -> Result<Arc<dyn tickloop_agent_model::ModelClient>, tickloop_agent_model::ModelError> {
        self.local.client(config)?;
        Ok(self.transport.clone())
    }
}

struct TransportClient {
    inner: Arc<dyn tickloop_agent_model::ModelClient>,
    config: ModelConfig,
}

#[async_trait]
impl tickloop_agent_model::ModelClient for TransportClient {
    fn connection_id(&self) -> Option<String> {
        self.inner.connection_id()
    }

    async fn generate_title(
        &self,
        request: tickloop_agent_model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_agent_model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_agent_model::ModelOutput, tickloop_agent_model::ModelError> {
        Ok(tickloop_agent_test_support::title_output(request))
    }

    fn identity(&self) -> tickloop_agent_protocol::ModelIdentity {
        self.inner.identity()
    }
    async fn complete(
        &self,
        mut request: tickloop_agent_model::ModelRequest,
        events: tokio::sync::mpsc::UnboundedSender<tickloop_agent_model::ModelDelta>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_agent_model::ModelOutput, tickloop_agent_model::ModelError> {
        if tickloop_agent_model::validate_reasoning_effort(&self.config, request.reasoning_effort)
            .is_err()
        {
            request.reasoning_effort = None;
        }
        self.inner.complete(request, events, cancel).await
    }
}

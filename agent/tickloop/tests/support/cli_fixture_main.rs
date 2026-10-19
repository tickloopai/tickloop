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
    path::{Path, PathBuf},
    process::ExitCode,
    sync::Arc,
};
use support::fixture_cli::{BUILD_IDENTITY_MODE, CLI_MODE, build_identity};
use tickloop_agent_client::DAEMON_RUN_ARGUMENTS;
use tickloop_agent_daemon::WORKER_ARGUMENT;
use tickloop_agent_model::model_client;
use tickloop_agent_protocol::{ATTACH_ARGUMENTS, ModelConfig};
use tickloop_agent_runtime::hosted::controller::TICKHOST_EXECUTABLE;

const TRANSPORT_FILE: &str = "TICKLOOP_TRANSPORT_FIXTURE";
const TRANSPORT_FIXTURE_FLAG: &str = "--transport-fixture";

fn main() -> ExitCode {
    let arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
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
    let invoked_as_tickhost = std::env::current_exe()
        .is_ok_and(|executable| executable.file_name() == Some(TICKHOST_EXECUTABLE.as_ref()));
    if invoked_as_tickhost {
        return tickhost::cli::run(arguments);
    }
    let result = match arguments.first().and_then(|argument| argument.to_str()) {
        Some(CLI_MODE) => run_cli(arguments[1..].to_vec()),
        Some("probe-upgrade") if arguments.len() == 3 => {
            runtime().block_on(graceful_upgrade::run_probe(
                Path::new(&arguments[1]),
                Path::new(&arguments[2]),
            ));
            Ok(())
        }
        _ if is_worker_entry(&arguments) && std::env::var_os(TRANSPORT_FILE).is_some() => {
            runtime().block_on(worker(&arguments))
        }
        _ if is_product_process(&arguments) => run_product_process(&arguments),
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
    tickloop_agent_daemon::run_worker_with_model_clients(
        &arguments[1..],
        Arc::new(move |selection, root| {
            model_client(selection, root)?;
            let inner = model_client(transport.clone(), root)?;
            Ok(Arc::new(TransportClient {
                inner,
                config: transport.clone(),
            }))
        }),
    )
    .await
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

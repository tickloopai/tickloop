//! Test executable providing a local model transport to the real CLI/daemon/worker chain.
//! No fixture definition enters the compiled model catalog or saved model selection.
//! The arguments pick the process: `--cli` runs the product CLI, a whole product process entry
//! runs as the product starts it, and anything else is a libtest probe of a target without tests.
#[path = "mod.rs"]
mod support;
use async_trait::async_trait;
use serde::Deserialize;
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    sync::Arc,
};
use support::fixture_cli::CLI_MODE;
use tickloop_agent::{
    agent_access::*,
    agent_daemon::{DAEMON_RUN_ARGUMENTS, WORKER_ARGUMENT},
    hosted::{controller::TICKHOST_RUN_ARGUMENTS, popup::ATTACH_ARGUMENTS},
    model::{ModelConfig, model_client},
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use uuid::Uuid;

const TRANSPORT_FILE: &str = "TICKLOOP_TRANSPORT_FIXTURE";
const TRANSPORT_FIXTURE_FLAG: &str = "--transport-fixture";
const TICK_LAUNCH_MODE: &str = "--tick-launch";

#[derive(Deserialize)]
struct Bootstrap {
    epoch: Uuid,
    launch: LaunchSpec,
    control: tickloop_agent::hosted::worker_control::WorkerControlGrant,
    limits: tickloop_agent::protocol::AccessLimits,
}

fn main() {
    let arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
    let result = match arguments.first().and_then(|argument| argument.to_str()) {
        Some(CLI_MODE) => run_cli(arguments[1..].to_vec()),
        Some(TICK_LAUNCH_MODE) => run_tick_launch(&arguments),
        _ if is_worker_entry(&arguments) && std::env::var_os(TRANSPORT_FILE).is_some() => {
            runtime().block_on(worker(&arguments))
        }
        _ if is_product_process(&arguments) => run_product_process(&arguments),
        // No arguments, `--list`, `--exact`, `--nocapture`, a filter or `--help`.
        _ => return,
    };
    if let Err(error) = result {
        eprintln!("Error: {}", tickloop_agent::terminal::safe_text(&error));
        std::process::exit(1);
    }
}

/// Whether `arguments` are a whole entry the product starts itself, so that a libtest filter
/// such as `daemon` or `worker` stays a probe.
fn is_product_process(arguments: &[OsString]) -> bool {
    let daemon_run = DAEMON_RUN_ARGUMENTS.len();
    is_worker_entry(arguments)
        || (arguments.len() > daemon_run && arguments[..daemon_run] == DAEMON_RUN_ARGUMENTS)
        || arguments == TICKHOST_RUN_ARGUMENTS
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
        .block_on(tickloop_agent::agent_daemon::run_internal(arguments))
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
        match tickloop_agent::agent_daemon::run_internal(&arguments).await {
            Some(result) => result,
            None => {
                tickloop_agent::terminal::run_with_arguments(
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
        && let Some(model) = tickloop_agent::model::cards::ModelCatalog::defaults()
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

/// A test launcher for hosted Ticks: the same launch contract `tick run` uses.
fn run_tick_launch(arguments: &[OsString]) -> ! {
    let command = arguments
        .iter()
        .skip_while(|argument| *argument != "--")
        .skip(1)
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let status = runtime().block_on(async {
        let context = tick_connect::launch::LaunchContext::from_environment()?
            .ok_or("no Tick launch context")?;
        let host = context
            .verify_foreground()
            .await
            .map_err(|error| error.to_string())?;
        tick_connect::launch::run(host, command, std::env::current_dir().unwrap())
            .await
            .map_err(|error| error.to_string())
    });
    match status {
        Ok(status) if status.success() => std::process::exit(0),
        Ok(_) => std::process::exit(1),
        Err(error) => {
            eprintln!("fixture launcher: {error}");
            std::process::exit(2);
        }
    }
}

async fn worker(arguments: &[OsString]) -> Result<(), String> {
    let endpoint = PathBuf::from(&arguments[1]);
    let mut stdin = BufReader::new(tokio::io::stdin());
    let mut bytes = Vec::new();
    (&mut stdin)
        .take(tickloop_agent::agent_daemon::WORKER_BOOTSTRAP_MAX_BYTES as u64 + 1)
        .read_until(b'\n', &mut bytes)
        .await
        .map_err(|error| error.to_string())?;
    let bootstrap: Bootstrap = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    let fixture = std::fs::read(std::env::var_os(TRANSPORT_FILE).unwrap())
        .map_err(|error| error.to_string())?;
    let config: ModelConfig =
        serde_json::from_slice(&fixture).map_err(|error| error.to_string())?;
    let inner = model_client(config.clone(), bootstrap.launch.config_root.as_deref())
        .map_err(|error| error.to_string())?;
    let client: Arc<dyn tickloop_agent::model::ModelClient> =
        Arc::new(TransportClient { inner, config });
    let agent = tickloop_agent::agent_daemon::build_runtime_with_client(
        &bootstrap.launch,
        client.clone(),
        Some(bootstrap.control),
    )
    .await?;
    let access = Arc::new(TransportAccess {
        inner: EmbeddedAgentAccess::for_worker(agent.clone(), bootstrap.epoch),
        client,
    });
    let listener = tokio::net::UnixListener::bind(&endpoint).map_err(|error| error.to_string())?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&endpoint, std::fs::Permissions::from_mode(0o600))
        .map_err(|error| error.to_string())?;
    let mut probe = [0u8; 1];
    let result = tokio::select! {
        result = serve_worker_listener(listener, access, bootstrap.epoch, bootstrap.limits) => result.map_err(|error| error.to_string()),
        result = stdin.read(&mut probe) => result.map(|_| ()).map_err(|error| error.to_string()),
    };
    agent.shutdown().await;
    let _ = std::fs::remove_file(endpoint);
    result
}

struct TransportAccess {
    inner: EmbeddedAgentAccess,
    client: Arc<dyn tickloop_agent::model::ModelClient>,
}

#[async_trait]
impl AgentAccess for TransportAccess {
    async fn observe(&self, query: ObservationRequest) -> Result<Observation, AccessError> {
        self.inner.observe(query).await
    }

    async fn execute(&self, command: CommandEnvelope) -> Result<CommandReceipt, AccessError> {
        let configured = match &command.command {
            AgentCommand::Configure {
                execution,
                change: ConfigurationChange::Model { .. },
                ..
            } => Some(*execution),
            _ => None,
        };
        let receipt = self.inner.execute(command).await?;
        if receipt.clone().into_result().is_ok()
            && let Some(execution) = configured
        {
            let snapshot = self
                .inner
                .observe(ObservationRequest::Agent {
                    execution,
                    detail: AgentDetailQuery::Snapshot,
                    after: None,
                })
                .await?;
            if let ObservationValue::Agent(snapshot) = snapshot.initial {
                self.inner
                    .runtime()
                    .set_model_with_thinking(
                        snapshot.conversation.id,
                        snapshot.conversation.model,
                        self.client.clone(),
                        snapshot.conversation.reasoning_effort,
                    )
                    .await?;
            }
        }
        Ok(receipt)
    }

    async fn show_view(&self, update: DisplayUpdate) -> Result<DisplayReceipt, AccessError> {
        self.inner.show_view(update).await
    }
}

struct TransportClient {
    inner: Arc<dyn tickloop_agent::model::ModelClient>,
    config: ModelConfig,
}

#[async_trait]
impl tickloop_agent::model::ModelClient for TransportClient {
    fn connection_id(&self) -> Option<String> {
        self.inner.connection_id()
    }

    async fn generate_title(
        &self,
        request: tickloop_agent::model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_agent::model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_agent::model::ModelOutput, tickloop_agent::model::ModelError> {
        Ok(support::title_output(request))
    }

    fn identity(&self) -> tickloop_agent::model::ModelIdentity {
        self.inner.identity()
    }
    async fn complete(
        &self,
        mut request: tickloop_agent::model::ModelRequest,
        events: tokio::sync::mpsc::UnboundedSender<tickloop_agent::model::ModelDelta>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_agent::model::ModelOutput, tickloop_agent::model::ModelError> {
        if tickloop_agent::model::validate_reasoning_effort(&self.config, request.reasoning_effort)
            .is_err()
        {
            request.reasoning_effort = None;
        }
        self.inner.complete(request, events, cancel).await
    }
}

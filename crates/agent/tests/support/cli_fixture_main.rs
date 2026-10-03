//! Test executable providing a local model transport to the real CLI/owner/worker chain.
//! No fixture definition enters the compiled model catalog or saved model selection.
use async_trait::async_trait;
use serde::Deserialize;
use std::{ffi::OsString, path::PathBuf, sync::Arc};
use tickloop_agent::{
    agent_access::*,
    model::{ModelConfig, model_client},
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use uuid::Uuid;

const TRANSPORT_FILE: &str = "TICKLOOP_TRANSPORT_FIXTURE";

#[derive(Deserialize)]
struct Bootstrap {
    epoch: Uuid,
    launch: LaunchSpec,
}

fn main() {
    let mut arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
    if let Some(index) = arguments
        .iter()
        .position(|arg| arg == "--transport-fixture")
    {
        let path = arguments.remove(index + 1);
        arguments.remove(index);
        // This standalone test process sets its transport before starting Tokio threads.
        unsafe {
            std::env::set_var(TRANSPORT_FILE, &path);
        }
    }
    if !arguments
        .first()
        .is_some_and(|arg| arg == "--agent-owner" || arg == "--agent-worker")
        && let Some(path) = std::env::var_os(TRANSPORT_FILE)
        && let Ok(bytes) = std::fs::read(path)
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
    if std::env::var_os(TRANSPORT_FILE).is_some() {
        unsafe {
            std::env::set_var("DEEPSEEK_API_KEY", "fixture-compiled-selection-key");
        }
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let result = runtime.block_on(async {
        if arguments.is_empty()
            || arguments
                .iter()
                .all(|argument| argument.to_string_lossy().starts_with("--test-threads="))
        {
            return Ok(());
        }
        if arguments.first().is_some_and(|arg| arg == "--agent-worker")
            && std::env::var_os(TRANSPORT_FILE).is_some()
        {
            return worker(&arguments).await;
        }
        match tickloop_agent::agent_daemon::run_internal(&arguments).await {
            Some(result) => result,
            None => {
                tickloop_agent::terminal::run_with_arguments(
                    arguments.into_iter().map(|arg| arg.into_string().unwrap()),
                )
                .await
            }
        }
    });
    if let Err(error) = result {
        eprintln!("Error: {}", tickloop_agent::terminal::safe_text(&error));
        std::process::exit(1);
    }
}

async fn worker(arguments: &[OsString]) -> Result<(), String> {
    let endpoint = PathBuf::from(&arguments[1]);
    let mut stdin = BufReader::new(tokio::io::stdin());
    let mut bytes = Vec::new();
    (&mut stdin)
        .take(tickloop_agent::agent_access::MAX_FRAME_BYTES as u64 + 1)
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
    let agent =
        tickloop_agent::agent_daemon::build_runtime_with_client(&bootstrap.launch, client.clone())
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
        result = serve_worker_listener(listener, access, bootstrap.epoch) => result.map_err(|error| error.to_string()),
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

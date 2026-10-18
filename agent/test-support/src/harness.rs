//! Agents built around model clients a test controls.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

use tickloop_agent_model::{ModelClient, ModelClients, ModelError};
use tickloop_agent_protocol::{AgentHostOptions, ModelConfig};
use tickloop_agent_runtime::{
    Agent, HarnessOptions, HarnessSeams, ToolPolicy, environment::LocalEnvironment,
    transcript::JsonlTranscriptStore,
};

/// Serves the client the test chose last for every model a Chat selects, so a model switch gets
/// the client the test means.
pub struct ChosenModels {
    chosen: Mutex<Arc<dyn ModelClient>>,
}

impl ChosenModels {
    pub fn new(client: Arc<dyn ModelClient>) -> Arc<Self> {
        Arc::new(Self {
            chosen: Mutex::new(client),
        })
    }

    pub fn choose(&self, client: Arc<dyn ModelClient>) {
        *self.chosen.lock().unwrap() = client;
    }
}

impl ModelClients for ChosenModels {
    fn client(&self, _: &ModelConfig) -> Result<Arc<dyn ModelClient>, ModelError> {
        Ok(self.chosen.lock().unwrap().clone())
    }
}

/// `Agent::new_embedded` with these model clients.
pub fn embedded_agent(model: ModelConfig, models: Arc<dyn ModelClients>) -> Agent {
    Agent::with_seams(
        HarnessSeams {
            transcripts: None,
            environment: Some(Arc::new(LocalEnvironment::bare())),
            models,
        },
        local_options(model, None),
    )
    .unwrap()
}

/// `Agent::open_embedded` with these model clients.
pub fn opened_agent(root: PathBuf, model: ModelConfig, models: Arc<dyn ModelClients>) -> Agent {
    let store = JsonlTranscriptStore::open(root).unwrap();
    let settings = tickloop_agent_config::UserSettings::in_root(store.root());
    Agent::with_seams(
        HarnessSeams {
            transcripts: Some(Arc::new(store)),
            environment: Some(Arc::new(LocalEnvironment::bare())),
            models,
        },
        local_options(model, Some(settings)),
    )
    .unwrap()
}

fn local_options(
    model: ModelConfig,
    settings: Option<tickloop_agent_config::UserSettings>,
) -> HarnessOptions {
    HarnessOptions {
        model,
        settings,
        host: AgentHostOptions::embedded_defaults(),
        tools: ToolPolicy::local(),
        host_rules: Vec::new(),
        waits: tickloop_agent_runtime::WaitHosting::Process,
    }
}

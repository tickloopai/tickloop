//! Agents built around model clients a test controls.

use std::sync::{Arc, Mutex};

use tickloop_agent_model::{ModelClient, ModelClients, ModelError};
use tickloop_agent_protocol::ModelConfig;
use tickloop_agent_runtime::{Agent, HarnessOptions, HarnessSeams, environment::FolderEnvironment};

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
            environment: Some(Arc::new(FolderEnvironment::bare())),
            models,
        },
        HarnessOptions::local(model),
    )
    .unwrap()
}

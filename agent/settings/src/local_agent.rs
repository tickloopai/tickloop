//! File-backed Agent composition for local integration tests.
use crate::UserSettings;
use std::{path::PathBuf, sync::Arc};
use tickloop_agent_model::{ModelClient, ModelClients, ModelError};
use tickloop_agent_protocol::{AgentHostOptions, ModelConfig};
use tickloop_agent_runtime::{
    Agent, AgentError, HarnessOptions, HarnessSeams, environment::LocalEnvironment,
    transcript::JsonlTranscriptStore,
};

pub trait LocalAgent {
    fn open(
        root: PathBuf,
        model: ModelConfig,
        client: Arc<dyn ModelClient>,
    ) -> Result<Agent, AgentError>;
    fn open_embedded(
        root: PathBuf,
        model: ModelConfig,
        client: Arc<dyn ModelClient>,
    ) -> Result<Agent, AgentError>;
    fn open_with_options(
        root: PathBuf,
        model: ModelConfig,
        client: Arc<dyn ModelClient>,
        host: AgentHostOptions,
    ) -> Result<Agent, AgentError>;
    fn new_with_user_settings_embedded(
        model: ModelConfig,
        client: Arc<dyn ModelClient>,
        settings: UserSettings,
    ) -> Agent;
}

struct OneClient(Arc<dyn ModelClient>);
impl ModelClients for OneClient {
    fn client(&self, _: &ModelConfig) -> Result<Arc<dyn ModelClient>, ModelError> {
        Ok(self.0.clone())
    }
}

impl LocalAgent for Agent {
    fn open(
        root: PathBuf,
        model: ModelConfig,
        client: Arc<dyn ModelClient>,
    ) -> Result<Agent, AgentError> {
        Self::open_with_options(root, model, client, AgentHostOptions::default())
    }
    fn open_embedded(
        root: PathBuf,
        model: ModelConfig,
        client: Arc<dyn ModelClient>,
    ) -> Result<Agent, AgentError> {
        Self::open_with_options(root, model, client, AgentHostOptions::embedded_defaults())
    }
    fn open_with_options(
        root: PathBuf,
        model: ModelConfig,
        client: Arc<dyn ModelClient>,
        host: AgentHostOptions,
    ) -> Result<Agent, AgentError> {
        let store = JsonlTranscriptStore::open(root)
            .map_err(|error| AgentError::Storage(error.to_string()))?;
        let root = store.root().to_owned();
        Agent::with_seams(
            HarnessSeams {
                transcripts: Some(Arc::new(store)),
                environment: Some(Arc::new(LocalEnvironment::bare())),
                models: Arc::new(OneClient(client)),
            },
            HarnessOptions {
                settings: Some(Arc::new(UserSettings::in_root(&root))),
                local_state_root: Some(root),
                host,
                ..HarnessOptions::local(model)
            },
        )
    }
    fn new_with_user_settings_embedded(
        model: ModelConfig,
        client: Arc<dyn ModelClient>,
        settings: UserSettings,
    ) -> Agent {
        Agent::with_seams(
            HarnessSeams {
                transcripts: None,
                environment: Some(Arc::new(LocalEnvironment::bare())),
                models: Arc::new(OneClient(client)),
            },
            HarnessOptions {
                local_state_root: settings.root().map(ToOwned::to_owned),
                settings: Some(Arc::new(settings)),
                ..HarnessOptions::local(model)
            },
        )
        .expect("local settings composition is valid")
    }
}

/// `Agent::open_embedded` with these model clients.
pub fn opened_agent(root: PathBuf, model: ModelConfig, models: Arc<dyn ModelClients>) -> Agent {
    let store = JsonlTranscriptStore::open(root).unwrap();
    let settings = crate::UserSettings::in_root(store.root());
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

fn local_options(model: ModelConfig, settings: Option<crate::UserSettings>) -> HarnessOptions {
    HarnessOptions {
        local_state_root: settings
            .as_ref()
            .and_then(|settings| settings.root().map(ToOwned::to_owned)),
        settings: settings.map(|settings| {
            Arc::new(settings) as Arc<dyn tickloop_agent_policy::settings::SettingsStore>
        }),
        ..HarnessOptions::local(model)
    }
}

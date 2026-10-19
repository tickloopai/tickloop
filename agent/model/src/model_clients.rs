use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard},
};

use sha2::{Digest, Sha256};

use tickloop_agent_protocol::{ChatId, ModelAuthSource, ModelConfig, ModelProvider};

use crate::{
    CredentialSource, HttpModelClient, LocalCredentials, ModelClient, ModelEndpoint, ModelError,
    VendorEndpoint, codex, validate_model_config,
};

/// Builds the client for each model configuration a Chat selects. Clients built by one
/// instance share its private-history provenance and never see another instance's.
pub trait ModelClients: Send + Sync {
    fn client(&self, config: &ModelConfig) -> Result<Arc<dyn ModelClient>, ModelError>;
}

/// (Chat, credential scope, private block fingerprint) of a private block a service returned.
type PrivateBlockKey = (ChatId, [u8; 32], [u8; 32]);

fn identity_digest(identity: &str) -> [u8; 32] {
    Sha256::digest(identity.as_bytes()).into()
}

/// The credential identity (as its digest) under which each private block was produced;
/// `None` once two identities produced the same block.
#[derive(Default)]
pub(crate) struct PrivateProvenance {
    blocks: Mutex<HashMap<PrivateBlockKey, Option<[u8; 32]>>>,
}

impl PrivateProvenance {
    fn blocks(&self) -> MutexGuard<'_, HashMap<PrivateBlockKey, Option<[u8; 32]>>> {
        self.blocks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(crate) fn proves_all(
        &self,
        keys: impl IntoIterator<Item = PrivateBlockKey>,
        identity: &str,
    ) -> bool {
        let identity = identity_digest(identity);
        let blocks = self.blocks();
        keys.into_iter()
            .all(|key| blocks.get(&key).copied().flatten() == Some(identity))
    }

    pub(crate) fn record(&self, keys: impl IntoIterator<Item = PrivateBlockKey>, identity: &str) {
        let identity = identity_digest(identity);
        let mut blocks = self.blocks();
        for key in keys {
            let recorded = blocks.entry(key).or_insert(Some(identity));
            if *recorded != Some(identity) {
                *recorded = None;
            }
        }
    }

    pub(crate) fn close_chat(&self, chat_id: ChatId) {
        self.blocks()
            .retain(|(recorded_chat, _, _), _| *recorded_chat != chat_id);
    }
}

/// HTTP model clients for API Providers through one endpoint and credential source.
pub struct HttpModelClients {
    endpoint: Arc<dyn ModelEndpoint>,
    credentials: Arc<dyn CredentialSource>,
    provenance: Arc<PrivateProvenance>,
}

impl HttpModelClients {
    pub fn new(endpoint: Arc<dyn ModelEndpoint>, credentials: Arc<dyn CredentialSource>) -> Self {
        Self {
            endpoint,
            credentials,
            provenance: Arc::default(),
        }
    }

    pub(crate) fn http_client(&self, config: ModelConfig) -> Result<HttpModelClient, ModelError> {
        HttpModelClient::assemble(
            config,
            self.endpoint.clone(),
            self.credentials.clone(),
            self.provenance.clone(),
        )
    }
}

impl ModelClients for HttpModelClients {
    fn client(&self, config: &ModelConfig) -> Result<Arc<dyn ModelClient>, ModelError> {
        validate_model_config(config)?;
        Ok(Arc::new(self.http_client(config.clone())?))
    }
}

/// Clients that reach vendors directly with the credentials under the Agent state directory,
/// including Codex subscriptions.
pub struct LocalModelClients {
    state_root: Option<PathBuf>,
    http: HttpModelClients,
}

impl LocalModelClients {
    pub fn new(state_root: Option<PathBuf>) -> Self {
        let credentials = Arc::new(LocalCredentials::new(state_root.clone()));
        Self {
            state_root,
            http: HttpModelClients::new(Arc::new(VendorEndpoint), credentials),
        }
    }
}

impl ModelClients for LocalModelClients {
    fn client(&self, config: &ModelConfig) -> Result<Arc<dyn ModelClient>, ModelError> {
        validate_model_config(config)?;
        let state_dir = self.state_root.as_deref();
        match config.provider {
            ModelProvider::Api => {
                if crate::auth::minimax::MiniMaxRegion::from_auth(config.auth_source).is_some()
                    && state_dir.is_none()
                {
                    return Err(ModelError::Configuration(
                        "MiniMax OAuth requires an Agent state directory".into(),
                    ));
                }
                if config.auth_source != ModelAuthSource::ApiKeyEnv && state_dir.is_none() {
                    return Err(ModelError::Configuration(
                        "Provider sign-in requires an Agent state directory".into(),
                    ));
                }
                Ok(Arc::new(self.http.http_client(config.clone())?))
            }
            ModelProvider::OpenAiCodex => {
                let state_dir = state_dir.ok_or_else(|| {
                    ModelError::Configuration(
                        "Codex subscription requires an Agent state directory".into(),
                    )
                })?;
                let client = codex::CodexModelClient::new(config.clone(), state_dir)?;
                #[cfg(any(test, feature = "test-support"))]
                let client =
                    if let Some(endpoint) = std::env::var_os("TICKLOOP_TEST_CODEX_RESPONSES") {
                        let endpoint = endpoint.into_string().map_err(|_| {
                            ModelError::Configuration(
                                "Codex test endpoint must be numeric loopback HTTP".into(),
                            )
                        })?;
                        client.with_loopback_test_endpoint(&endpoint)?
                    } else {
                        client
                    };
                Ok(Arc::new(client))
            }
        }
    }
}

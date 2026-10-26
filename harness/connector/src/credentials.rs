//! The MCP credential seam: where a Connector keeps one remote server's client registration and
//! token. The host decides where that is; Harness never chooses a file.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tick_oauth::OAuthToken;

/// One remote MCP Server's authorization, as the host stores it. It names the server address it
/// was granted for, so a token is never sent to another address after the configuration changes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectorCredential {
    pub server_url: String,
    pub registration: ClientRegistration,
    /// The RFC 8707 resource the token was requested for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
    /// Absent after logout, when the registration is kept for the next login.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<OAuthToken>,
}

/// The client Tickloop is at the authorization server, registered dynamically or configured.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientRegistration {
    pub authorization_server: String,
    pub client_id: String,
    pub redirect_uri: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the MCP credentials cannot be {0}")]
pub struct CredentialError(pub &'static str);

/// Held while a credential is read, renewed and written back, so Agents and logins that share it
/// do not renew it twice or overwrite each other. Dropping it releases the lock.
#[must_use = "dropping the lock releases it"]
pub struct CredentialLock {
    _guard: Box<dyn Send>,
}

impl CredentialLock {
    pub fn new(guard: impl Send + 'static) -> Self {
        Self {
            _guard: Box::new(guard),
        }
    }
}

/// Reads and writes one remote MCP Server's credential. `lock` is exclusive across every process
/// using the same credential; `load` and `save` never take it themselves.
#[async_trait]
pub trait ConnectorCredentials: Send + Sync {
    async fn lock(&self) -> Result<CredentialLock, CredentialError>;
    async fn load(&self) -> Result<Option<ConnectorCredential>, CredentialError>;
    async fn save(&self, credential: &ConnectorCredential) -> Result<(), CredentialError>;
}

/// Credentials that live as long as the process, for stateless hosts and tests.
#[derive(Default)]
pub struct InMemoryCredentials {
    lock: Arc<tokio::sync::Mutex<()>>,
    credential: Mutex<Option<ConnectorCredential>>,
}

impl InMemoryCredentials {
    pub fn current(&self) -> Option<ConnectorCredential> {
        self.credential.lock().unwrap().clone()
    }

    pub fn replace(&self, credential: Option<ConnectorCredential>) {
        *self.credential.lock().unwrap() = credential;
    }
}

#[async_trait]
impl ConnectorCredentials for InMemoryCredentials {
    async fn lock(&self) -> Result<CredentialLock, CredentialError> {
        Ok(CredentialLock::new(self.lock.clone().lock_owned().await))
    }

    async fn load(&self) -> Result<Option<ConnectorCredential>, CredentialError> {
        Ok(self.current())
    }

    async fn save(&self, credential: &ConnectorCredential) -> Result<(), CredentialError> {
        self.replace(Some(credential.clone()));
        Ok(())
    }
}

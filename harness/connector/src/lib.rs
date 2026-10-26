//! The Connector of a remote MCP Server that requires OAuth: logging in (discovery from the
//! server's 401 challenge or a configured metadata address, dynamic registration of a public
//! client or a configured client id, authorization code with S256 PKCE received on this machine
//! or pasted back), handing out and renewing the access token, and logging out.
//!
//! The host supplies where the credential lives through [`ConnectorCredentials`]. Tokens never
//! appear in errors or `Debug` output.

mod credentials;
mod http;
mod login;
mod tokens;

use std::fmt;
use std::future::Future;
use std::sync::Arc;

use reqwest::Url;
use tickloop_harness_base::network::is_loopback;
use tokio_util::sync::CancellationToken;

pub use credentials::{
    ClientRegistration, ConnectorCredential, ConnectorCredentials, CredentialError, CredentialLock,
    InMemoryCredentials,
};
pub use login::{LoginError, PendingLogin};
pub use tick_oauth::OAuthToken;
pub use tickloop_harness_base::oauth_callback::{CallbackError, CallbackPaste};
pub use tokens::{Logout, TokenError};

/// The `oauth` settings of a remote server entry.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OAuthSettings {
    /// A client registered with the authorization server in advance; registration is skipped.
    pub client_id: Option<String>,
    /// The loopback port the browser returns to; a free port when absent.
    pub callback_port: Option<u16>,
    /// The authorization server metadata to use instead of discovering it.
    pub authorization_server_metadata_url: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConfigurationError {
    #[error("the MCP server address must use https, or http on this machine")]
    ServerAddress,
    #[error("the authorization server metadata address must use https, or http on this machine")]
    MetadataAddress,
    #[error("the OAuth client id must not be empty")]
    ClientId,
}

/// An access token for the server's `Authorization: Bearer` header.
#[derive(Clone, PartialEq, Eq)]
pub struct AccessToken(String);

impl AccessToken {
    pub fn secret(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for AccessToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AccessToken([redacted])")
    }
}

/// One remote MCP Server's authorization, shared by the logins and Agents that use the same
/// credential.
#[derive(Clone)]
pub struct Connector {
    server_url: Url,
    settings: OAuthSettings,
    metadata_url: Option<Url>,
    credentials: Arc<dyn ConnectorCredentials>,
    http: http::PolicyClient,
}

impl Connector {
    pub fn new(
        server_url: &str,
        settings: OAuthSettings,
        credentials: Arc<dyn ConnectorCredentials>,
    ) -> Result<Self, ConfigurationError> {
        let server_url = Url::parse(server_url)
            .ok()
            .filter(http::is_allowed_address)
            .ok_or(ConfigurationError::ServerAddress)?;
        let metadata_url = settings
            .authorization_server_metadata_url
            .as_deref()
            .map(|address| {
                Url::parse(address)
                    .ok()
                    .filter(http::is_allowed_address)
                    .ok_or(ConfigurationError::MetadataAddress)
            })
            .transpose()?;
        if settings
            .client_id
            .as_deref()
            .is_some_and(|client_id| client_id.trim().is_empty())
        {
            return Err(ConfigurationError::ClientId);
        }
        let configured_on_this_machine =
            is_loopback(&server_url) || metadata_url.as_ref().is_some_and(is_loopback);
        Ok(Self {
            server_url,
            settings,
            metadata_url,
            credentials,
            http: http::PolicyClient::new(configured_on_this_machine),
        })
    }
}

/// The operation stopped because its cancellation token fired.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Cancelled;

/// Runs `future` unless `cancellation` fires first.
pub(crate) async fn cancellable<T>(
    cancellation: &CancellationToken,
    future: impl Future<Output = T>,
) -> Result<T, Cancelled> {
    tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(Cancelled),
        value = future => Ok(value),
    }
}

impl fmt::Debug for Connector {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Connector")
            .field("server_url", &self.server_url.as_str())
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

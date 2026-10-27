//! Logging in to a remote MCP Server: find its authorization server, register Tickloop or use
//! the configured client, send the user to the authorization URL, and exchange the code that
//! comes back through the loopback callback or a pasted redirect address.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use reqwest::Url;
use rmcp::transport::auth::AuthorizationMetadata;
use rmcp::transport::{
    AuthError, AuthorizationManager, AuthorizationRequest, AuthorizationSession,
};
use serde::Deserialize;
use tick_oauth::OAuthToken;
use tickloop_harness_base::oauth_callback::{
    CallbackError, CallbackListener, CallbackPages, CallbackPaste, PendingCallback,
};
use tokio_util::sync::CancellationToken;

use crate::credentials::{ClientRegistration, ConnectorCredential, CredentialError};
use crate::tokens::now_seconds;
use crate::{Cancelled, Connector, cancellable};

const CLIENT_NAME: &str = "Tickloop";
const CALLBACK_PATH: &str = "/callback";
const LOGIN_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// RFC 6749 only recommends `expires_in`; a token without it is renewed after an hour.
const DEFAULT_TOKEN_LIFETIME_SECONDS: u64 = 3600;
const CALLBACK_PAGES: CallbackPages = CallbackPages {
    received: "Authorization code received. Return to Tickloop; it will confirm whether the login succeeded.",
    state_mismatch: "This callback belongs to a different login. Return to the current browser tab or log in again from /mcp.",
    invalid_url: "The callback address is invalid or has no authorization code. If this browser tab is old, log in again from /mcp.",
    invalid_request: "Could not read the browser callback. Return to the current login or log in again from /mcp.",
    denied: "Authorization was denied.",
    failed: "Authorization failed.",
};

/// Why a login ended without saving a token. The texts never include server responses or
/// tokens.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LoginError {
    #[error("the login was cancelled")]
    Cancelled,
    #[error("the login timed out after {} minutes", LOGIN_TIMEOUT.as_secs() / 60)]
    TimedOut,
    /// The port given, or 0 when no free port was found.
    #[error("{}", unavailable_port(*.0))]
    CallbackPortUnavailable(u16),
    #[error("the authorization was denied")]
    Denied,
    #[error("the authorization server reported an error instead of an authorization code")]
    AuthorizationFailed,
    #[error("the login has already ended")]
    Ended,
    #[error("the server publishes no usable OAuth authorization server metadata")]
    Discovery,
    #[error("the authorization server does not support the authorization code flow with S256 PKCE")]
    Unsupported,
    #[error(
        "the authorization server did not register Tickloop as a client; configure oauth.clientId"
    )]
    Registration,
    #[error("the authorization code could not be exchanged for a token")]
    TokenExchange,
    #[error(transparent)]
    Credentials(#[from] CredentialError),
}

impl From<Cancelled> for LoginError {
    fn from(_: Cancelled) -> Self {
        Self::Cancelled
    }
}

fn unavailable_port(port: u16) -> String {
    if port == 0 {
        "no free callback port on 127.0.0.1 is available".to_owned()
    } else {
        format!(
            "the callback port {port} on 127.0.0.1 is already in use; free it or set another oauth.callbackPort"
        )
    }
}

/// A login waiting for the user to approve it in a browser. Dropping it cancels the login and
/// closes the callback listener without changing any credential.
pub struct PendingLogin {
    connector: Connector,
    session: AuthorizationSession,
    callback: PendingCallback,
    authorization_url: String,
    registration: ClientRegistration,
    resource: Option<String>,
    endpoints: TokenEndpoints,
}

/// Where the token is renewed and revoked later.
struct TokenEndpoints {
    issuer: String,
    token: String,
    revocation: Option<String>,
}

impl PendingLogin {
    /// The address to open in a browser.
    pub fn authorization_url(&self) -> &str {
        &self.authorization_url
    }

    /// The address the browser returns to, which the user pastes back when the browser runs on
    /// another machine.
    pub fn redirect_uri(&self) -> &str {
        self.callback.redirect_uri()
    }

    pub fn paste(&self) -> CallbackPaste {
        self.callback.paste()
    }

    /// Waits for the callback or the pasted address until the login timeout, counted from when
    /// the authorization URL was ready, then exchanges the code and saves the token.
    pub async fn finish(mut self, cancellation: &CancellationToken) -> Result<(), LoginError> {
        let response = self
            .callback
            .wait(cancellation)
            .await
            .map_err(callback_error)?;
        let state = self.callback.state().to_owned();
        let token_response = cancellable(
            cancellation,
            self.session.handle_callback_with_issuer(
                &response.code,
                &state,
                response.issuer.as_deref(),
            ),
        )
        .await?
        .map_err(|_| LoginError::TokenExchange)?;
        let token = self.oauth_token(&token_response)?;
        let credentials = &self.connector.credentials;
        let _lock = cancellable(cancellation, credentials.lock()).await??;
        if cancellation.is_cancelled() {
            return Err(LoginError::Cancelled);
        }
        credentials
            .save(&ConnectorCredential {
                server_url: self.connector.server_url.to_string(),
                registration: self.registration.clone(),
                resource: self.resource.clone(),
                token: Some(token),
            })
            .await?;
        Ok(())
    }

    fn oauth_token(
        &self,
        response: &rmcp::transport::auth::OAuthTokenResponse,
    ) -> Result<OAuthToken, LoginError> {
        #[derive(Deserialize)]
        struct TokenFields {
            access_token: String,
            token_type: String,
            refresh_token: Option<String>,
            scope: Option<String>,
            expires_in: Option<u64>,
        }
        let fields: TokenFields = serde_json::to_value(response)
            .and_then(serde_json::from_value)
            .map_err(|_| LoginError::TokenExchange)?;
        if fields.access_token.is_empty() {
            return Err(LoginError::TokenExchange);
        }
        Ok(OAuthToken {
            access_token: fields.access_token,
            token_type: fields.token_type,
            refresh_token: fields.refresh_token.filter(|token| !token.is_empty()),
            scope: fields.scope,
            issuer: self.endpoints.issuer.clone(),
            client_id: self.registration.client_id.clone(),
            token_endpoint: self.endpoints.token.clone(),
            revocation_endpoint: self.endpoints.revocation.clone(),
            expires_at: now_seconds()
                .saturating_add(fields.expires_in.unwrap_or(DEFAULT_TOKEN_LIFETIME_SECONDS)),
        })
    }
}

impl fmt::Debug for PendingLogin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PendingLogin")
            .field("redirect_uri", &self.callback.redirect_uri())
            .finish_non_exhaustive()
    }
}

impl Connector {
    /// Starts a login. `challenge` is the `WWW-Authenticate` header of the server's 401, which
    /// names its protected resource metadata; without it the well-known addresses are tried.
    pub async fn start_login(
        &self,
        challenge: Option<&str>,
        cancellation: &CancellationToken,
    ) -> Result<PendingLogin, LoginError> {
        let mut manager = cancellable(
            cancellation,
            AuthorizationManager::new_with_oauth_http_client(
                self.server_url.clone(),
                Arc::new(self.http.clone()),
            ),
        )
        .await?
        .map_err(|_| LoginError::Discovery)?;
        let metadata = cancellable(
            cancellation,
            self.authorization_metadata(&manager, challenge),
        )
        .await??;
        let endpoints = TokenEndpoints::from_metadata(&metadata)?;
        manager.set_metadata(metadata);

        let stored = cancellable(cancellation, self.credentials.load()).await??;
        let (listener, preregistered) = self.callback_and_client(stored, &endpoints.issuer).await?;
        let redirect_uri = listener.redirect_uri().to_owned();
        let mut request =
            AuthorizationRequest::new(redirect_uri.clone()).with_client_name(CLIENT_NAME);
        if let Some(client_id) = preregistered {
            request = request.with_preregistered_client(client_id);
        }
        let session = cancellable(cancellation, AuthorizationSession::new(manager, request))
            .await?
            .map_err(|(_, error)| authorization_error(&error))?;
        let (client_id, _) = cancellable(cancellation, session.get_credentials())
            .await?
            .map_err(|_| LoginError::Registration)?;
        let authorization_url = session.get_authorization_url().to_owned();
        let query = |name: &str| {
            Url::parse(&authorization_url).ok().and_then(|url| {
                url.query_pairs()
                    .find(|(field, _)| field == name)
                    .map(|(_, value)| value.into_owned())
            })
        };
        let state = query("state").ok_or(LoginError::Unsupported)?;
        let resource = query("resource");
        Ok(PendingLogin {
            connector: self.clone(),
            session,
            callback: listener.receive(state, CALLBACK_PAGES, LOGIN_TIMEOUT),
            authorization_url,
            registration: ClientRegistration {
                authorization_server: endpoints.issuer.clone(),
                client_id,
                redirect_uri,
            },
            resource,
            endpoints,
        })
    }

    async fn authorization_metadata(
        &self,
        manager: &AuthorizationManager,
        challenge: Option<&str>,
    ) -> Result<AuthorizationMetadata, LoginError> {
        if let Some(metadata_url) = &self.metadata_url {
            let (status, body) = self
                .http
                .get_bounded(metadata_url)
                .await
                .map_err(|_| LoginError::Discovery)?;
            if status != 200 {
                return Err(LoginError::Discovery);
            }
            return serde_json::from_slice(&body).map_err(|_| LoginError::Discovery);
        }
        if let Some(challenge) = challenge {
            self.http
                .validate_challenge(challenge)
                .map_err(|_| LoginError::Discovery)?;
        }
        let resolution = manager
            .resolve_metadata_from_challenge(challenge)
            .await
            .map_err(|_| LoginError::Discovery)?;
        if !resolution.source.is_discovered() {
            return Err(LoginError::Discovery);
        }
        Ok(resolution.metadata)
    }

    /// Binds the callback port and picks the client: the configured one, else the stored
    /// registration with this authorization server when its port is free again, else none so
    /// that Tickloop registers anew.
    async fn callback_and_client(
        &self,
        stored: Option<ConnectorCredential>,
        issuer: &str,
    ) -> Result<(CallbackListener, Option<String>), LoginError> {
        let configured_port = self.settings.callback_port.unwrap_or(0);
        if let Some(client_id) = &self.settings.client_id {
            return Ok((bind(configured_port).await?, Some(client_id.clone())));
        }
        let reusable = stored
            .map(|credential| credential.registration)
            .filter(|registration| registration.authorization_server == issuer)
            .and_then(|registration| {
                let port = Url::parse(&registration.redirect_uri).ok()?.port()?;
                let usable = self
                    .settings
                    .callback_port
                    .is_none_or(|configured| configured == port);
                usable.then_some((port, registration.client_id))
            });
        if let Some((port, client_id)) = reusable
            && let Ok(listener) = bind(port).await
        {
            return Ok((listener, Some(client_id)));
        }
        Ok((bind(configured_port).await?, None))
    }
}

impl TokenEndpoints {
    fn from_metadata(metadata: &AuthorizationMetadata) -> Result<Self, LoginError> {
        let token = Url::parse(&metadata.token_endpoint).map_err(|_| LoginError::Discovery)?;
        let issuer = match &metadata.issuer {
            Some(issuer) => issuer.clone(),
            None => token.origin().ascii_serialization(),
        };
        let revocation = metadata
            .additional_fields
            .get("revocation_endpoint")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        Ok(Self {
            issuer,
            token: token.to_string(),
            revocation,
        })
    }
}

async fn bind(port: u16) -> Result<CallbackListener, LoginError> {
    CallbackListener::bind(port, CALLBACK_PATH)
        .await
        .map_err(callback_error)
}

fn callback_error(error: CallbackError) -> LoginError {
    match error {
        CallbackError::PortUnavailable(port) => LoginError::CallbackPortUnavailable(port),
        CallbackError::Denied => LoginError::Denied,
        CallbackError::AuthorizationFailed
        | CallbackError::InvalidCallback
        | CallbackError::StateMismatch => LoginError::AuthorizationFailed,
        CallbackError::Ended => LoginError::Ended,
        CallbackError::Cancelled => LoginError::Cancelled,
        CallbackError::TimedOut => LoginError::TimedOut,
    }
}

fn authorization_error(error: &AuthError) -> LoginError {
    match error {
        AuthError::RegistrationFailed(_) => LoginError::Registration,
        AuthError::PkceUnsupported | AuthError::InvalidScope(_) => LoginError::Unsupported,
        _ => LoginError::Discovery,
    }
}

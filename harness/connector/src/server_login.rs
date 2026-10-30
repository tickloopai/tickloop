//! Logging in when the host receives the authorization response at its own redirect address, as
//! a web service does: starting returns the authorization URL and a pending login the host keeps,
//! possibly for another process or replica, until the browser comes back; finishing checks the
//! callback against it, exchanges the code and saves the token.

use std::fmt;
use std::time::Duration;

use reqwest::Url;
use serde::{Deserialize, Serialize};
use tickloop_harness_base::oauth_callback::{CallbackError, read_authorization_response};
use tokio_util::sync::CancellationToken;

use crate::credentials::ClientRegistration;
use crate::login::{LoginError, TokenEndpoints, TokenFields, WEB_APPLICATION};
use crate::tokens::now_seconds;
use crate::{Connector, cancellable, http};

const LOGIN_LIFETIME: Duration = Duration::from_secs(10 * 60);
const AUTHORIZATION_CODE_GRANT: &str = "authorization_code";

/// A server-side login waiting for the browser to return to the host's redirect address. It
/// holds the PKCE verifier and `state`, so the host keeps it encrypted, and uses it once.
#[derive(Clone, Serialize, Deserialize)]
pub struct PendingServerLogin {
    server_url: String,
    authorization_url: String,
    state: String,
    pkce_verifier: String,
    registration: ClientRegistration,
    resource: Option<String>,
    endpoints: TokenEndpoints,
    /// RFC 9207: the issuer the response must name when it names one, and whether it must.
    expected_issuer: Option<String>,
    requires_issuer: bool,
    /// Seconds since the Unix epoch.
    expires_at: u64,
}

impl PendingServerLogin {
    /// The address to send the browser to.
    pub fn authorization_url(&self) -> &str {
        &self.authorization_url
    }

    /// The `state` the callback carries, by which the host finds this login again.
    pub fn state(&self) -> &str {
        &self.state
    }

    pub fn redirect_uri(&self) -> &str {
        &self.registration.redirect_uri
    }

    /// When the login expires, in seconds since the Unix epoch.
    pub fn expires_at(&self) -> u64 {
        self.expires_at
    }

    fn check_issuer(&self, received: Option<&str>) -> Result<(), LoginError> {
        let matches = match (self.expected_issuer.as_deref(), received) {
            (Some(expected), Some(received)) => expected == received,
            (None, Some(_)) => false,
            (_, None) => !self.requires_issuer,
        };
        if matches {
            Ok(())
        } else {
            Err(LoginError::IssuerMismatch)
        }
    }
}

/// Only the origin of `address`: its path and query may carry secrets.
fn origin(address: &str) -> String {
    Url::parse(address).map_or_else(
        |_| "<address>".to_owned(),
        |url| url.origin().ascii_serialization(),
    )
}

impl fmt::Debug for PendingServerLogin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PendingServerLogin")
            .field("server", &origin(&self.server_url))
            .field("redirect", &origin(&self.registration.redirect_uri))
            .field(
                "authorization_server",
                &origin(&self.registration.authorization_server),
            )
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

impl Connector {
    /// Starts a login whose authorization response the host receives at `redirect_uri`, an
    /// https address (http only on this machine). `challenge` is as for [`Self::start_login`].
    /// Tickloop registers with `redirect_uri` unless a client id is configured; a stored
    /// registration is reused only for the same authorization server and redirect address.
    /// The login expires after ten minutes.
    pub async fn start_server_login(
        &self,
        redirect_uri: &str,
        challenge: Option<&str>,
        cancellation: &CancellationToken,
    ) -> Result<PendingServerLogin, LoginError> {
        let redirect_uri = Url::parse(redirect_uri)
            .ok()
            .filter(|url| http::is_allowed_address(url) && url.fragment().is_none())
            .ok_or(LoginError::RedirectAddress)?
            .to_string();
        let discovery = self.discover(challenge, cancellation).await?;
        let stored = cancellable(cancellation, self.credentials.load()).await??;
        let issuer = &discovery.endpoints.issuer;
        let preregistered = self.settings.client_id.clone().or_else(|| {
            stored
                .map(|credential| credential.registration)
                .filter(|registration| {
                    registration.authorization_server == *issuer
                        && registration.redirect_uri == redirect_uri
                })
                .map(|registration| registration.client_id)
        });
        let authorization = self
            .authorize(
                discovery,
                redirect_uri,
                preregistered,
                WEB_APPLICATION,
                cancellation,
            )
            .await?;
        let recorded = authorization.recorded;
        Ok(PendingServerLogin {
            server_url: self.server_url.to_string(),
            authorization_url: authorization.url,
            state: authorization.state,
            pkce_verifier: recorded.pkce_verifier,
            registration: authorization.registration,
            resource: authorization.resource,
            endpoints: authorization.endpoints,
            expected_issuer: recorded.expected_issuer,
            requires_issuer: recorded.require_issuer,
            expires_at: now_seconds().saturating_add(LOGIN_LIFETIME.as_secs()),
        })
    }

    /// Finishes `pending`, possibly started by another process, with `callback_query`, the query
    /// of the browser's request to the redirect address. The credential changes only when the
    /// callback carries the login's `state` before it expires, the authorization server granted a
    /// code, and the code is exchanged for a token.
    pub async fn finish_server_login(
        &self,
        pending: PendingServerLogin,
        callback_query: &str,
        cancellation: &CancellationToken,
    ) -> Result<(), LoginError> {
        if pending.server_url != self.server_url.as_str() {
            return Err(LoginError::StateMismatch);
        }
        let mut callback =
            Url::parse(pending.redirect_uri()).map_err(|_| LoginError::InvalidCallback)?;
        callback.set_query(Some(
            callback_query.strip_prefix('?').unwrap_or(callback_query),
        ));
        let response =
            read_authorization_response(callback.as_str(), pending.redirect_uri(), &pending.state);
        if response == Err(CallbackError::StateMismatch) {
            return Err(LoginError::StateMismatch);
        }
        if now_seconds() >= pending.expires_at {
            return Err(LoginError::Expired);
        }
        let response = response.map_err(|error| match error {
            CallbackError::Denied => LoginError::Denied,
            CallbackError::AuthorizationFailed => LoginError::AuthorizationFailed,
            _ => LoginError::InvalidCallback,
        })?;
        pending.check_issuer(response.issuer.as_deref())?;
        let token = self
            .exchange_code(&pending, &response.code, cancellation)
            .await?;
        self.save_login(pending.registration, pending.resource, token, cancellation)
            .await
    }

    async fn exchange_code(
        &self,
        pending: &PendingServerLogin,
        code: &str,
        cancellation: &CancellationToken,
    ) -> Result<tick_oauth::OAuthToken, LoginError> {
        let endpoint =
            Url::parse(&pending.endpoints.token).map_err(|_| LoginError::TokenExchange)?;
        let registration = &pending.registration;
        let mut form = vec![
            ("grant_type", AUTHORIZATION_CODE_GRANT),
            ("code", code),
            ("redirect_uri", &registration.redirect_uri),
            ("client_id", &registration.client_id),
            ("code_verifier", &pending.pkce_verifier),
        ];
        if let Some(resource) = &pending.resource {
            form.push(("resource", resource));
        }
        let (status, body) =
            cancellable(cancellation, self.http.post_form_bounded(&endpoint, &form))
                .await?
                .map_err(|_| LoginError::TokenExchange)?;
        if !(200..300).contains(&status) {
            return Err(LoginError::TokenExchange);
        }
        serde_json::from_slice::<TokenFields>(&body)
            .map_err(|_| LoginError::TokenExchange)?
            .into_token(registration, &pending.endpoints)
    }
}

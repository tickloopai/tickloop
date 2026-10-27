//! Logging in to a remote MCP Server: find its authorization server, register Tickloop or use
//! the configured client, send the user to the authorization URL, and exchange the code that
//! comes back through the loopback callback or a pasted redirect address.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use reqwest::Url;
use rmcp::transport::auth::{AuthorizationMetadata, WWWAuthenticateParams};
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

/// Why a login ended without saving a token. The texts never include tokens; only a discovery
/// failure carries a cause, as one line in which every address is reduced to its origin.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
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
    #[error("the server publishes no usable OAuth authorization server metadata: {0}")]
    Discovery(String),
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
        let redaction = self.cause_redaction(challenge);
        let mut manager = cancellable(
            cancellation,
            AuthorizationManager::new_with_oauth_http_client(
                self.server_url.clone(),
                Arc::new(self.http.clone()),
            ),
        )
        .await?
        .map_err(|error| redaction.discovery_failure(&error))?;
        let metadata = cancellable(
            cancellation,
            self.authorization_metadata(&manager, challenge, &redaction),
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
            .map_err(|(_, error)| authorization_error(&error, &redaction))?;
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
        redaction: &CauseRedaction,
    ) -> Result<AuthorizationMetadata, LoginError> {
        if let Some(metadata_url) = &self.metadata_url {
            let (status, body) = self
                .http
                .get_bounded(metadata_url)
                .await
                .map_err(|error| redaction.error_cause(&*error))?;
            if status != 200 {
                return Err(LoginError::Discovery(format!(
                    "the configured metadata address answered HTTP {status}"
                )));
            }
            return serde_json::from_slice(&body).map_err(|_| {
                LoginError::Discovery(
                    "the configured metadata address did not return authorization server metadata"
                        .to_owned(),
                )
            });
        }
        if let Some(challenge) = challenge {
            self.http
                .validate_challenge(challenge)
                .map_err(|error| redaction.error_cause(&*error))?;
        }
        let resolution = manager
            .resolve_metadata_from_challenge(challenge)
            .await
            .map_err(|error| redaction.discovery_failure(&error))?;
        if !resolution.source.is_discovered() {
            return Err(LoginError::Discovery(
                "the server publishes neither protected resource metadata nor authorization server metadata"
                    .to_owned(),
            ));
        }
        Ok(resolution.metadata)
    }

    /// The addresses a discovery failure can quote that this login already knows.
    fn cause_redaction(&self, challenge: Option<&str>) -> CauseRedaction {
        let challenged = challenge.and_then(|challenge| {
            WWWAuthenticateParams::parse(challenge, &self.server_url).resource_metadata_url
        });
        CauseRedaction::new(
            [
                Some(self.server_url.clone()),
                self.metadata_url.clone(),
                challenged,
            ]
            .into_iter()
            .flatten(),
        )
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
        let token = Url::parse(&metadata.token_endpoint).map_err(|_| {
            LoginError::Discovery("the token endpoint is not a valid address".to_owned())
        })?;
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

fn authorization_error(error: &AuthError, redaction: &CauseRedaction) -> LoginError {
    match error {
        AuthError::RegistrationFailed(_) => LoginError::Registration,
        AuthError::PkceUnsupported | AuthError::InvalidScope(_) => LoginError::Unsupported,
        _ => redaction.discovery_failure(error),
    }
}

const MAX_CAUSE_CHARS: usize = 400;
const CAUSED_BY: &str = "Caused by:";
const UNREADABLE_ADDRESS: &str = "<address>";
const TRAILING_PUNCTUATION: &[char] = &[')', ']', '>', '\'', '"', '`', ',', ';', ':', '.'];

/// Turns error texts into the one-line cause of a discovery failure. Each address keeps only
/// its origin, since a server address may carry a credential in its user information, path or
/// query: every word that contains `://` is replaced whole, by the origin of the known address
/// it consists of, else by the origin parsed from it, so no other character of that word can
/// survive.
struct CauseRedaction {
    /// Each address this login knows, with its origin.
    known: Vec<(String, String)>,
}

impl CauseRedaction {
    fn new(addresses: impl IntoIterator<Item = Url>) -> Self {
        let known = addresses
            .into_iter()
            .map(|address| (address.to_string(), origin_of(&address)))
            .collect();
        Self { known }
    }

    fn discovery_failure(&self, error: &AuthError) -> LoginError {
        match error {
            AuthError::MetadataError(message) => LoginError::Discovery(self.line(message)),
            _ => self.error_cause(error),
        }
    }

    /// `error` and its sources, skipping a source its parent already quotes.
    fn error_cause(&self, error: &(dyn std::error::Error + 'static)) -> LoginError {
        let mut chain = error.to_string();
        for source in std::iter::successors(error.source(), |source| source.source()) {
            let message = source.to_string();
            if !chain.contains(&message) {
                chain.push('\n');
                chain.push_str(&message);
            }
        }
        LoginError::Discovery(self.line(&chain))
    }

    /// The lines of `text` joined into one, without control characters.
    fn line(&self, text: &str) -> String {
        let line: String = text
            .lines()
            .map(|line| line.trim().strip_prefix(CAUSED_BY).unwrap_or(line).trim())
            .filter(|line| !line.is_empty())
            .map(|line| self.redact(line))
            .collect::<Vec<_>>()
            .join(": ")
            .chars()
            .filter(|character| !character.is_control())
            .collect();
        if line.chars().count() <= MAX_CAUSE_CHARS {
            return line;
        }
        let mut truncated: String = line.chars().take(MAX_CAUSE_CHARS).collect();
        truncated.push('…');
        truncated
    }

    fn redact(&self, line: &str) -> String {
        line.split_whitespace()
            .map(|word| match word.find("://") {
                Some(separator) => self.address_origin(word, separator),
                None => word.to_owned(),
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// The origin that replaces `word`, whose address, possibly wrapped in punctuation, has its
    /// `://` at `separator`.
    fn address_origin(&self, word: &str, separator: usize) -> String {
        let start = word[..separator]
            .char_indices()
            .rev()
            .take_while(|(_, character)| {
                character.is_ascii_alphanumeric() || matches!(character, '+' | '-' | '.')
            })
            .last()
            .map_or(separator, |(index, _)| index);
        let address = &word[start..];
        let unwrapped = address.trim_end_matches(TRAILING_PUNCTUATION);
        if let Some((_, origin)) = self
            .known
            .iter()
            .find(|(known, _)| known == address || known == unwrapped)
        {
            return origin.clone();
        }
        Url::parse(address).map_or_else(|_| UNREADABLE_ADDRESS.to_owned(), |url| origin_of(&url))
    }
}

/// `url`'s origin, when its host is an ordinary name or IP address.
fn origin_of(url: &Url) -> String {
    let ordinary_host = url.host_str().is_some_and(|host| {
        host.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '.' | ':' | '[' | ']')
        })
    });
    let origin = url.origin();
    if ordinary_host && origin.is_tuple() {
        origin.ascii_serialization()
    } else {
        UNREADABLE_ADDRESS.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn redaction() -> CauseRedaction {
        CauseRedaction::new([
            Url::parse("https://mcp.example.test/key-1/mcp?token=secret-1").unwrap(),
            Url::parse("https://mcp.example.test/key-1/mcp").unwrap(),
        ])
    }

    #[test]
    fn a_discovery_cause_is_one_line_with_only_the_origins_of_addresses() {
        let timed_out = "OAuth metadata discovery failed for https://mcp.longbridge.cn/\n  \
            Caused by: error sending request for url (https://mcp.longbridge.cn/)\n  \
            Caused by: client error (Connect)\n  Caused by: operation timed out";
        assert_eq!(
            redaction().line(timed_out),
            "OAuth metadata discovery failed for https://mcp.longbridge.cn: error sending \
             request for url https://mcp.longbridge.cn: client error (Connect): operation \
             timed out"
        );
        assert_eq!(
            redaction().line("expected https://a.test/x and a:// and “https://b.test/y\u{7}"),
            "expected https://a.test and <address> and https://b.test"
        );
        let long = redaction().line(&"word ".repeat(MAX_CAUSE_CHARS));
        assert_eq!(long.chars().count(), MAX_CAUSE_CHARS + 1);
        assert!(long.ends_with('…'));
    }

    #[test]
    fn no_part_of_an_address_after_its_host_survives_whatever_it_contains() {
        let redaction = redaction();
        for delimiter in ['"', ')', ',', ';', '>', '\'', '(', '<', '`', ']'] {
            let text = format!(
                "reference 'https://user:pw-secret@mcp.example.test:8443/path-secret?a=b{delimiter}token=query-secret#fragment-secret', \
                 for (https://mcp.example.test/mcp?x=1{delimiter}code=code-secret{delimiter}) and \
                 https://mcp.example.test/key-1/mcp?token=secret-1{delimiter}more-secret"
            );
            let line = redaction.line(&text);
            assert!(!line.contains("secret"), "{delimiter}: {line}");
            assert!(!line.contains("token"), "{delimiter}: {line}");
            assert!(!line.contains('?'), "{delimiter}: {line}");
            assert!(line.contains("https://mcp.example.test:8443"), "{line}");
        }
        assert_eq!(
            redaction.line("at https://mcp.example.test/key-1/mcp?token=secret-1 and https://mcp.example.test/key-1/mcp/.well-known/x"),
            "at https://mcp.example.test and https://mcp.example.test"
        );
        assert_eq!(
            redaction.line("(https://mcp.example.test/key-1/mcp): failed"),
            "https://mcp.example.test failed"
        );
        assert_eq!(
            redaction.line("issuer https://a.test,https://b.test/?k=secret"),
            "issuer <address>"
        );
    }

    #[test]
    fn a_known_address_inside_a_longer_one_leaves_nothing_of_the_longer_one() {
        let redaction = CauseRedaction::new([
            Url::parse("https://mcp.example.com/").unwrap(),
            Url::parse("https://mcp.example.com/key-1").unwrap(),
        ]);
        for text in [
            "at https://mcp.example.com/oauth2/token?code=SECRET#SECRET",
            "at https://mcp.example.com/key-1/oauth2/token?code=SECRET",
            "at prefix-SECRET(https://mcp.example.com/)SECRET?code=SECRET",
            "at (https://mcp.example.com/key-1)/SECRET",
            "at https://mcp.example.com/\"SECRET",
            "at https://mcp.example.com/,SECRET;SECRET>SECRET)SECRET",
        ] {
            let line = redaction.line(text);
            assert_eq!(line, "at https://mcp.example.com", "{text}");
        }
    }
}

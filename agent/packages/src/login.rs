//! Logging a package in to and out of its data source as its manifest declares. Tickloop is the
//! OAuth 2.0 client: it reads the server's metadata (RFC 8414), registers itself once per issuer
//! (RFC 7591), waits for the user to approve a device code (RFC 8628) and saves the token
//! (RFC 6749); logging out revokes it (RFC 7009). The package's programs only use what is saved
//! and refresh the token themselves. Errors name servers and steps, never codes or tokens.

use std::{fmt, time::Duration};

use reqwest::{
    Client, RequestBuilder, Response, StatusCode, Url,
    header::{ACCEPT, CONTENT_TYPE, USER_AGENT},
    redirect::Policy,
};
use serde::{Deserialize, de::DeserializeOwned};
use tick_packages::{LockedOAuth, OAuthIssuer, OAuthToken, PackageAuth, PackageCredentials};
use tickloop_agent_base::network::{HttpPolicy, build_http_client, is_loopback};
use tokio::time::{Instant, sleep_until};
use tokio_util::sync::CancellationToken;

const DEVICE_CODE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
const REFRESH_TOKEN_GRANT: &str = "refresh_token";
const METADATA_PATH: &str = "/.well-known/oauth-authorization-server";
const CLIENT_NAME: &str = "Tickloop";
/// Tickloop never takes a redirect, but servers may require a registration to name one (RFC 7591
/// allows it): this is the one the Longbridge device login has always registered.
const REGISTERED_REDIRECT_URI: &str = "http://localhost:60355/callback";
const CLIENT_AGENT: &str = concat!("tickloop/", env!("CARGO_PKG_VERSION"));
const JSON: &str = "application/json";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Far above any metadata, registration or token response.
const RESPONSE_SIZE_LIMIT: usize = 64 * 1024;
/// RFC 8628: the interval when the server names none, and how much slower each `slow_down` makes
/// the polling.
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(5);
const SLOW_DOWN_STEP: Duration = Duration::from_secs(5);
/// Polling backs off up to this after the server cannot be reached.
const MAX_RETRY_INTERVAL: Duration = Duration::from_secs(60);
/// A device code lives at most this long, whatever the server says.
const MAX_DEVICE_CODE_LIFETIME: Duration = Duration::from_secs(3600);
/// RFC 6749 only recommends `expires_in`; a token without it counts as expiring after an hour.
const DEFAULT_TOKEN_LIFETIME_SECONDS: u64 = 3600;
/// Longer user codes and links than this are not shown to the user.
const MAX_SHOWN_LENGTH: usize = 2048;

/// Why a login ended without saving a token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoginError {
    Cancelled,
    /// The user refused the authorization.
    Denied,
    /// The device code expired before the user approved it.
    Expired,
    Failed(String),
}

impl fmt::Display for LoginError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("the login was cancelled"),
            Self::Denied => formatter.write_str("the authorization was denied"),
            Self::Expired => {
                formatter.write_str("the code expired before the authorization was approved")
            }
            Self::Failed(reason) => formatter.write_str(reason),
        }
    }
}

impl From<tick_packages::PackageError> for LoginError {
    fn from(error: tick_packages::PackageError) -> Self {
        Self::Failed(error.to_string())
    }
}

/// A device authorization waiting for the user: they open [`Self::link`] and confirm
/// [`Self::user_code`]; [`Self::finish`] waits for the approval and saves the token.
pub struct OAuthLogin {
    server: AuthorizationServer,
    credentials: PackageCredentials,
    endpoints: Endpoints,
    client_id: String,
    device_code: String,
    user_code: String,
    verification_uri: Url,
    verification_uri_complete: Option<Url>,
    interval: Duration,
    expires_in: Duration,
    deadline: Instant,
}

impl fmt::Debug for OAuthLogin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OAuthLogin")
            .field("issuer", &self.server.issuer)
            .field("verification_uri", &self.verification_uri.as_str())
            .finish_non_exhaustive()
    }
}

impl OAuthLogin {
    /// Discovers `issuer`'s endpoints, registers Tickloop there unless it did before, and asks
    /// for a device code with `scopes` (none asks for the server's default).
    pub async fn start(
        credentials: &PackageCredentials,
        issuer: &OAuthIssuer,
        scopes: &[String],
    ) -> Result<Self, LoginError> {
        let server = AuthorizationServer::new(&issuer.issuer)?;
        let endpoints = server.discover().await?;
        let issuer = issuer.issuer.clone();
        let saved_client = locked(credentials, {
            let issuer = issuer.clone();
            move |files| files.client_id(&issuer)
        })
        .await?;
        let is_saved_client = saved_client.is_some();
        let mut client_id = match saved_client {
            Some(client_id) => client_id,
            None => register(&server, &endpoints, credentials, &issuer).await?,
        };
        let mut authorization = server
            .authorize_device(&endpoints, &client_id, scopes)
            .await?;
        if is_saved_client && matches!(authorization, Authorization::UnknownClient) {
            // The server forgot the client registered before: register anew, once.
            client_id = register(&server, &endpoints, credentials, &issuer).await?;
            authorization = server
                .authorize_device(&endpoints, &client_id, scopes)
                .await?;
        }
        let device = authorization.granted()?;
        let verification_uri = server.shown_url(&device.verification_uri, "verification_uri")?;
        let verification_uri_complete = device
            .verification_uri_complete
            .as_deref()
            .filter(|uri| !uri.is_empty())
            .map(|uri| server.shown_url(uri, "verification_uri_complete"))
            .transpose()?;
        let is_shown = |text: &str| {
            !text.is_empty()
                && text.len() <= MAX_SHOWN_LENGTH
                && !text.chars().any(char::is_control)
        };
        if device.device_code.is_empty() || !is_shown(&device.user_code) || device.expires_in == 0 {
            return Err(server.invalid("device authorization"));
        }
        let expires_in = Duration::from_secs(device.expires_in).min(MAX_DEVICE_CODE_LIFETIME);
        Ok(Self {
            server,
            credentials: credentials.clone(),
            endpoints,
            client_id,
            device_code: device.device_code,
            user_code: device.user_code,
            verification_uri,
            verification_uri_complete,
            interval: device.interval.map_or(DEFAULT_POLL_INTERVAL, |seconds| {
                Duration::from_secs(seconds.max(1))
            }),
            expires_in,
            deadline: Instant::now() + expires_in,
        })
    }

    /// The code the user confirms on the verification page.
    pub fn user_code(&self) -> &str {
        &self.user_code
    }

    /// The page where the user enters the code.
    pub fn verification_uri(&self) -> &str {
        self.verification_uri.as_str()
    }

    /// The page to open: the one with the code filled in when the server offers it.
    pub fn link(&self) -> &str {
        self.verification_uri_complete
            .as_ref()
            .unwrap_or(&self.verification_uri)
            .as_str()
    }

    /// How long the code stays valid from when the login started.
    pub fn expires_in(&self) -> Duration {
        self.expires_in
    }

    /// Polls until the user approves, refuses, the code expires or `cancellation` fires, and
    /// saves the token on approval. The server cannot be reached for a while without failing the
    /// login: polling backs off until the code expires.
    pub async fn finish(self, cancellation: &CancellationToken) -> Result<(), LoginError> {
        let mut interval = self.interval;
        let mut unreachable: Option<String> = None;
        loop {
            let next_poll = Instant::now() + interval;
            if next_poll >= self.deadline {
                return Err(match unreachable {
                    Some(reason) => LoginError::Failed(format!(
                        "the code expired while the authorization server could not be reached: {reason}"
                    )),
                    None => LoginError::Expired,
                });
            }
            tokio::select! {
                () = cancellation.cancelled() => return Err(LoginError::Cancelled),
                () = sleep_until(next_poll) => {}
            }
            let request = self.server.http.post(self.endpoints.token.clone()).form(&[
                ("grant_type", DEVICE_CODE_GRANT),
                ("device_code", &self.device_code),
                ("client_id", &self.client_id),
            ]);
            let reply = tokio::select! {
                () = cancellation.cancelled() => return Err(LoginError::Cancelled),
                reply = self.server.exchange(request, &self.endpoints.token) => reply,
            };
            let (status, body) = match reply {
                Ok((status, _)) if status.is_server_error() => {
                    unreachable = Some(format!("{} answered HTTP {status}", self.endpoints.token));
                    interval = (interval * 2).min(MAX_RETRY_INTERVAL);
                    continue;
                }
                Ok(reply) => reply,
                Err(reason) => {
                    unreachable = Some(reason);
                    interval = (interval * 2).min(MAX_RETRY_INTERVAL);
                    continue;
                }
            };
            unreachable = None;
            if status.is_success() {
                let token = self.token_from(&body)?;
                if cancellation.is_cancelled() {
                    return Err(LoginError::Cancelled);
                }
                return locked(&self.credentials, move |files| files.save_token(&token)).await;
            }
            match error_code(&body).as_deref() {
                Some("authorization_pending") => {}
                Some("slow_down") => interval += SLOW_DOWN_STEP,
                Some("access_denied") => return Err(LoginError::Denied),
                Some("expired_token") => return Err(LoginError::Expired),
                Some(code) => {
                    return Err(LoginError::Failed(format!(
                        "{} refused the device code: {code}",
                        self.endpoints.token
                    )));
                }
                None => {
                    return Err(LoginError::Failed(format!(
                        "{} answered HTTP {status}",
                        self.endpoints.token
                    )));
                }
            }
        }
    }

    fn token_from(&self, body: &[u8]) -> Result<OAuthToken, LoginError> {
        let response: TokenResponse = parse(body).ok_or_else(|| self.server.invalid("token"))?;
        if response.access_token.is_empty() || response.token_type.is_empty() {
            return Err(self.server.invalid("token"));
        }
        let lifetime = response
            .expires_in
            .unwrap_or(DEFAULT_TOKEN_LIFETIME_SECONDS);
        Ok(OAuthToken {
            access_token: response.access_token,
            token_type: response.token_type,
            refresh_token: response.refresh_token.filter(|token| !token.is_empty()),
            scope: response.scope,
            issuer: self.server.issuer.clone(),
            client_id: self.client_id.clone(),
            token_endpoint: self.endpoints.token.to_string(),
            revocation_endpoint: self.endpoints.revocation.as_ref().map(ToString::to_string),
            expires_at: now_seconds().saturating_add(lifetime),
        })
    }
}

/// What logging out did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoggedOut {
    /// Whether there were saved credentials to delete.
    pub had_credentials: bool,
    /// Why the authorization server did not revoke the token; it is deleted all the same.
    pub revocation_failure: Option<String>,
}

/// Deletes the package's saved credentials. An OAuth token is first revoked where its server
/// offers revocation, under the lock, so a program cannot replace it meanwhile.
pub async fn log_out(
    credentials: &PackageCredentials,
    auth: &PackageAuth,
) -> Result<LoggedOut, String> {
    match auth {
        PackageAuth::None => Err("the package needs no login".to_owned()),
        PackageAuth::ApiKey { .. } => {
            let had_credentials = credentials
                .remove_api_key()
                .map_err(|error| error.to_string())?;
            Ok(LoggedOut {
                had_credentials,
                revocation_failure: None,
            })
        }
        PackageAuth::OAuth2 { .. } => {
            if !credentials.oauth_token_path().exists() {
                return Ok(LoggedOut {
                    had_credentials: false,
                    revocation_failure: None,
                });
            }
            let files = {
                let credentials = credentials.clone();
                tokio::task::spawn_blocking(move || credentials.lock_oauth())
                    .await
                    .map_err(|error| error.to_string())?
                    .map_err(|error| error.to_string())?
            };
            // A token that cannot be read is deleted without revoking it.
            let revocation_failure = match files.token() {
                Ok(Some(token)) => revoke(&token).await.err(),
                Ok(None) | Err(_) => None,
            };
            let had_credentials = files.remove_token().map_err(|error| error.to_string())?;
            Ok(LoggedOut {
                had_credentials,
                revocation_failure,
            })
        }
    }
}

/// Revokes the refresh token, or the access token when there is none, as RFC 7009 says; a token
/// whose server offers no revocation needs none.
async fn revoke(token: &OAuthToken) -> Result<(), String> {
    let Some(endpoint) = &token.revocation_endpoint else {
        return Ok(());
    };
    let server = AuthorizationServer::new(&token.issuer).map_err(|error| error.to_string())?;
    let endpoint = server
        .endpoint_url(endpoint, "revocation_endpoint")
        .map_err(|error| error.to_string())?;
    let (secret, hint) = match token.refresh_token() {
        Some(refresh_token) => (refresh_token, REFRESH_TOKEN_GRANT),
        None => (token.access_token.as_str(), "access_token"),
    };
    let request = server.http.post(endpoint.clone()).form(&[
        ("token", secret),
        ("token_type_hint", hint),
        ("client_id", &token.client_id),
    ]);
    let (status, _) = server.exchange(request, &endpoint).await?;
    if status.is_success() {
        Ok(())
    } else {
        Err(format!("{endpoint} answered HTTP {status}"))
    }
}

/// Registers Tickloop with the server as a public device client and keeps its `client_id` for
/// the next login.
async fn register(
    server: &AuthorizationServer,
    endpoints: &Endpoints,
    credentials: &PackageCredentials,
    issuer: &str,
) -> Result<String, LoginError> {
    let endpoint = endpoints.registration.as_ref().ok_or_else(|| {
        LoginError::Failed(format!(
            "{} offers no client registration (registration_endpoint)",
            server.issuer
        ))
    })?;
    let metadata = serde_json::json!({
        "client_name": CLIENT_NAME,
        "grant_types": [DEVICE_CODE_GRANT, REFRESH_TOKEN_GRANT],
        "token_endpoint_auth_method": "none",
        "redirect_uris": [REGISTERED_REDIRECT_URI],
    });
    let request = server
        .http
        .post(endpoint.clone())
        .header(CONTENT_TYPE, JSON)
        .body(metadata.to_string());
    let (status, body) = server
        .exchange(request, endpoint)
        .await
        .map_err(LoginError::Failed)?;
    if !status.is_success() {
        return Err(LoginError::Failed(match error_code(&body) {
            Some(code) => format!("{endpoint} refused to register Tickloop: {code}"),
            None => format!("{endpoint} answered HTTP {status}"),
        }));
    }
    let registration: Registration =
        parse(&body).ok_or_else(|| server.invalid("client registration"))?;
    let is_public = registration
        .token_endpoint_auth_method
        .as_deref()
        .is_none_or(|method| method == "none");
    if registration.client_id.is_empty() || !is_public {
        return Err(server.invalid("client registration"));
    }
    let client_id = registration.client_id;
    locked(credentials, {
        let (issuer, client_id) = (issuer.to_owned(), client_id.clone());
        move |files| files.set_client_id(&issuer, Some(&client_id))
    })
    .await?;
    Ok(client_id)
}

/// Runs `change` on the package's OAuth files under their lock, off the async threads, since
/// the lock waits while a program refreshes the token.
async fn locked<T: Send + 'static>(
    credentials: &PackageCredentials,
    change: impl FnOnce(&LockedOAuth) -> Result<T, tick_packages::PackageError> + Send + 'static,
) -> Result<T, LoginError> {
    let credentials = credentials.clone();
    tokio::task::spawn_blocking(move || change(&credentials.lock_oauth()?))
        .await
        .map_err(|error| LoginError::Failed(error.to_string()))?
        .map_err(LoginError::from)
}

/// The endpoints an issuer's metadata names, each checked like the issuer.
struct Endpoints {
    token: Url,
    device_authorization: Url,
    registration: Option<Url>,
    revocation: Option<Url>,
}

#[derive(Deserialize)]
struct ServerMetadata {
    issuer: String,
    token_endpoint: Option<String>,
    device_authorization_endpoint: Option<String>,
    registration_endpoint: Option<String>,
    revocation_endpoint: Option<String>,
    grant_types_supported: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct Registration {
    client_id: String,
    token_endpoint_auth_method: Option<String>,
}

#[derive(Deserialize)]
struct DeviceAuthorization {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: Option<String>,
    expires_in: u64,
    interval: Option<u64>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    token_type: String,
    expires_in: Option<u64>,
    refresh_token: Option<String>,
    scope: Option<String>,
}

#[derive(Deserialize)]
struct ErrorResponse {
    error: String,
}

enum Authorization {
    Granted(DeviceAuthorization),
    /// The server does not know the client: `invalid_client`.
    UnknownClient,
}

impl Authorization {
    fn granted(self) -> Result<DeviceAuthorization, LoginError> {
        match self {
            Self::Granted(device) => Ok(device),
            Self::UnknownClient => Err(LoginError::Failed(
                "the authorization server does not know the client Tickloop registered \
                 (invalid_client)"
                    .to_owned(),
            )),
        }
    }
}

/// An authorization server, reached through the Agent's HTTP policy without following redirects.
/// Its addresses must use https, or http when the issuer itself is on this machine.
struct AuthorizationServer {
    http: Client,
    issuer: String,
    is_on_this_machine: bool,
}

impl AuthorizationServer {
    fn new(issuer: &str) -> Result<Self, LoginError> {
        let url = Url::parse(issuer)
            .map_err(|_| LoginError::Failed(format!("{issuer:?} is not a valid issuer URL")))?;
        let is_on_this_machine = is_loopback(&url);
        let mut policy = HttpPolicy::new(Policy::none());
        policy.connect_timeout = Some(CONNECT_TIMEOUT);
        policy.timeout = Some(REQUEST_TIMEOUT);
        let (http, _) = build_http_client(&url, policy).map_err(|error| {
            LoginError::Failed(format!("cannot set up the HTTP client: {error}"))
        })?;
        let server = Self {
            http,
            issuer: issuer.to_owned(),
            is_on_this_machine,
        };
        server.endpoint_url(issuer, "issuer")?;
        Ok(server)
    }

    /// Reads the metadata (RFC 8414): it must name this issuer, a token and a device
    /// authorization endpoint, and the device code grant.
    async fn discover(&self) -> Result<Endpoints, LoginError> {
        let mut url = self.endpoint_url(&self.issuer, "issuer")?;
        let issuer_path = url.path().trim_end_matches('/').to_owned();
        url.set_path(&format!("{METADATA_PATH}{issuer_path}"));
        let (status, body) = self
            .exchange(self.http.get(url.clone()), &url)
            .await
            .map_err(|reason| {
                LoginError::Failed(format!(
                    "cannot read the authorization server metadata: {reason}"
                ))
            })?;
        if !status.is_success() {
            return Err(LoginError::Failed(format!(
                "cannot read the authorization server metadata: {url} answered HTTP {status}"
            )));
        }
        let metadata: ServerMetadata =
            parse(&body).ok_or_else(|| self.invalid("authorization server metadata"))?;
        if metadata.issuer != self.issuer {
            return Err(LoginError::Failed(format!(
                "{url} describes another issuer than {}",
                self.issuer
            )));
        }
        let offers_device_grant = metadata
            .grant_types_supported
            .is_some_and(|grants| grants.iter().any(|grant| grant == DEVICE_CODE_GRANT));
        let (Some(device_authorization), Some(token), true) = (
            metadata.device_authorization_endpoint,
            metadata.token_endpoint,
            offers_device_grant,
        ) else {
            return Err(LoginError::Failed(format!(
                "{} offers no device authorization (RFC 8628), which Tickloop logs in with",
                self.issuer
            )));
        };
        let optional =
            |url: Option<String>, field| url.map(|url| self.endpoint_url(&url, field)).transpose();
        Ok(Endpoints {
            token: self.endpoint_url(&token, "token_endpoint")?,
            device_authorization: self
                .endpoint_url(&device_authorization, "device_authorization_endpoint")?,
            registration: optional(metadata.registration_endpoint, "registration_endpoint")?,
            revocation: optional(metadata.revocation_endpoint, "revocation_endpoint")?,
        })
    }

    async fn authorize_device(
        &self,
        endpoints: &Endpoints,
        client_id: &str,
        scopes: &[String],
    ) -> Result<Authorization, LoginError> {
        let endpoint = &endpoints.device_authorization;
        let mut form = vec![("client_id", client_id.to_owned())];
        if !scopes.is_empty() {
            form.push(("scope", scopes.join(" ")));
        }
        let (status, body) = self
            .exchange(self.http.post(endpoint.clone()).form(&form), endpoint)
            .await
            .map_err(LoginError::Failed)?;
        if status.is_success() {
            return parse(&body)
                .map(Authorization::Granted)
                .ok_or_else(|| self.invalid("device authorization"));
        }
        match error_code(&body) {
            Some(code) if code == "invalid_client" => Ok(Authorization::UnknownClient),
            Some(code) => Err(LoginError::Failed(format!(
                "{endpoint} refused the device authorization: {code}"
            ))),
            None => Err(LoginError::Failed(format!(
                "{endpoint} answered HTTP {status}"
            ))),
        }
    }

    /// Sends `request` to `url` and reads the answer, up to [`RESPONSE_SIZE_LIMIT`].
    async fn exchange(
        &self,
        request: RequestBuilder,
        url: &Url,
    ) -> Result<(StatusCode, Vec<u8>), String> {
        let failed = |error: reqwest::Error| format!("{url}: {}", network_failure(&error));
        let mut response: Response = request
            .header(USER_AGENT, CLIENT_AGENT)
            .header(ACCEPT, JSON)
            .send()
            .await
            .map_err(failed)?;
        let status = response.status();
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(failed)? {
            body.extend_from_slice(&chunk);
            if body.len() > RESPONSE_SIZE_LIMIT {
                return Err(format!(
                    "{url} answered more than {} KiB",
                    RESPONSE_SIZE_LIMIT / 1024
                ));
            }
        }
        Ok((status, body))
    }

    /// `text` as an address of this server: https, or http when the issuer is on this machine
    /// and so is the address.
    fn endpoint_url(&self, text: &str, field: &str) -> Result<Url, LoginError> {
        let url = Url::parse(text)
            .map_err(|_| LoginError::Failed(format!("{field} {text:?} is not a valid URL")))?;
        let is_allowed = url.scheme() == "https"
            || self.is_on_this_machine && url.scheme() == "http" && is_loopback(&url);
        if is_allowed && url.username().is_empty() && url.password().is_none() {
            Ok(url)
        } else {
            Err(LoginError::Failed(format!(
                "{field} {url} is refused: only https is accepted, or http on this machine"
            )))
        }
    }

    /// A link the user is shown and may open: an address of this server that prints on one
    /// line.
    fn shown_url(&self, text: &str, field: &str) -> Result<Url, LoginError> {
        if text.len() > MAX_SHOWN_LENGTH || text.chars().any(char::is_control) {
            return Err(self.invalid("device authorization"));
        }
        self.endpoint_url(text, field)
    }

    fn invalid(&self, response: &str) -> LoginError {
        LoginError::Failed(format!(
            "{} sent an invalid {response} response",
            self.issuer
        ))
    }
}

fn parse<T: DeserializeOwned>(body: &[u8]) -> Option<T> {
    serde_json::from_slice(body).ok()
}

/// The `error` of an OAuth error response, when it is a valid error code (RFC 6749: printable
/// ASCII but '"' and '\').
fn error_code(body: &[u8]) -> Option<String> {
    let ErrorResponse { error } = parse(body)?;
    let is_code = !error.is_empty()
        && error.len() <= 128
        && error
            .bytes()
            .all(|byte| (0x20..=0x7e).contains(&byte) && byte != b'"' && byte != b'\\');
    is_code.then_some(error)
}

/// What went wrong, with the innermost cause, which names the actual failure.
fn network_failure(error: &reqwest::Error) -> String {
    let summary = if error.is_timeout() {
        "timed out"
    } else if error.is_connect() {
        "cannot connect"
    } else {
        "the request failed"
    };
    let mut cause: &dyn std::error::Error = error;
    while let Some(source) = cause.source() {
        cause = source;
    }
    format!("{summary} ({cause})")
}

fn now_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

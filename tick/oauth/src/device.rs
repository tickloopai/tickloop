//! A device login (RFC 8628): the server's metadata, the client registered there, a device code
//! the user approves on another device, and the token saved under the credentials' lock.

use std::{fmt, sync::Arc, time::Duration};

use crate::{CredentialStore, OAuthToken};
use reqwest::{Client, Url, header::CONTENT_TYPE};
use serde::Deserialize;
use tokio::time::{Instant, sleep_until};
use tokio_util::sync::CancellationToken;

use crate::{
    LoginError,
    server::{
        Answer, DEVICE_CODE_GRANT, Endpoints, REFRESH_TOKEN_GRANT, Server, TokenResponse, parse,
    },
};

const CLIENT_NAME: &str = "Tickloop";
/// Tickloop never takes a redirect, but servers may require a registration to name one (RFC 7591
/// allows it): this is the one Tickloop's device logins have always registered.
const REGISTERED_REDIRECT_URI: &str = "http://localhost:60355/callback";
const JSON: &str = "application/json";
/// RFC 8628: the interval when the server names none, and how much slower each `slow_down` makes
/// the polling.
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(5);
const SLOW_DOWN_STEP: Duration = Duration::from_secs(5);
/// Polling backs off up to this while the server cannot answer.
const MAX_RETRY_INTERVAL: Duration = Duration::from_secs(60);
/// A device code lives at most this long, whatever the server says.
const MAX_DEVICE_CODE_LIFETIME: Duration = Duration::from_secs(3600);
/// Longer user codes and links than this are not shown to the user.
const MAX_SHOWN_LENGTH: usize = 2048;

/// How a public client obtains its identity at an issuer.
#[derive(Clone, Copy, Debug)]
pub enum ClientRegistration<'a> {
    Dynamic,
    Preregistered(&'a str),
}

/// What a login tells its caller while it waits for the user.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Progress {
    /// The server stopped answering; polling backs off until it answers again. Said once each
    /// time it stops.
    Unreachable,
    /// The server asked for slower polling.
    SlowerPolling,
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

/// A device authorization waiting for the user: they open [`Self::link`] and confirm
/// [`Self::user_code`]; [`Self::finish`] waits for the approval and saves the token.
pub struct DeviceLogin {
    server: Server,
    credentials: Arc<dyn CredentialStore>,
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

impl fmt::Debug for DeviceLogin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DeviceLogin")
            .field("issuer", &self.server.issuer)
            .field("verification_uri", &self.verification_uri.as_str())
            .finish_non_exhaustive()
    }
}

impl DeviceLogin {
    /// Discovers `issuer`'s endpoints through `http`, registers there unless a login of the
    /// package did before, and asks for a device code with `scopes` (none asks for the server's
    /// default).
    pub async fn start(
        http: Client,
        credentials: &(impl CredentialStore + Clone + 'static),
        issuer: &str,
        scopes: &[String],
    ) -> Result<Self, LoginError> {
        Self::start_with(
            http,
            credentials,
            issuer,
            scopes,
            ClientRegistration::Dynamic,
            &[],
        )
        .await
    }

    /// Uses an explicit public client and optional RFC 8707 resources. A preregistered client
    /// never falls back to registration, including when the server refuses its identity.
    pub async fn start_with(
        http: Client,
        credentials: &(impl CredentialStore + Clone + 'static),
        issuer: &str,
        scopes: &[String],
        registration: ClientRegistration<'_>,
        resources: &[String],
    ) -> Result<Self, LoginError> {
        let server = Server::new(http, issuer)?;
        let endpoints = server.discover().await?;
        let saved_client = match registration {
            ClientRegistration::Dynamic => credentials.client_id(issuer).await?,
            ClientRegistration::Preregistered(client_id) if !client_id.is_empty() => {
                Some(client_id.to_owned())
            }
            ClientRegistration::Preregistered(_) => {
                return Err(LoginError::Failed(
                    "the preregistered client ID is empty".into(),
                ));
            }
        };
        let can_register_again =
            saved_client.is_some() && matches!(registration, ClientRegistration::Dynamic);
        let mut client_id = match saved_client {
            Some(client_id) => client_id,
            None => register(&server, &endpoints, credentials).await?,
        };
        let mut device =
            authorize_device(&server, &endpoints, &client_id, scopes, resources).await?;
        if can_register_again && device.is_none() {
            client_id = register(&server, &endpoints, credentials).await?;
            device = authorize_device(&server, &endpoints, &client_id, scopes, resources).await?;
        }
        let device = device.ok_or_else(|| LoginError::Refused {
            code: "invalid_client".to_owned(),
            message: format!(
                "{} refused the public client (invalid_client)",
                server.issuer
            ),
        })?;
        let is_shown = |text: &str| {
            !text.is_empty()
                && text.len() <= MAX_SHOWN_LENGTH
                && !text.chars().any(char::is_control)
        };
        let shown_url = |text: &str, field| {
            if is_shown(text) {
                server.endpoint(text, field)
            } else {
                Err(server.invalid("device authorization"))
            }
        };
        let verification_uri = shown_url(&device.verification_uri, "verification_uri")?;
        let verification_uri_complete = device
            .verification_uri_complete
            .as_deref()
            .filter(|uri| !uri.is_empty())
            .map(|uri| shown_url(uri, "verification_uri_complete"))
            .transpose()?;
        if device.device_code.is_empty() || !is_shown(&device.user_code) || device.expires_in == 0 {
            return Err(server.invalid("device authorization"));
        }
        let expires_in = Duration::from_secs(device.expires_in).min(MAX_DEVICE_CODE_LIFETIME);
        Ok(Self {
            server,
            credentials: Arc::new(credentials.clone()),
            endpoints,
            client_id,
            device_code: device.device_code,
            user_code: device.user_code,
            verification_uri,
            verification_uri_complete,
            interval: device
                .interval
                .map_or(DEFAULT_POLL_INTERVAL, |seconds| {
                    Duration::from_secs(seconds.max(1))
                })
                .min(expires_in),
            expires_in,
            deadline: Instant::now() + expires_in,
        })
    }

    pub fn issuer(&self) -> &str {
        &self.server.issuer
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

    /// How long the code stays valid from now.
    pub fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }

    /// Polls until the user approves, refuses, the code expires or `cancellation` fires, and
    /// saves the token on approval. An answer that is no OAuth response, or none at all, does not
    /// end the login: polling backs off, `progress` hears so once, and the code's expiry reports
    /// the last failure.
    pub async fn finish(
        &self,
        cancellation: &CancellationToken,
        progress: impl FnMut(Progress),
    ) -> Result<(), LoginError> {
        let token = self.authorize(cancellation, progress).await?;
        self.credentials.save_token(&token).await
    }

    /// Completes the exchange without persisting, for callers that must verify an account and
    /// obtain resource tokens before atomically replacing their saved login.
    pub async fn authorize(
        &self,
        cancellation: &CancellationToken,
        mut progress: impl FnMut(Progress),
    ) -> Result<OAuthToken, LoginError> {
        let token_endpoint = &self.endpoints.token;
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
            let request = self.server.http.post(token_endpoint.clone()).form(&[
                ("grant_type", DEVICE_CODE_GRANT),
                ("device_code", &self.device_code),
                ("client_id", &self.client_id),
            ]);
            let answer = tokio::select! {
                () = cancellation.cancelled() => return Err(LoginError::Cancelled),
                answer = self.server.exchange(request, token_endpoint) => answer,
            };
            let failure = match answer {
                Ok(Answer::Unexpected(status)) => {
                    format!("{token_endpoint} answered HTTP {status}")
                }
                Err(reason) => reason,
                Ok(Answer::Success(body)) => {
                    let token = self.token_from(&body)?;
                    if cancellation.is_cancelled() {
                        return Err(LoginError::Cancelled);
                    }
                    return Ok(token);
                }
                Ok(Answer::Refused(code)) => {
                    unreachable = None;
                    match code.as_str() {
                        "authorization_pending" => {}
                        "slow_down" => {
                            interval += SLOW_DOWN_STEP;
                            progress(Progress::SlowerPolling);
                        }
                        "access_denied" => return Err(LoginError::Denied),
                        "expired_token" => return Err(LoginError::Expired),
                        _ => {
                            return Err(LoginError::Refused {
                                message: format!(
                                    "{token_endpoint} refused the device code: {code}"
                                ),
                                code,
                            });
                        }
                    }
                    continue;
                }
            };
            if unreachable.is_none() {
                progress(Progress::Unreachable);
            }
            unreachable = Some(failure);
            interval = (interval * 2).min(MAX_RETRY_INTERVAL.max(interval));
        }
    }

    fn token_from(&self, body: &[u8]) -> Result<OAuthToken, LoginError> {
        let response: TokenResponse = parse(body)
            .filter(|token: &TokenResponse| {
                !token.access_token.is_empty() && !token.token_type.is_empty()
            })
            .ok_or_else(|| self.server.invalid("token"))?;
        Ok(OAuthToken {
            refresh_token: response.refresh_token(),
            expires_at: response.expires_at(),
            access_token: response.access_token,
            token_type: response.token_type,
            scope: response.scope,
            issuer: self.server.issuer.clone(),
            client_id: self.client_id.clone(),
            token_endpoint: self.endpoints.token.to_string(),
            revocation_endpoint: self.endpoints.revocation.as_ref().map(ToString::to_string),
        })
    }
}

/// Registers as a public device client and keeps the `client_id` for the next login of the
/// package, Tickloop's or its program's.
async fn register(
    server: &Server,
    endpoints: &Endpoints,
    credentials: &impl CredentialStore,
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
    let body = match server
        .exchange(request, endpoint)
        .await
        .map_err(LoginError::Failed)?
    {
        Answer::Success(body) => body,
        Answer::Refused(code) => {
            return Err(LoginError::Refused {
                message: format!("{endpoint} refused to register Tickloop: {code}"),
                code,
            });
        }
        Answer::Unexpected(status) => {
            return Err(LoginError::Failed(format!(
                "{endpoint} answered HTTP {status}"
            )));
        }
    };
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
    credentials
        .save_client_id(&server.issuer, &client_id)
        .await?;
    Ok(client_id)
}

/// Asks for a device code; `None` when the server does not know `client_id`.
async fn authorize_device(
    server: &Server,
    endpoints: &Endpoints,
    client_id: &str,
    scopes: &[String],
    resources: &[String],
) -> Result<Option<DeviceAuthorization>, LoginError> {
    let endpoint = &endpoints.device_authorization;
    let mut form = vec![("client_id", client_id.to_owned())];
    if !scopes.is_empty() {
        form.push(("scope", scopes.join(" ")));
    }
    form.extend(
        resources
            .iter()
            .map(|resource| ("resource", resource.clone())),
    );
    let request = server.http.post(endpoint.clone()).form(&form);
    match server
        .exchange(request, endpoint)
        .await
        .map_err(LoginError::Failed)?
    {
        Answer::Success(body) => parse(&body)
            .map(Some)
            .ok_or_else(|| server.invalid("device authorization")),
        Answer::Refused(code) if code == "invalid_client" => Ok(None),
        Answer::Refused(code) => Err(LoginError::Refused {
            message: format!("{endpoint} refused the device authorization: {code}"),
            code,
        }),
        Answer::Unexpected(status) => Err(LoginError::Failed(format!(
            "{endpoint} answered HTTP {status}"
        ))),
    }
}

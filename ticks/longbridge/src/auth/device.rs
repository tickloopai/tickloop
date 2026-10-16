//! The device login of `tick-longbridge auth`, made as Tickloop makes the package's login: the
//! server's metadata (RFC 8414), a client registered once per server and shared with Tickloop
//! (RFC 7591), and a device code the user approves (RFC 8628). The token is saved where
//! Tickloop keeps it, so the Agent and every `tick-longbridge` use the one login.

use std::time::Duration;

use reqwest::Url;
use serde::Deserialize;
use tick_packages::{OAuthToken, PackageCredentials};
use tick_provider::error::{Error, Result};
use tokio::{
    sync::mpsc,
    time::{Instant, sleep_until},
};

use super::{
    locked,
    server::{DEVICE_CODE_GRANT, Endpoints, REFRESH_TOKEN_GRANT, Reply, Server, error_code, parse},
};
use crate::not_authorized;

const CLIENT_NAME: &str = "Tickloop";
/// Registered as Longbridge's earlier device client was, which it accepts for device logins.
const REDIRECT_URI: &str = "http://localhost:60355/callback";
/// RFC 8628: the interval when the server names none, and how much slower each `slow_down`
/// makes the polling.
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(5);
const SLOW_DOWN_STEP: Duration = Duration::from_secs(5);
/// Polling backs off up to this while the server cannot be reached.
const MAX_RETRY_INTERVAL: Duration = Duration::from_secs(30);
/// A device code lives at most this long, whatever the server says.
const MAX_DEVICE_CODE_LIFETIME: Duration = Duration::from_secs(3600);
/// Longer user codes and pages than this are not shown to the user.
const MAX_SHOWN_LENGTH: usize = 2048;

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

/// A device authorization waiting for the user, who opens [`Self::verification_uri`] and
/// approves [`Self::user_code`]; [`Self::finish`] waits for the approval and saves the token.
pub struct DeviceLogin {
    server: Server,
    credentials: PackageCredentials,
    endpoints: Endpoints,
    client_id: String,
    device_code: String,
    user_code: String,
    verification_uri: Url,
    verification_uri_complete: Option<Url>,
    interval: Duration,
    deadline: Instant,
}

/// What the waiting screen tells the user while the login goes on.
pub enum LoginNotice {
    TemporaryFailureRetry,
    SlowerPolling,
}

impl LoginNotice {
    pub fn message(&self) -> &'static str {
        match self {
            Self::TemporaryFailureRetry => {
                "Longbridge authorization is temporarily unavailable; retrying automatically."
            }
            Self::SlowerPolling => {
                "Longbridge requested less frequent checks; waiting for confirmation."
            }
        }
    }
}

impl DeviceLogin {
    /// Discovers `issuer`'s endpoints, registers there unless Tickloop or this program did
    /// before, and asks for a device code.
    pub async fn start(credentials: &PackageCredentials, issuer: &str) -> Result<Self> {
        let server = Server::new(issuer)?;
        let endpoints = server.discover().await?;
        let saved_client = locked(credentials, {
            let issuer = issuer.to_owned();
            move |files| files.client_id(&issuer)
        })
        .await?;
        let is_saved_client = saved_client.is_some();
        let mut client_id = match saved_client {
            Some(client_id) => client_id,
            None => register(&server, &endpoints, credentials).await?,
        };
        let mut authorization = authorize_device(&server, &endpoints, &client_id).await?;
        if is_saved_client && authorization.is_none() {
            // The server forgot the client registered before: register anew, once.
            client_id = register(&server, &endpoints, credentials).await?;
            authorization = authorize_device(&server, &endpoints, &client_id).await?;
        }
        let device = authorization.ok_or_else(|| {
            not_authorized("Longbridge does not know the device login client (invalid_client)")
        })?;
        let is_shown = |text: &str| {
            !text.is_empty()
                && text.len() <= MAX_SHOWN_LENGTH
                && !text.chars().any(char::is_control)
        };
        let shown_page = |page: &str, field| {
            if is_shown(page) {
                server.endpoint(page, field)
            } else {
                Err(server.invalid("device authorization"))
            }
        };
        let verification_uri = shown_page(&device.verification_uri, "verification_uri")?;
        let verification_uri_complete = device
            .verification_uri_complete
            .as_deref()
            .filter(|page| !page.is_empty())
            .map(|page| shown_page(page, "verification_uri_complete"))
            .transpose()?;
        if device.device_code.is_empty() || !is_shown(&device.user_code) || device.expires_in == 0 {
            return Err(server.invalid("device authorization"));
        }
        let lifetime = Duration::from_secs(device.expires_in).min(MAX_DEVICE_CODE_LIFETIME);
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
            deadline: Instant::now() + lifetime,
        })
    }

    /// The code the user approves on the verification page.
    pub fn user_code(&self) -> &str {
        &self.user_code
    }

    /// The verification page with the user code filled in when Longbridge offers one.
    pub fn verification_uri(&self) -> &str {
        self.verification_uri_complete
            .as_ref()
            .unwrap_or(&self.verification_uri)
            .as_str()
    }

    /// The verification page where the user enters the user code.
    pub fn plain_verification_uri(&self) -> &str {
        self.verification_uri.as_str()
    }

    pub fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }

    /// Polls until the user approves, refuses or the code expires, and saves the token on
    /// approval. While the server cannot be reached the polling backs off and `notices` hears
    /// so once; it hears too when the server asks for slower polling.
    pub async fn finish(&self, notices: &mpsc::UnboundedSender<LoginNotice>) -> Result<()> {
        let mut interval = self.interval;
        let mut is_unreachable = false;
        loop {
            let next_poll = Instant::now() + interval;
            if next_poll >= self.deadline {
                return Err(if is_unreachable {
                    Error::network(
                        "cannot reach the Longbridge authorization server; the device code expired",
                    )
                } else {
                    not_authorized("device code expired")
                });
            }
            sleep_until(next_poll).await;
            let request = self
                .server
                .http()
                .post(self.endpoints.token.clone())
                .form(&[
                    ("grant_type", DEVICE_CODE_GRANT),
                    ("device_code", &self.device_code),
                    ("client_id", &self.client_id),
                ]);
            let reply = match self.server.request_token(request).await {
                Ok(Reply::Unexpected(status)) if status.is_server_error() => None,
                Ok(reply) => Some(reply),
                Err(_) => None,
            };
            let Some(reply) = reply else {
                if !is_unreachable {
                    let _ = notices.send(LoginNotice::TemporaryFailureRetry);
                }
                is_unreachable = true;
                interval = (interval * 2).min(MAX_RETRY_INTERVAL);
                continue;
            };
            is_unreachable = false;
            match reply {
                Reply::Token(response) => {
                    let token = OAuthToken {
                        refresh_token: response.refresh_token(),
                        expires_at: response.expires_at(),
                        access_token: response.access_token,
                        token_type: response.token_type,
                        scope: response.scope,
                        issuer: self.server.issuer().to_owned(),
                        client_id: self.client_id.clone(),
                        token_endpoint: self.endpoints.token.to_string(),
                        revocation_endpoint: self
                            .endpoints
                            .revocation
                            .as_ref()
                            .map(ToString::to_string),
                    };
                    return locked(&self.credentials, move |files| files.save_token(&token)).await;
                }
                Reply::Refused(code) => match code.as_str() {
                    "authorization_pending" => {}
                    "slow_down" => {
                        interval += SLOW_DOWN_STEP;
                        let _ = notices.send(LoginNotice::SlowerPolling);
                    }
                    "access_denied" => return Err(not_authorized("authorization denied")),
                    "expired_token" => return Err(not_authorized("device code expired")),
                    code => {
                        return Err(not_authorized(format!(
                            "Longbridge refused the device authorization ({code})"
                        )));
                    }
                },
                Reply::Unexpected(status) => {
                    return Err(Error::network(format!(
                        "device authorization failed: {} answered HTTP {status}",
                        self.endpoints.token
                    )));
                }
            }
        }
    }
}

/// Registers as a public device client and keeps the `client_id` for the next login, Tickloop's
/// included.
async fn register(
    server: &Server,
    endpoints: &Endpoints,
    credentials: &PackageCredentials,
) -> Result<String> {
    let endpoint = endpoints.registration.as_ref().ok_or_else(|| {
        Error::network(format!(
            "{} offers no client registration (registration_endpoint)",
            server.issuer()
        ))
    })?;
    let metadata = serde_json::json!({
        "client_name": CLIENT_NAME,
        "grant_types": [DEVICE_CODE_GRANT, REFRESH_TOKEN_GRANT],
        "token_endpoint_auth_method": "none",
        "redirect_uris": [REDIRECT_URI],
    });
    let (status, body) = server
        .exchange(server.http().post(endpoint.clone()).json(&metadata))
        .await
        .map_err(|_| Error::network("client registration request failed"))?;
    if !status.is_success() {
        return Err(Error::network(format!(
            "client registration failed: {endpoint} answered HTTP {status}"
        )));
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
        let (issuer, client_id) = (server.issuer().to_owned(), client_id.clone());
        move |files| files.set_client_id(&issuer, Some(&client_id))
    })
    .await?;
    Ok(client_id)
}

/// Asks for a device code; `None` when the server does not know `client_id`.
async fn authorize_device(
    server: &Server,
    endpoints: &Endpoints,
    client_id: &str,
) -> Result<Option<DeviceAuthorization>> {
    let endpoint = &endpoints.device_authorization;
    let (status, body) = server
        .exchange(
            server
                .http()
                .post(endpoint.clone())
                .form(&[("client_id", client_id)]),
        )
        .await
        .map_err(|_| Error::network("device authorization request failed"))?;
    if status.is_success() {
        return parse(&body)
            .map(Some)
            .ok_or_else(|| server.invalid("device authorization"));
    }
    match error_code(&body).as_deref() {
        Some("invalid_client") => Ok(None),
        Some(code) => Err(not_authorized(format!(
            "Longbridge refused the device authorization ({code})"
        ))),
        None => Err(Error::network(format!(
            "device authorization failed: {endpoint} answered HTTP {status}"
        ))),
    }
}

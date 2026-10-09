use std::{
    io::{BufRead, Write},
    path::Path,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use longbridge::oauth::{StoredToken, TokenStorage};
use reqwest::{Client, Response};
use serde::{Deserialize, Serialize};
use tokio::{
    sync::{mpsc, watch},
    time::{Instant, sleep_until, timeout_at},
};

use super::storage::{AuthStorage, atomic_write, now_seconds};
use crate::{
    config::{Region, Settings},
    error::{Error, Result},
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const DEFAULT_POLL_SECONDS: u64 = 5;
const DEFAULT_TOKEN_LIFETIME_SECONDS: u64 = 3600;
const SLOW_DOWN_SECONDS: u64 = 5;
const CLIENT_NAME: &str = "Tick Loop";
const REDIRECT_URI: &str = "http://localhost:60355/callback";

#[derive(Clone)]
pub struct AuthEndpoints {
    pub ap: String,
    pub us: String,
}

impl AuthEndpoints {
    pub fn for_region(region: Region) -> Self {
        Self {
            ap: format!(
                "{}/oauth2",
                match region {
                    Region::Cn => "https://openapi.longbridge.cn",
                    Region::Global => "https://openapi.longbridge.com",
                }
            ),
            us: "https://openapi.longbridge.com/oauth2".into(),
        }
    }
}

#[derive(Deserialize, Serialize)]
struct Registration {
    client_id: String,
}

#[derive(Deserialize)]
struct DeviceCode {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: Option<String>,
    expires_in: u64,
    interval: Option<u64>,
}

pub struct DeviceLogin {
    client_id: String,
    device: DeviceCode,
    deadline: Instant,
}

impl DeviceLogin {
    pub fn user_code(&self) -> &str {
        &self.device.user_code
    }

    pub fn verification_uri(&self) -> &str {
        self.device
            .verification_uri_complete
            .as_deref()
            .filter(|uri| !uri.is_empty())
            .unwrap_or(&self.device.verification_uri)
    }

    pub fn expires_in(&self) -> u64 {
        self.device.expires_in
    }

    pub fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }
}

#[derive(Deserialize)]
pub(super) struct TokenResponse {
    pub access_token: String,
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_in: Option<u64>,
}

impl TokenResponse {
    pub fn stored(self, client_id: String) -> Result<StoredToken> {
        let expires_in = self.expires_in.unwrap_or(DEFAULT_TOKEN_LIFETIME_SECONDS);
        if self.access_token.is_empty() || expires_in == 0 {
            return Err(Error::network("invalid token response"));
        }
        Ok(StoredToken {
            client_id,
            access_token: self.access_token,
            refresh_token: self.refresh_token,
            expires_at: now_seconds().saturating_add(expires_in),
        })
    }
}

pub(super) fn http_client() -> Result<Client> {
    Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("tickloop-ticks/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|_| Error::network("cannot create HTTP client"))
}

pub(super) async fn response_json<T: serde::de::DeserializeOwned>(
    response: Response,
    operation: &str,
) -> Result<T> {
    let status = response.status();
    if !status.is_success() {
        return Err(Error::network(format!("{operation} failed: HTTP {status}")));
    }
    response
        .json()
        .await
        .map_err(|_| Error::network(format!("invalid {operation} response")))
}

/// Complete interactive input before installing asynchronous signal handlers.
pub fn prepare_region(
    settings: &Settings,
    config_path: &Path,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> Result<Region> {
    match settings.region {
        Some(region) => Ok(region),
        None => {
            writeln!(output, "Select region: cn (中国大陆) or global (全球)")?;
            output.flush()?;
            let mut choice = String::new();
            input.read_line(&mut choice)?;
            let region: Region = choice
                .trim()
                .parse()
                .map_err(|_| Error::usage("enter cn or global to select a region"))?;
            save_region(config_path, region)?;
            Ok(region)
        }
    }
}

pub fn save_region(config_path: &Path, region: Region) -> Result<()> {
    let existing = match std::fs::read_to_string(config_path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.into()),
    };
    let mut document = existing
        .parse::<toml_edit::DocumentMut>()
        .map_err(|error| Error::usage(error.to_string()))?;
    document["region"] = toml_edit::value(region.as_str());
    atomic_write(config_path, document.to_string().as_bytes())?;
    Ok(())
}

pub async fn begin_login(
    region: Region,
    storage: &AuthStorage,
    endpoints: Option<AuthEndpoints>,
) -> Result<DeviceLogin> {
    let endpoints = endpoints.unwrap_or_else(|| AuthEndpoints::for_region(region));
    let http = http_client()?;
    let registration_path = storage.directory.join("client.json");
    let registration: Registration = match std::fs::read(&registration_path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|_| Error::usage("invalid local client registration"))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let response = http
                .post(format!("{}/register", endpoints.ap))
                .json(&serde_json::json!({"client_name": CLIENT_NAME, "redirect_uris": [REDIRECT_URI]}))
                .send()
                .await
                .map_err(|_| Error::network("client registration request failed"))?;
            let registration: Registration = response_json(response, "client registration").await?;
            if registration.client_id.is_empty() {
                return Err(Error::network("missing client_id in registration"));
            }
            atomic_write(
                &registration_path,
                &serde_json::to_vec(&registration)
                    .map_err(|error| Error::usage(error.to_string()))?,
            )?;
            registration
        }
        Err(error) => return Err(error.into()),
    };
    let response = http
        .post(format!("{}/device/authorize", endpoints.ap))
        .form(&[("client_id", &registration.client_id)])
        .send()
        .await
        .map_err(|_| Error::network("device authorization request failed"))?;
    let device: DeviceCode = response_json(response, "device authorization").await?;
    if device.device_code.is_empty()
        || device.user_code.is_empty()
        || device.verification_uri.is_empty()
    {
        return Err(Error::network("incomplete device authorization response"));
    }
    let deadline = Instant::now()
        .checked_add(Duration::from_secs(device.expires_in))
        .ok_or_else(|| Error::network("invalid device code expiry"))?;
    Ok(DeviceLogin {
        client_id: registration.client_id,
        device,
        deadline,
    })
}

pub(super) async fn complete_login(
    region: Region,
    storage: &AuthStorage,
    endpoints: Option<AuthEndpoints>,
    login: &DeviceLogin,
    notices: &mpsc::UnboundedSender<LoginNotice>,
) -> Result<()> {
    let endpoints = endpoints.unwrap_or_else(|| AuthEndpoints::for_region(region));
    let http = http_client()?;
    let token = wait_for_token(
        &http,
        &endpoints,
        &login.client_id,
        &login.device,
        login.deadline,
        notices,
    )
    .await?;
    storage
        .save(&token)
        .map_err(|error| Error::usage(error.to_string()))
}

pub async fn run_login(
    settings: &Settings,
    config_path: &Path,
    storage: &AuthStorage,
    endpoints: Option<AuthEndpoints>,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> Result<()> {
    let region = prepare_region(settings, config_path, input, output)?;
    let login = begin_login(region, storage, endpoints.clone()).await?;
    writeln!(
        output,
        "\nTick Loop · Longbridge\n\n1. Open this link in your browser:\n   {}\n\n2. Confirm this device code and approve access:\n   {}\n\nWaiting for Longbridge to confirm (up to {}s). Ctrl+C cancels.",
        login.verification_uri(),
        login.user_code(),
        login.expires_in()
    )?;
    output.flush()?;
    let (notices, mut updates) = mpsc::unbounded_channel();
    let polling = complete_login(region, storage, endpoints, &login, &notices);
    tokio::pin!(polling);
    loop {
        tokio::select! {
            result = &mut polling => break result?,
            Some(notice) = updates.recv() => {
                writeln!(output, "{}", notice.message())?;
                output.flush()?;
            }
        }
    }
    writeln!(output, "\nLogged in. You can close the authorization page.")?;
    Ok(())
}

async fn wait_for_token(
    http: &Client,
    endpoints: &AuthEndpoints,
    client_id: &str,
    device: &DeviceCode,
    deadline: Instant,
    notices: &mpsc::UnboundedSender<LoginNotice>,
) -> Result<StoredToken> {
    let (interval, _) = watch::channel(device.interval.unwrap_or(DEFAULT_POLL_SECONDS).max(1));
    let poll = TokenPoll {
        http,
        client_id,
        device,
        deadline,
        interval,
        active_transient_failures: AtomicUsize::new(0),
        notices,
    };
    let ap = poll.run(&endpoints.ap, "ap");
    let us = poll.run(&endpoints.us, "us");
    tokio::pin!(ap, us);
    let (first, second) = tokio::select! {
        result = &mut ap => (result, us),
        result = &mut us => (result, ap),
    };
    match first {
        Ok(token) => Ok(token),
        Err(PollFailure::Terminal(error)) => Err(error),
        Err(PollFailure::Unavailable(first_error)) => {
            let _ = notices.send(LoginNotice::EndpointUnavailable);
            second.await.map_err(|failure| match failure {
                PollFailure::Terminal(error) => error,
                PollFailure::Unavailable(error) => {
                    Error::network(format!("{first_error}; {error}"))
                }
            })
        }
    }
}

pub(super) enum LoginNotice {
    TemporaryFailureRetry,
    EndpointUnavailable,
    SlowerPolling,
}

impl LoginNotice {
    pub(super) fn message(&self) -> &'static str {
        match self {
            Self::TemporaryFailureRetry => {
                "Longbridge authorization is temporarily unavailable; retrying automatically."
            }
            Self::EndpointUnavailable => {
                "One Longbridge authorization endpoint failed; checking the other endpoint."
            }
            Self::SlowerPolling => {
                "Longbridge requested less frequent checks; waiting for confirmation."
            }
        }
    }
}

enum PollFailure {
    Terminal(Error),
    Unavailable(Error),
}

impl From<Error> for PollFailure {
    fn from(error: Error) -> Self {
        Self::Terminal(error)
    }
}

struct TokenPoll<'a> {
    http: &'a Client,
    client_id: &'a str,
    device: &'a DeviceCode,
    deadline: Instant,
    interval: watch::Sender<u64>,
    active_transient_failures: AtomicUsize,
    notices: &'a mpsc::UnboundedSender<LoginNotice>,
}

impl TokenPoll<'_> {
    async fn run(&self, base: &str, center: &str) -> std::result::Result<StoredToken, PollFailure> {
        let mut changes = self.interval.subscribe();
        let mut previous_poll = Instant::now();
        let mut has_transient_failure = false;
        loop {
            let next_poll = previous_poll
                .checked_add(Duration::from_secs(*changes.borrow_and_update()))
                .unwrap_or(self.deadline)
                .min(self.deadline);
            tokio::select! {
                _ = sleep_until(next_poll) => (),
                _ = changes.changed() => continue,
            }
            if Instant::now() >= self.deadline {
                return Err(if has_transient_failure {
                    PollFailure::Unavailable(Error::network(format!(
                        "cannot reach Longbridge authorization server ({center}); device code expired"
                    )))
                } else {
                    Error::auth("device code expired").into()
                });
            }
            let response = timeout_at(
                self.deadline,
                self.http
                    .post(format!("{base}/token"))
                    .header("x-dc-region", center)
                    .form(&[
                        ("client_id", self.client_id),
                        ("device_code", &self.device.device_code),
                        ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                    ])
                    .send(),
            )
            .await
            .map_err(|_| {
                PollFailure::Unavailable(Error::network(format!(
                    "Longbridge token request timed out ({center}); device code expired"
                )))
            })?;
            previous_poll = Instant::now();
            let response = match response {
                Ok(response) if !response.status().is_server_error() => response,
                _ => {
                    if !has_transient_failure
                        && self
                            .active_transient_failures
                            .fetch_add(1, Ordering::SeqCst)
                            == 0
                    {
                        let _ = self.notices.send(LoginNotice::TemporaryFailureRetry);
                    }
                    has_transient_failure = true;
                    self.interval
                        .send_modify(|seconds| *seconds = seconds.saturating_mul(2).min(30));
                    continue;
                }
            };
            if has_transient_failure {
                self.active_transient_failures
                    .fetch_sub(1, Ordering::SeqCst);
                has_transient_failure = false;
            }
            if response.status().is_success() {
                let token = timeout_at(
                    self.deadline,
                    response_json::<TokenResponse>(response, "token"),
                )
                .await
                .map_err(|_| {
                    PollFailure::Unavailable(Error::network(format!(
                        "Longbridge token response timed out ({center}); device code expired"
                    )))
                })?
                .map_err(PollFailure::Terminal)?;
                return token
                    .stored(self.client_id.to_owned())
                    .map_err(PollFailure::Terminal);
            }
            let status = response.status();
            let body: serde_json::Value = timeout_at(self.deadline, response.json())
                .await
                .map_err(|_| {
                    PollFailure::Unavailable(Error::network(format!(
                        "Longbridge token response timed out ({center}); device code expired"
                    )))
                })?
                .map_err(|_| {
                    PollFailure::Unavailable(Error::network(format!(
                        "invalid token poll response ({center}; HTTP {status})"
                    )))
                })?;
            match body["error"].as_str() {
                Some("authorization_pending") => (),
                Some("slow_down") => {
                    self.interval.send_modify(|seconds| {
                        *seconds = seconds.saturating_add(SLOW_DOWN_SECONDS)
                    });
                    let _ = self.notices.send(LoginNotice::SlowerPolling);
                }
                Some("access_denied") => {
                    return Err(Error::auth("authorization denied").into());
                }
                Some("expired_token") => {
                    return Err(Error::auth("device code expired").into());
                }
                Some("invalid_client") => {
                    return Err(
                        Error::auth("device login client was rejected (invalid_client)").into(),
                    );
                }
                Some("invalid_grant") => {
                    return Err(
                        Error::auth("device authorization was rejected (invalid_grant)").into(),
                    );
                }
                _ => {
                    return Err(PollFailure::Unavailable(Error::network(format!(
                        "device authorization failed ({center}; HTTP {status})"
                    ))));
                }
            }
        }
    }
}

//! GitHub Copilot sign-in as pi does it: GitHub's device flow for the GitHub Copilot app that
//! VS Code uses, then an exchange of that GitHub token for a short-lived Copilot token whose
//! `proxy-ep` names the account's API host. Tick Loop has no partner client of its own, so GitHub
//! shows its Copilot app on the consent page.

use std::{
    fmt,
    fs::{File, OpenOptions},
    io::{self, Read},
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
    time::{Duration, Instant},
};

use reqwest::{Client, redirect::Policy};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    model::copilot::{CLIENT_HEADERS, CLIENT_USER_AGENT},
    network::{HttpPolicy, build_http_client, is_loopback},
    private_fs::{
        OpenTarget, PRIVATE_DIRECTORY_MODE, PRIVATE_FILE_MODE, open_without_following_links,
        replace_file_atomically, unix_time_milliseconds, verify_private_directory,
    },
};

/// The GitHub Copilot app used by VS Code, pi and OpenClaw.
const CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";
const SCOPE: &str = "read:user";
pub const CONSENT_NOTICE: &str = "GitHub shows its own GitHub Copilot app (the client VS Code uses); Tick Loop signs in through it like pi, which GitHub may treat as an unsupported client.";
const GITHUB_ORIGIN: &str = "https://github.com";
const GITHUB_API_ORIGIN: &str = "https://api.github.com";
const COPILOT_TOKEN_PATH: &str = "/copilot_internal/v2/token";
const COPILOT_API_HOST_SUFFIX: &str = ".githubcopilot.com";
/// Copilot token expiries below this are Unix seconds rather than milliseconds.
const SECONDS_EXPIRY_LIMIT: u64 = 100_000_000_000;
const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
const CREDENTIAL_FILE: &str = "github-copilot.json";
const LOCK_FILE: &str = "github-copilot.lock";
const RESPONSE_LIMIT: usize = 64 * 1024;
const MAX_CREDENTIAL_FILE_BYTES: usize = 64 * 1024;
const REFRESH_SKEW_MS: u64 = 5 * 60_000;
const MAX_LOGIN_MS: u64 = 15 * 60_000;
const DEFAULT_INTERVAL_MS: u64 = 5_000;
const MIN_INTERVAL_MS: u64 = 1_000;
const MAX_INTERVAL_MS: u64 = 60_000;
const SLOW_DOWN_INCREMENT_MS: u64 = 5_000;
const LOCK_TIMEOUT: Duration = Duration::from_secs(35);

#[derive(Debug, thiserror::Error)]
pub enum CopilotAuthError {
    #[error("GitHub Copilot credential refresh exceeded the request attempt budget")]
    BudgetExhausted,
    #[error("GitHub Copilot sign-in cancelled")]
    Cancelled,
    #[error("GitHub device authorization expired; retry from /provider")]
    Expired,
    #[error("GitHub authorization was denied")]
    Denied,
    #[error("GitHub device flow is unavailable for the Copilot app")]
    DeviceFlowDisabled,
    #[error("GitHub OAuth response is invalid or belongs to another authorization")]
    InvalidResponse,
    #[error("GitHub rejected the Copilot authorization request ({0})")]
    Rejected(&'static str),
    #[error(
        "this GitHub account has no Copilot access for this client; check the subscription or organization policy"
    )]
    NoCopilotAccess,
    #[error("GitHub OAuth request failed with HTTP {0}")]
    Http(u16),
    #[error("GitHub OAuth connection failed or timed out")]
    Network,
    #[error("GitHub Copilot is not signed in; select its Provider in /model")]
    NotLoggedIn,
    #[error("GitHub Copilot sign-in expired; select the Provider and press r")]
    RefreshFailed,
    #[error("GitHub Copilot credential storage is invalid or unsafe")]
    Storage,
    #[error("GitHub Copilot sign-in was superseded or signed out; start a new authorization")]
    Superseded,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct CopilotCredentials {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    refresh_token: Option<String>,
    /// `None` when the GitHub App issues tokens that never expire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    refresh_expires_at_ms: Option<u64>,
    /// Stable for refreshes of one sign-in; a new sign-in gets a new identity.
    pub identity: Uuid,
    /// The Copilot token exchanged for the current GitHub token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session: Option<CopilotSession>,
}

#[derive(Clone, Deserialize, Serialize)]
struct CopilotSession {
    token: String,
    expires_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    api_base: Option<String>,
}

/// What a Copilot model request needs: the short-lived Copilot token, the account's API base
/// when the token names one, and the stable sign-in identity.
#[derive(Clone)]
pub struct CopilotAccess {
    pub token: String,
    pub api_base: Option<String>,
    pub identity: Uuid,
}

impl fmt::Debug for CopilotAccess {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CopilotAccess")
            .field("token", &"[redacted]")
            .field("api_base", &self.api_base)
            .finish()
    }
}

impl fmt::Debug for CopilotCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CopilotCredentials")
            .field("tokens", &"[redacted]")
            .field("expires_at_ms", &self.expires_at_ms)
            .finish()
    }
}

#[derive(Deserialize, Serialize)]
struct StoredState {
    generation: Uuid,
    credentials: Option<CopilotCredentials>,
}

pub struct CopilotDeviceLogin {
    pub verification_uri: String,
    pub user_code: String,
    device_code: String,
    generation: Uuid,
    expires_at_ms: u64,
    interval_ms: u64,
}

#[derive(Clone)]
pub struct CopilotAuth {
    root: PathBuf,
    origin: String,
    api_origin: String,
    client: Client,
    min_interval_ms: u64,
}

impl CopilotAuth {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self::at_origin(
            root.into(),
            GITHUB_ORIGIN.into(),
            GITHUB_API_ORIGIN.into(),
            MIN_INTERVAL_MS,
        )
    }

    fn at_origin(root: PathBuf, origin: String, api_origin: String, min_interval_ms: u64) -> Self {
        let mut policy = HttpPolicy::new(Policy::none());
        policy.timeout = Some(Duration::from_secs(30));
        let (client, _) = build_http_client(
            &reqwest::Url::parse(&origin).expect("validated OAuth origin"),
            policy,
        )
        .expect("valid OAuth HTTP client");
        Self {
            root,
            origin,
            api_origin,
            client,
            min_interval_ms,
        }
    }

    /// A loopback issuer serving both the GitHub login and API paths, whose polling interval is
    /// honored without the production floor.
    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub fn new_for_loopback_test(
        root: impl Into<PathBuf>,
        origin: &str,
    ) -> Result<Self, CopilotAuthError> {
        let url = reqwest::Url::parse(origin).map_err(|_| CopilotAuthError::InvalidResponse)?;
        if url.scheme() != "http"
            || !is_loopback(&url)
            || url.port().is_none()
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(CopilotAuthError::InvalidResponse);
        }
        let origin = origin.trim_end_matches('/').to_owned();
        Ok(Self::at_origin(root.into(), origin.clone(), origin, 0))
    }

    pub fn has_saved_login(&self) -> Result<bool, CopilotAuthError> {
        match std::fs::symlink_metadata(self.root.join("auth").join(CREDENTIAL_FILE)) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(_) => return Err(CopilotAuthError::Storage),
        }
        Ok(self
            .read()?
            .is_some_and(|state| state.credentials.is_some()))
    }

    pub async fn start_device_login(
        &self,
        cancel: &CancellationToken,
    ) -> Result<CopilotDeviceLogin, CopilotAuthError> {
        let generation = Uuid::new_v4();
        {
            let _lock = self.lock(cancel).await?;
            let credentials = self.read()?.and_then(|state| state.credentials);
            self.save(&StoredState {
                generation,
                credentials,
            })?;
        }
        let payload = self
            .post(
                "/login/device/code",
                &[("client_id", CLIENT_ID), ("scope", SCOPE)],
                cancel,
            )
            .await?;
        if let Some(error) = error_code(&payload) {
            return Err(authorization_error(error));
        }
        let verification_uri = field(&payload, "verification_uri")?;
        let url = reqwest::Url::parse(&verification_uri)
            .map_err(|_| CopilotAuthError::InvalidResponse)?;
        if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
            return Err(CopilotAuthError::InvalidResponse);
        }
        let lifetime_ms = payload["expires_in"]
            .as_u64()
            .filter(|seconds| *seconds > 0)
            .ok_or(CopilotAuthError::InvalidResponse)?
            .saturating_mul(1000)
            .min(MAX_LOGIN_MS);
        Ok(CopilotDeviceLogin {
            verification_uri,
            user_code: field(&payload, "user_code")?,
            device_code: field(&payload, "device_code")?,
            generation,
            expires_at_ms: unix_time_milliseconds().saturating_add(lifetime_ms),
            interval_ms: self.interval(payload["interval"].as_u64(), DEFAULT_INTERVAL_MS),
        })
    }

    pub async fn finish_device_login(
        &self,
        login: CopilotDeviceLogin,
        cancel: &CancellationToken,
    ) -> Result<CopilotCredentials, CopilotAuthError> {
        let mut interval = login.interval_ms;
        loop {
            let remaining = login.expires_at_ms.saturating_sub(unix_time_milliseconds());
            if remaining == 0 {
                return Err(CopilotAuthError::Expired);
            }
            tokio::select! {
                biased;
                () = cancel.cancelled() => return Err(CopilotAuthError::Cancelled),
                () = tokio::time::sleep(Duration::from_millis(interval.min(remaining))) => {},
            }
            let payload = self
                .post(
                    "/login/oauth/access_token",
                    &[
                        ("client_id", CLIENT_ID),
                        ("device_code", &login.device_code),
                        ("grant_type", DEVICE_GRANT),
                    ],
                    cancel,
                )
                .await?;
            match error_code(&payload) {
                None => {
                    let credential = parse_credentials(&payload, Uuid::new_v4())?;
                    let _lock = self.lock(cancel).await?;
                    let current = self.read()?.ok_or(CopilotAuthError::Superseded)?;
                    if current.generation != login.generation {
                        return Err(CopilotAuthError::Superseded);
                    }
                    if cancel.is_cancelled() {
                        return Err(CopilotAuthError::Cancelled);
                    }
                    self.save(&StoredState {
                        generation: login.generation,
                        credentials: Some(credential.clone()),
                    })?;
                    return Ok(credential);
                }
                Some("authorization_pending") => {}
                Some("slow_down") => {
                    interval = self.interval(
                        payload["interval"].as_u64(),
                        interval.saturating_add(SLOW_DOWN_INCREMENT_MS),
                    );
                }
                Some(error) => return Err(authorization_error(error)),
            }
        }
    }

    pub async fn resolve_credentials(
        &self,
        cancel: &CancellationToken,
    ) -> Result<CopilotCredentials, CopilotAuthError> {
        let _lock = self.lock(cancel).await?;
        let mut state = self.read()?.ok_or(CopilotAuthError::NotLoggedIn)?;
        let credentials = self
            .current_github_sign_in(&mut state, &mut || true)
            .await?;
        if cancel.is_cancelled() {
            return Err(CopilotAuthError::Cancelled);
        }
        Ok(credentials)
    }

    pub async fn resolve_access(
        &self,
        cancel: &CancellationToken,
    ) -> Result<CopilotAccess, CopilotAuthError> {
        self.resolve_access_with_refresh_gate(cancel, || true).await
    }

    /// Refreshing the GitHub token and exchanging it for a Copilot token each consume one
    /// request attempt from `allow_refresh`.
    pub(crate) async fn resolve_access_with_refresh_gate(
        &self,
        cancel: &CancellationToken,
        mut allow_refresh: impl FnMut() -> bool,
    ) -> Result<CopilotAccess, CopilotAuthError> {
        let _lock = self.lock(cancel).await?;
        let mut state = self.read()?.ok_or(CopilotAuthError::NotLoggedIn)?;
        let mut credentials = self
            .current_github_sign_in(&mut state, &mut allow_refresh)
            .await?;
        let now = unix_time_milliseconds();
        let session = match credentials.session.clone() {
            Some(session) if session.expires_at_ms > now.saturating_add(REFRESH_SKEW_MS) => session,
            _ => {
                if !allow_refresh() {
                    return Err(CopilotAuthError::BudgetExhausted);
                }
                let session = self
                    .exchange_for_copilot_token(&credentials.access_token, cancel)
                    .await?;
                credentials.session = Some(session.clone());
                state.credentials = Some(credentials.clone());
                self.save(&state)?;
                session
            }
        };
        if cancel.is_cancelled() {
            return Err(CopilotAuthError::Cancelled);
        }
        Ok(CopilotAccess {
            token: session.token,
            api_base: session.api_base,
            identity: credentials.identity,
        })
    }

    /// The saved GitHub sign-in, refreshed when it is about to expire. The caller holds the lock
    /// and checks cancellation afterwards: a used refresh token is invalid, so a started refresh
    /// always completes and is saved.
    async fn current_github_sign_in(
        &self,
        state: &mut StoredState,
        allow_refresh: &mut impl FnMut() -> bool,
    ) -> Result<CopilotCredentials, CopilotAuthError> {
        let current = state
            .credentials
            .clone()
            .ok_or(CopilotAuthError::NotLoggedIn)?;
        let now = unix_time_milliseconds();
        if current
            .expires_at_ms
            .is_none_or(|deadline| deadline > now.saturating_add(REFRESH_SKEW_MS))
        {
            return Ok(current);
        }
        let refresh_token = current.refresh_token.clone().filter(|_| {
            current
                .refresh_expires_at_ms
                .is_none_or(|deadline| deadline > now)
        });
        let Some(refresh_token) = refresh_token else {
            if current.expires_at_ms.is_some_and(|deadline| deadline > now) {
                return Ok(current);
            }
            state.credentials = None;
            self.save(state)?;
            return Err(CopilotAuthError::RefreshFailed);
        };
        if !allow_refresh() {
            return Err(CopilotAuthError::BudgetExhausted);
        }
        // GitHub invalidates a refresh token once it is used, so the exchange runs to completion
        // (bounded by the HTTP timeout) and its result is saved before a cancellation is honored.
        let payload = self
            .post(
                "/login/oauth/access_token",
                &[
                    ("client_id", CLIENT_ID),
                    ("grant_type", "refresh_token"),
                    ("refresh_token", &refresh_token),
                ],
                &CancellationToken::new(),
            )
            .await?;
        if let Some(error) = error_code(&payload) {
            if error == "bad_refresh_token" {
                state.credentials = None;
                self.save(state)?;
                return Err(CopilotAuthError::RefreshFailed);
            }
            return Err(authorization_error(error));
        }
        let mut refreshed = parse_credentials(&payload, current.identity)?;
        if refreshed.refresh_token.is_none() {
            refreshed.refresh_token = Some(refresh_token);
            refreshed.refresh_expires_at_ms = current.refresh_expires_at_ms;
        }
        state.credentials = Some(refreshed.clone());
        self.save(state)?;
        Ok(refreshed)
    }

    async fn exchange_for_copilot_token(
        &self,
        github_token: &str,
        cancel: &CancellationToken,
    ) -> Result<CopilotSession, CopilotAuthError> {
        let mut authorization =
            reqwest::header::HeaderValue::from_str(&format!("Bearer {github_token}"))
                .map_err(|_| CopilotAuthError::Storage)?;
        authorization.set_sensitive(true);
        let mut request = self
            .client
            .get(format!("{}{COPILOT_TOKEN_PATH}", self.api_origin))
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::AUTHORIZATION, authorization);
        for (name, value) in CLIENT_HEADERS {
            request = request.header(name, value);
        }
        let (status, payload) = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(CopilotAuthError::Cancelled),
            result = read_json(request) => result?,
        };
        match status {
            200..=299 => {}
            401 => return Err(CopilotAuthError::RefreshFailed),
            403 | 404 => return Err(CopilotAuthError::NoCopilotAccess),
            status => return Err(CopilotAuthError::Http(status)),
        }
        let payload = payload.ok_or(CopilotAuthError::InvalidResponse)?;
        let token = field(&payload, "token")?;
        let expires_at = payload["expires_at"]
            .as_u64()
            .ok_or(CopilotAuthError::InvalidResponse)?;
        let expires_at_ms = if expires_at < SECONDS_EXPIRY_LIMIT {
            expires_at.saturating_mul(1000)
        } else {
            expires_at
        };
        Ok(CopilotSession {
            api_base: api_base_from_token(&token),
            token,
            expires_at_ms,
        })
    }

    /// Removes the local sign-in only; GitHub keeps the app authorization until the user
    /// revokes it in their GitHub settings.
    pub async fn logout(&self, cancel: &CancellationToken) -> Result<(), CopilotAuthError> {
        let _lock = self.lock(cancel).await?;
        self.save(&StoredState {
            generation: Uuid::new_v4(),
            credentials: None,
        })
    }

    fn interval(&self, server_seconds: Option<u64>, fallback_ms: u64) -> u64 {
        server_seconds
            .map_or(fallback_ms, |seconds| seconds.saturating_mul(1000))
            .clamp(self.min_interval_ms, MAX_INTERVAL_MS)
    }

    async fn post(
        &self,
        path: &str,
        form: &[(&str, &str)],
        cancel: &CancellationToken,
    ) -> Result<Value, CopilotAuthError> {
        let request = self
            .client
            .post(format!("{}{path}", self.origin))
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::USER_AGENT, CLIENT_USER_AGENT)
            .form(form);
        tokio::select! {
            biased;
            () = cancel.cancelled() => Err(CopilotAuthError::Cancelled),
            result = async {
                let mut response = request.send().await.map_err(|_| CopilotAuthError::Network)?;
                let status = response.status();
                let mut bytes = Vec::new();
                while let Some(chunk) = response.chunk().await.map_err(|_| CopilotAuthError::Network)? {
                    if bytes.len().saturating_add(chunk.len()) > RESPONSE_LIMIT {
                        return Err(CopilotAuthError::InvalidResponse);
                    }
                    bytes.extend_from_slice(&chunk);
                }
                let value: Value = serde_json::from_slice(&bytes).map_err(|_| {
                    if status.is_success() { CopilotAuthError::InvalidResponse } else { CopilotAuthError::Http(status.as_u16()) }
                })?;
                if !status.is_success() && error_code(&value).is_none() {
                    return Err(CopilotAuthError::Http(status.as_u16()));
                }
                Ok(value)
            } => result,
        }
    }

    fn directory(&self) -> Result<PathBuf, CopilotAuthError> {
        let directory = self.root.join("auth");
        open_without_following_links(
            &directory,
            OpenTarget::Directory {
                create_mode: Some(PRIVATE_DIRECTORY_MODE),
            },
        )
        .map_err(|_| CopilotAuthError::Storage)?;
        verify_private_directory(&directory, "GitHub Copilot credential directory")
            .map_err(|_| CopilotAuthError::Storage)?;
        Ok(directory)
    }

    fn read(&self) -> Result<Option<StoredState>, CopilotAuthError> {
        let path = self.directory()?.join(CREDENTIAL_FILE);
        let file = match open_without_following_links(&path, OpenTarget::Entry) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(CopilotAuthError::Storage),
        };
        super::verify_private_file(&file).map_err(|_| CopilotAuthError::Storage)?;
        let mut bytes = Vec::new();
        file.take(MAX_CREDENTIAL_FILE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| CopilotAuthError::Storage)?;
        if bytes.len() > MAX_CREDENTIAL_FILE_BYTES {
            return Err(CopilotAuthError::Storage);
        }
        let state: StoredState =
            serde_json::from_slice(&bytes).map_err(|_| CopilotAuthError::Storage)?;
        if state.credentials.as_ref().is_some_and(|credential| {
            credential.access_token.is_empty()
                || credential
                    .refresh_token
                    .as_ref()
                    .is_some_and(String::is_empty)
                || credential
                    .session
                    .as_ref()
                    .is_some_and(|session| session.token.is_empty())
        }) {
            return Err(CopilotAuthError::Storage);
        }
        Ok(Some(state))
    }

    fn save(&self, state: &StoredState) -> Result<(), CopilotAuthError> {
        let path = self.directory()?.join(CREDENTIAL_FILE);
        self.read()?;
        replace_file_atomically(
            &path,
            &serde_json::to_vec(state).map_err(|_| CopilotAuthError::Storage)?,
            PRIVATE_FILE_MODE,
        )
        .map_err(|_| CopilotAuthError::Storage)
    }

    async fn lock(&self, cancel: &CancellationToken) -> Result<File, CopilotAuthError> {
        let path = self.directory()?.join(LOCK_FILE);
        let deadline = Instant::now() + LOCK_TIMEOUT;
        loop {
            if cancel.is_cancelled() {
                return Err(CopilotAuthError::Cancelled);
            }
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(PRIVATE_FILE_MODE)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&path)
                .map_err(|_| CopilotAuthError::Storage)?;
            super::verify_private_file(&file).map_err(|_| CopilotAuthError::Storage)?;
            match file.try_lock() {
                Ok(()) => return Ok(file),
                Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                    tokio::select! {
                        biased;
                        () = cancel.cancelled() => return Err(CopilotAuthError::Cancelled),
                        () = tokio::time::sleep(Duration::from_millis(50)) => {},
                    }
                }
                _ => return Err(CopilotAuthError::Storage),
            }
        }
    }
}

fn parse_credentials(
    payload: &Value,
    identity: Uuid,
) -> Result<CopilotCredentials, CopilotAuthError> {
    if payload["token_type"]
        .as_str()
        .is_some_and(|kind| !kind.eq_ignore_ascii_case("bearer"))
    {
        return Err(CopilotAuthError::InvalidResponse);
    }
    let now = unix_time_milliseconds();
    let deadline = |name: &str| {
        payload[name]
            .as_u64()
            .map(|seconds| now.saturating_add(seconds.saturating_mul(1000)))
    };
    let refresh_token = payload["refresh_token"]
        .as_str()
        .filter(|token| !token.is_empty())
        .map(str::to_owned);
    Ok(CopilotCredentials {
        access_token: field(payload, "access_token")?,
        refresh_token,
        expires_at_ms: deadline("expires_in"),
        refresh_expires_at_ms: deadline("refresh_token_expires_in"),
        identity,
        session: None,
    })
}

async fn read_json(
    request: reqwest::RequestBuilder,
) -> Result<(u16, Option<Value>), CopilotAuthError> {
    let mut response = request
        .send()
        .await
        .map_err(|_| CopilotAuthError::Network)?;
    let status = response.status().as_u16();
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| CopilotAuthError::Network)?
    {
        if bytes.len().saturating_add(chunk.len()) > RESPONSE_LIMIT {
            return Err(CopilotAuthError::InvalidResponse);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok((status, serde_json::from_slice(&bytes).ok()))
}

/// The token's `proxy-ep` names the account's plan host (`proxy.individual.githubcopilot.com`);
/// its API counterpart is `api.…`. The value is credential data, so only Copilot hosts count.
fn api_base_from_token(token: &str) -> Option<String> {
    let proxy = token
        .split(';')
        .find_map(|part| part.trim().strip_prefix("proxy-ep="))?;
    let host = proxy
        .strip_prefix("proxy.")
        .map_or_else(|| proxy.to_owned(), |rest| format!("api.{rest}"));
    let valid = host.ends_with(COPILOT_API_HOST_SUFFIX)
        && host.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b".-".contains(&byte)
        });
    valid.then(|| format!("https://{host}"))
}

fn authorization_error(code: &str) -> CopilotAuthError {
    match code {
        "access_denied" => CopilotAuthError::Denied,
        "expired_token" | "token_expired" => CopilotAuthError::Expired,
        "device_flow_disabled" => CopilotAuthError::DeviceFlowDisabled,
        "incorrect_client_credentials" => {
            CopilotAuthError::Rejected("incorrect_client_credentials")
        }
        "incorrect_device_code" => CopilotAuthError::Rejected("incorrect_device_code"),
        "unsupported_grant_type" => CopilotAuthError::Rejected("unsupported_grant_type"),
        _ => CopilotAuthError::InvalidResponse,
    }
}

fn field(payload: &Value, name: &str) -> Result<String, CopilotAuthError> {
    payload[name]
        .as_str()
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
        .ok_or(CopilotAuthError::InvalidResponse)
}

fn error_code(payload: &Value) -> Option<&str> {
    payload["error"].as_str()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;
    use tokio::{
        io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
        net::TcpListener,
    };

    /// Request line, form fields and the raw request head of each request.
    type Recorded = Vec<(String, HashMap<String, String>, String)>;

    /// Answers one request per connection with the given JSON replies (a `_status` field sets
    /// the HTTP status), recording each request line, form and head.
    async fn issuer(replies: Vec<Value>) -> (String, tokio::task::JoinHandle<Recorded>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let mut requests = Vec::new();
            for mut reply in replies {
                let status = reply
                    .as_object_mut()
                    .and_then(|object| object.remove("_status"))
                    .and_then(|status| status.as_u64())
                    .unwrap_or(200);
                let (socket, _) = listener.accept().await.unwrap();
                let mut reader = BufReader::new(socket);
                let mut headers = String::new();
                while !headers.ends_with("\r\n\r\n") {
                    assert_ne!(reader.read_line(&mut headers).await.unwrap(), 0);
                }
                let length = headers
                    .lines()
                    .find_map(|line| {
                        let (key, value) = line.split_once(':')?;
                        key.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                let mut bytes = vec![0; length];
                reader.read_exact(&mut bytes).await.unwrap();
                let form = reqwest::Url::parse(&format!(
                    "http://localhost/?{}",
                    String::from_utf8(bytes).unwrap()
                ))
                .unwrap()
                .query_pairs()
                .map(|(key, value)| (key.into_owned(), value.into_owned()))
                .collect();
                requests.push((
                    headers.lines().next().unwrap().to_owned(),
                    form,
                    headers.clone(),
                ));
                let body = reply.to_string();
                reader.into_inner().write_all(format!("HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            }
            requests
        });
        (origin, task)
    }

    fn device_reply() -> Value {
        json!({"device_code":"test-device-code","user_code":"ABCD-1234","verification_uri":"https://github.com/login/device","expires_in":900,"interval":0})
    }

    fn token(access: &str, refresh: &str, expires_in: u64) -> Value {
        json!({"access_token":access,"token_type":"bearer","scope":"","expires_in":expires_in,"refresh_token":refresh,"refresh_token_expires_in":15897600})
    }

    fn auth(directory: &tempfile::TempDir, origin: &str) -> CopilotAuth {
        CopilotAuth::new_for_loopback_test(directory.path().canonicalize().unwrap(), origin)
            .unwrap()
    }

    #[tokio::test]
    async fn device_flow_polls_through_pending_and_slow_down_then_saves_the_sign_in() {
        let directory = tempfile::tempdir().unwrap();
        let (origin, requests) = issuer(vec![
            device_reply(),
            json!({"error":"authorization_pending"}),
            json!({"error":"slow_down","interval":0}),
            token("test-access", "test-refresh", 28800),
        ])
        .await;
        let auth = auth(&directory, &origin);
        let cancel = CancellationToken::new();
        let login = auth.start_device_login(&cancel).await.unwrap();
        assert_eq!(login.user_code, "ABCD-1234");
        assert_eq!(login.verification_uri, "https://github.com/login/device");
        let credential = auth.finish_device_login(login, &cancel).await.unwrap();
        assert_eq!(credential.access_token, "test-access");
        assert!(auth.has_saved_login().unwrap());
        let resolved = auth.resolve_credentials(&cancel).await.unwrap();
        assert_eq!(resolved.identity, credential.identity);

        let requests = requests.await.unwrap();
        assert_eq!(requests[0].0, "POST /login/device/code HTTP/1.1");
        assert_eq!(
            requests[0].1,
            HashMap::from([
                ("client_id".into(), CLIENT_ID.into()),
                ("scope".into(), SCOPE.into())
            ])
        );
        for (line, form, _) in &requests[1..] {
            assert_eq!(line, "POST /login/oauth/access_token HTTP/1.1");
            assert_eq!(form["client_id"], CLIENT_ID);
            assert_eq!(form["device_code"], "test-device-code");
            assert_eq!(form["grant_type"], DEVICE_GRANT);
        }
        let file = directory.path().join("auth").join(CREDENTIAL_FILE);
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[tokio::test]
    async fn authorization_errors_map_to_their_reasons_and_save_nothing() {
        for (reply, expected) in [
            ("access_denied", "denied"),
            ("expired_token", "expired"),
            ("device_flow_disabled", "disabled"),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let (origin, _) = issuer(vec![device_reply(), json!({"error":reply})]).await;
            let auth = auth(&directory, &origin);
            let cancel = CancellationToken::new();
            let login = auth.start_device_login(&cancel).await.unwrap();
            let error = auth.finish_device_login(login, &cancel).await.unwrap_err();
            assert!(
                matches!(
                    (expected, &error),
                    ("denied", CopilotAuthError::Denied)
                        | ("expired", CopilotAuthError::Expired)
                        | ("disabled", CopilotAuthError::DeviceFlowDisabled)
                ),
                "{reply}: {error}"
            );
            assert!(!auth.has_saved_login().unwrap());
        }
    }

    #[tokio::test]
    async fn sign_out_or_cancel_during_polling_cannot_revive_the_sign_in() {
        for sign_out in [true, false] {
            let directory = tempfile::tempdir().unwrap();
            let (origin, _) = issuer(vec![
                device_reply(),
                token("late-access", "late-refresh", 28800),
            ])
            .await;
            let auth = auth(&directory, &origin);
            let cancel = CancellationToken::new();
            let login = auth.start_device_login(&cancel).await.unwrap();
            if sign_out {
                auth.logout(&cancel).await.unwrap();
            } else {
                cancel.cancel();
            }
            let result = auth.finish_device_login(login, &cancel).await;
            if sign_out {
                assert!(matches!(result, Err(CopilotAuthError::Superseded)));
            } else {
                assert!(matches!(result, Err(CopilotAuthError::Cancelled)));
            }
            assert!(!auth.has_saved_login().unwrap());
        }
    }

    async fn signed_in(
        directory: &tempfile::TempDir,
        mut later: Vec<Value>,
        expires_in: u64,
    ) -> (
        CopilotAuth,
        CopilotCredentials,
        tokio::task::JoinHandle<Recorded>,
    ) {
        let mut replies = vec![
            device_reply(),
            token("first-access", "first-refresh", expires_in),
        ];
        replies.append(&mut later);
        let (origin, requests) = issuer(replies).await;
        let auth = auth(directory, &origin);
        let cancel = CancellationToken::new();
        let login = auth.start_device_login(&cancel).await.unwrap();
        let credential = auth.finish_device_login(login, &cancel).await.unwrap();
        (auth, credential, requests)
    }

    #[tokio::test]
    async fn expiring_token_refreshes_once_rotates_the_refresh_token_and_keeps_identity() {
        let directory = tempfile::tempdir().unwrap();
        let (auth, credential, requests) = signed_in(
            &directory,
            vec![token("second-access", "second-refresh", 28800)],
            1,
        )
        .await;
        let cancel = CancellationToken::new();
        assert!(matches!(
            auth.resolve_access_with_refresh_gate(&cancel, || false)
                .await,
            Err(CopilotAuthError::BudgetExhausted)
        ));
        let (first, second) = tokio::join!(
            auth.resolve_credentials(&cancel),
            auth.resolve_credentials(&cancel)
        );
        for refreshed in [first.unwrap(), second.unwrap()] {
            assert_eq!(refreshed.access_token, "second-access");
            assert_eq!(refreshed.identity, credential.identity);
        }
        let requests = requests.await.unwrap();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[2].1["grant_type"], "refresh_token");
        assert_eq!(requests[2].1["refresh_token"], "first-refresh");
        assert!(!requests[2].1.contains_key("client_secret"));
        let saved = auth.read().unwrap().unwrap().credentials.unwrap();
        assert_eq!(saved.refresh_token.as_deref(), Some("second-refresh"));
    }

    #[tokio::test]
    async fn cancelling_during_refresh_still_saves_the_rotated_refresh_token() {
        let directory = tempfile::tempdir().unwrap();
        let (auth, credential, _) = signed_in(&directory, Vec::new(), 1).await;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let (received, request_seen) = tokio::sync::oneshot::channel();
        let (release, reply_allowed) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(socket);
            let mut headers = String::new();
            while !headers.ends_with("\r\n\r\n") {
                assert_ne!(reader.read_line(&mut headers).await.unwrap(), 0);
            }
            received.send(()).unwrap();
            reply_allowed.await.unwrap();
            let body = token("rotated-access", "rotated-refresh", 28800).to_string();
            let _ = reader.into_inner().write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await;
        });
        let auth = CopilotAuth { origin, ..auth };
        let cancel = CancellationToken::new();
        let refresh = tokio::spawn({
            let auth = auth.clone();
            let cancel = cancel.clone();
            async move { auth.resolve_credentials(&cancel).await }
        });
        request_seen.await.unwrap();
        cancel.cancel();
        release.send(()).unwrap();
        assert!(matches!(
            refresh.await.unwrap(),
            Err(CopilotAuthError::Cancelled)
        ));
        let saved = auth.read().unwrap().unwrap().credentials.unwrap();
        assert_eq!(saved.access_token, "rotated-access");
        assert_eq!(saved.refresh_token.as_deref(), Some("rotated-refresh"));
        assert_eq!(saved.identity, credential.identity);
    }

    #[tokio::test]
    async fn refresh_reply_without_a_new_refresh_token_keeps_the_previous_one() {
        let directory = tempfile::tempdir().unwrap();
        let (auth, _, _) = signed_in(
            &directory,
            vec![json!({"access_token":"second-access","token_type":"bearer","expires_in":28800})],
            1,
        )
        .await;
        auth.resolve_credentials(&CancellationToken::new())
            .await
            .unwrap();
        let saved = auth.read().unwrap().unwrap().credentials.unwrap();
        assert_eq!(saved.access_token, "second-access");
        assert_eq!(saved.refresh_token.as_deref(), Some("first-refresh"));
    }

    #[tokio::test]
    async fn copilot_token_exchange_is_cached_and_names_the_account_host() {
        let directory = tempfile::tempdir().unwrap();
        let expires_at = unix_time_milliseconds() / 1000 + 1800;
        let (auth, credential, requests) = signed_in(
            &directory,
            vec![json!({"token":"tid=1;exp=2;proxy-ep=proxy.business.githubcopilot.com;sku=x","expires_at":expires_at})],
            28800,
        )
        .await;
        let cancel = CancellationToken::new();
        let first = auth.resolve_access(&cancel).await.unwrap();
        let second = auth.resolve_access(&cancel).await.unwrap();
        for access in [&first, &second] {
            assert!(access.token.starts_with("tid=1;"));
            assert_eq!(
                access.api_base.as_deref(),
                Some("https://api.business.githubcopilot.com")
            );
            assert_eq!(access.identity, credential.identity);
        }
        let requests = requests.await.unwrap();
        assert_eq!(requests.len(), 3);
        let (line, _, head) = &requests[2];
        assert_eq!(line, "GET /copilot_internal/v2/token HTTP/1.1");
        let head = head.to_ascii_lowercase();
        assert!(head.contains("authorization: bearer first-access"));
        for (name, value) in CLIENT_HEADERS {
            assert!(
                head.contains(&format!("{name}: {}", value.to_ascii_lowercase())),
                "{name}"
            );
        }
    }

    #[tokio::test]
    async fn exchange_refusal_and_untrusted_proxy_hosts_are_not_used() {
        let directory = tempfile::tempdir().unwrap();
        let (auth, _, _) = signed_in(
            &directory,
            vec![
                json!({"_status":404,"message":"Not Found"}),
                json!({"token":"tid=1;proxy-ep=proxy.attacker.example;","expires_at":unix_time_milliseconds() / 1000 + 1800}),
            ],
            28800,
        )
        .await;
        let cancel = CancellationToken::new();
        assert!(matches!(
            auth.resolve_access(&cancel).await,
            Err(CopilotAuthError::NoCopilotAccess)
        ));
        assert!(auth.has_saved_login().unwrap());
        assert_eq!(auth.resolve_access(&cancel).await.unwrap().api_base, None);
    }

    #[tokio::test]
    async fn rejected_refresh_token_clears_the_sign_in() {
        let directory = tempfile::tempdir().unwrap();
        let (auth, _, _) =
            signed_in(&directory, vec![json!({"error":"bad_refresh_token"})], 1).await;
        let cancel = CancellationToken::new();
        assert!(matches!(
            auth.resolve_credentials(&cancel).await,
            Err(CopilotAuthError::RefreshFailed)
        ));
        assert!(!auth.has_saved_login().unwrap());
        assert!(matches!(
            auth.resolve_credentials(&cancel).await,
            Err(CopilotAuthError::NotLoggedIn)
        ));
    }

    #[tokio::test]
    async fn non_expiring_tokens_are_used_without_refresh() {
        let directory = tempfile::tempdir().unwrap();
        let (origin, requests) = issuer(vec![
            device_reply(),
            json!({"access_token":"forever-access","token_type":"bearer","scope":""}),
        ])
        .await;
        let auth = auth(&directory, &origin);
        let cancel = CancellationToken::new();
        let login = auth.start_device_login(&cancel).await.unwrap();
        auth.finish_device_login(login, &cancel).await.unwrap();
        let resolved = auth.resolve_credentials(&cancel).await.unwrap();
        assert_eq!(resolved.access_token, "forever-access");
        assert_eq!(resolved.expires_at_ms, None);
        assert_eq!(requests.await.unwrap().len(), 2);
    }
}

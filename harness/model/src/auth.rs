//! Local subscription credentials.

pub mod copilot;
pub mod device_subscriptions;
pub mod google;
pub mod minimax;
mod provider_store;
#[cfg(test)]
pub(crate) mod provider_test_support;

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use reqwest::{Client, StatusCode, redirect::Policy};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use tickloop_harness_base::network::{
    HttpPolicy, NetworkDiagnostic, NetworkRoute, build_http_client,
};
use tickloop_harness_base::oauth_callback::{
    CallbackError, CallbackListener, CallbackPages, CallbackPaste, PendingCallback,
};
use tickloop_harness_base::private_fs::{
    OpenTarget, PRIVATE_DIRECTORY_MODE, PRIVATE_FILE_MODE, current_uid,
    open_without_following_links, replace_file_atomically, unix_time_milliseconds,
    verify_private_directory,
};
use tickloop_harness_protocol::ProviderCredentialScope;

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const PRODUCTION_ORIGIN: &str = "https://auth.openai.com";
const CREDENTIAL_FILE: &str = "openai-codex-credentials.json";
const LOCK_FILE: &str = "openai-codex.lock";
const LOGIN_GENERATION_STORE: &str = "openai-codex-generation";
const DEVICE_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
const REFRESH_WINDOW_MS: u64 = 5 * 60 * 1000;
const LOCK_RETRY: Duration = Duration::from_millis(50);
const LOCK_TIMEOUT: Duration = Duration::from_secs(15);
const MIN_POLL_INTERVAL: Duration = Duration::from_secs(1);
const SLOW_DOWN_INCREMENT: Duration = Duration::from_secs(5);
const MAX_OAUTH_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_CREDENTIAL_FILE_BYTES: usize = 1024 * 1024;
const BROWSER_CALLBACK_PORT: u16 = 1455;
const BROWSER_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const CALLBACK_PATH: &str = "/auth/callback";
const BROWSER_SCOPE: &str = "openid profile email offline_access";
const BROWSER_CALLBACK_PAGES: CallbackPages = CallbackPages {
    received: "Authorization code received. Return to Tickloop to finish login; the terminal will confirm whether it succeeded.",
    state_mismatch: "This callback belongs to a different sign-in flow. Return to the current browser tab or start again from /provider.",
    invalid_url: "Callback URL is invalid or missing an authorization code. If this browser tab is old, start again from /provider.",
    invalid_request: "Could not read the browser callback. Return to the current sign-in flow or start again from /provider.",
    denied: "Authorization was denied.",
    failed: "Authorization failed.",
};

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("{0}")]
    Provider(String),
    #[error("{0}")]
    MiniMax(String),
    #[error("{0}")]
    Copilot(String),
    #[error("OpenAI Codex login was cancelled")]
    Cancelled,
    #[error("This Codex sign-in was superseded or signed out; start again from Connection Details")]
    SupersededLogin,
    #[error("OpenAI Codex authorization timed out; retry from /provider")]
    TimedOut,
    #[error("OpenAI Codex is not logged in; use /provider")]
    NotLoggedIn,
    #[error(
        "OpenAI Codex device login is unavailable on this server; choose browser sign-in from /provider"
    )]
    DeviceUnavailable,
    #[error("OpenAI Codex authorization was denied")]
    AuthorizationDenied,
    #[error("OpenAI Codex authorization expired; retry from /provider")]
    AuthorizationExpired,
    #[error("OpenAI Codex OAuth {operation} failed with HTTP {status}")]
    HttpStatus {
        operation: &'static str,
        status: u16,
    },
    #[error("OpenAI Codex OAuth {operation} request failed: {reason}")]
    Network {
        operation: &'static str,
        reason: &'static str,
    },
    #[error("OpenAI Codex OAuth {operation} request timed out")]
    NetworkTimeout { operation: &'static str },
    #[error("OpenAI Codex OAuth {0} response is invalid")]
    InvalidResponse(&'static str),
    #[error("OpenAI Codex credential storage failed: {0}")]
    Storage(&'static str),
    #[error("OpenAI Codex credential file is invalid or unsafe")]
    InvalidCredentialFile,
    #[error(
        "OpenAI Codex login needs to be renewed; select the Codex Provider in /model and press r"
    )]
    RefreshFailed,
    #[error("OpenAI Codex credential refresh exceeded the request attempt budget")]
    RefreshBudgetExhausted,
    #[error("test OAuth endpoint must be an HTTP loopback origin")]
    InvalidTestEndpoint,
    #[error(
        "OpenAI Codex browser callback port is unavailable; press d on the Codex Provider in /model"
    )]
    CallbackPortUnavailable,
    #[error(
        "OpenAI Codex browser callback URL is invalid or missing an authorization code; paste the full redirect URL from the current Provider sign-in flow"
    )]
    InvalidCallback,
    #[error(
        "OpenAI Codex browser callback belongs to a different sign-in flow; use the current Provider browser tab or restart from /provider"
    )]
    StateMismatch,
    #[error("OpenAI Codex browser login has ended")]
    BrowserLoginEnded,
    #[error("OpenAI Codex browser authorization failed; retry from /provider")]
    BrowserAuthorizationFailed,
}

#[derive(Clone)]
pub struct Credentials {
    pub access_token: String,
    pub account_id: String,
    pub expires_at_ms: u64,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Credentials")
            .field("access_token", &"[redacted]")
            .field("account_id", &"[redacted]")
            .field("expires_at_ms", &self.expires_at_ms)
            .finish()
    }
}

pub struct DeviceLogin {
    pub user_code: String,
    pub verification_uri: String,
    pub interval_seconds: u64,
    pub expires_in_seconds: u64,
    device_auth_id: String,
    generation: Uuid,
    started_at: Instant,
}

pub struct BrowserLogin {
    generation: Uuid,
    pub authorization_url: String,
    pub callback_uri: String,
    verifier: String,
    callback: PendingCallback,
}

impl BrowserLogin {
    pub fn manual_callback(&self) -> BrowserCallback {
        BrowserCallback {
            paste: self.callback.paste(),
        }
    }
}

impl fmt::Debug for BrowserLogin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BrowserLogin")
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub struct BrowserCallback {
    paste: CallbackPaste,
}

impl BrowserCallback {
    pub fn submit(&self, pasted_redirect_url: &str) -> Result<(), AuthError> {
        self.paste
            .submit(pasted_redirect_url)
            .map_err(browser_callback_error)
    }
}

impl fmt::Debug for BrowserCallback {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BrowserCallback")
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for DeviceLogin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DeviceLogin")
            .field("user_code", &"[redacted]")
            .field("verification_uri", &self.verification_uri)
            .field("interval_seconds", &self.interval_seconds)
            .field("expires_in_seconds", &self.expires_in_seconds)
            .finish_non_exhaustive()
    }
}

#[derive(Serialize, Deserialize)]
struct StoredCredential {
    access_token: String,
    refresh_token: String,
    account_id: String,
    expires_at_ms: u64,
}

impl StoredCredential {
    fn public(&self) -> Credentials {
        Credentials {
            access_token: self.access_token.clone(),
            account_id: self.account_id.clone(),
            expires_at_ms: self.expires_at_ms,
        }
    }

    fn expires_soon(&self) -> bool {
        unix_time_milliseconds().saturating_add(REFRESH_WINDOW_MS) >= self.expires_at_ms
    }
}

#[derive(Clone)]
pub struct CodexAuth {
    state_dir: PathBuf,
    oauth_origin: String,
    callback_port: u16,
    http: Client,
    network_route: NetworkRoute,
}

impl CodexAuth {
    pub fn new(state_dir: impl Into<PathBuf>) -> Self {
        let origin = reqwest::Url::parse(PRODUCTION_ORIGIN).expect("fixed OAuth origin");
        let (http, network_route) = oauth_http_client(&origin);
        Self {
            state_dir: state_dir.into(),
            oauth_origin: PRODUCTION_ORIGIN.to_owned(),
            callback_port: BROWSER_CALLBACK_PORT,
            http,
            network_route,
        }
    }

    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub fn new_for_loopback_test(
        state_dir: impl Into<PathBuf>,
        origin: &str,
    ) -> Result<Self, AuthError> {
        Self::new_for_loopback_test_with_callback_port(state_dir, origin, 0)
    }

    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub fn new_for_loopback_test_with_callback_port(
        state_dir: impl Into<PathBuf>,
        origin: &str,
        callback_port: u16,
    ) -> Result<Self, AuthError> {
        let url = reqwest::Url::parse(origin).map_err(|_| AuthError::InvalidTestEndpoint)?;
        if url.scheme() != "http"
            || !matches!(url.host_str(), Some("127.0.0.1" | "::1"))
            || url.port().is_none()
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(AuthError::InvalidTestEndpoint);
        }
        let (http, network_route) = oauth_http_client(&url);
        Ok(Self {
            state_dir: state_dir.into(),
            oauth_origin: origin.trim_end_matches('/').to_owned(),
            callback_port,
            http,
            network_route,
        })
    }

    pub async fn start_device_login(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<DeviceLogin, AuthError> {
        let generation = self.begin_login(cancellation).await?;
        let response = self
            .post_json(
                "/api/accounts/deviceauth/usercode",
                json!({ "client_id": CLIENT_ID }),
                cancellation,
                "device authorization",
            )
            .await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Err(AuthError::DeviceUnavailable);
        }
        if !response.status().is_success() {
            return Err(http_error("device authorization", response.status()));
        }
        let value = read_json(response, cancellation, "device authorization").await?;
        let device_auth_id = required_string(&value, "device_auth_id", "device authorization")?;
        let user_code = required_string(&value, "user_code", "device authorization")?;
        let interval_seconds = value
            .get("interval")
            .and_then(|interval| {
                interval
                    .as_u64()
                    .or_else(|| interval.as_str()?.trim().parse::<u64>().ok())
            })
            .ok_or(AuthError::InvalidResponse("device authorization"))?
            .min(DEVICE_TIMEOUT.as_secs());
        Ok(DeviceLogin {
            user_code,
            verification_uri: format!("{}/codex/device", self.oauth_origin),
            interval_seconds,
            expires_in_seconds: DEVICE_TIMEOUT.as_secs(),
            device_auth_id,
            generation,
            started_at: Instant::now(),
        })
    }

    pub async fn start_browser_login(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<BrowserLogin, AuthError> {
        if cancellation.is_cancelled() {
            return Err(AuthError::Cancelled);
        }
        let generation = self.begin_login(cancellation).await?;
        let listener = CallbackListener::bind(self.callback_port, CALLBACK_PATH)
            .await
            .map_err(browser_callback_error)?;
        if cancellation.is_cancelled() {
            return Err(AuthError::Cancelled);
        }
        let callback_uri = listener.redirect_uri().to_owned();
        let verifier = new_pkce_verifier();
        let challenge = encode_base64url(&Sha256::digest(verifier.as_bytes()));
        let state = tickloop_ids::random_token();
        let mut authorize_url =
            reqwest::Url::parse(&format!("{}/oauth/authorize", self.oauth_origin))
                .expect("trusted OAuth origin");
        authorize_url
            .query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", CLIENT_ID)
            .append_pair("redirect_uri", &callback_uri)
            .append_pair("scope", BROWSER_SCOPE)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", &state)
            .append_pair("id_token_add_organizations", "true")
            .append_pair("codex_cli_simplified_flow", "true")
            .append_pair("originator", "tickloop");
        Ok(BrowserLogin {
            generation,
            authorization_url: authorize_url.into(),
            callback_uri,
            verifier,
            callback: listener.receive(state, BROWSER_CALLBACK_PAGES, BROWSER_TIMEOUT),
        })
    }

    pub async fn finish_browser_login(
        &self,
        mut login: BrowserLogin,
        cancellation: &CancellationToken,
    ) -> Result<Credentials, AuthError> {
        let response = login
            .callback
            .wait(cancellation)
            .await
            .map_err(browser_callback_error)?;
        let credential = self
            .exchange_code(
                &response.code,
                &login.verifier,
                &login.callback_uri,
                cancellation,
            )
            .await?;
        let _lock = self.lock(cancellation).await?;
        if cancellation.is_cancelled() {
            return Err(AuthError::Cancelled);
        }
        let _generation_lock = self.lock_login_generation(cancellation).await?;
        if cancellation.is_cancelled() {
            return Err(AuthError::Cancelled);
        }
        self.ensure_current_login(login.generation)?;
        self.save(&credential)?;
        Ok(credential.public())
    }

    pub async fn finish_device_login(
        &self,
        device: DeviceLogin,
        cancellation: &CancellationToken,
    ) -> Result<Credentials, AuthError> {
        let deadline = device.started_at + DEVICE_TIMEOUT;
        let mut interval = Duration::from_secs(device.interval_seconds).max(MIN_POLL_INTERVAL);
        loop {
            if Instant::now() >= deadline {
                return Err(AuthError::TimedOut);
            }
            let response = self
                .post_json(
                    "/api/accounts/deviceauth/token",
                    json!({
                        "device_auth_id": device.device_auth_id,
                        "user_code": device.user_code,
                    }),
                    cancellation,
                    "device polling",
                )
                .await?;
            if Instant::now() >= deadline {
                return Err(AuthError::TimedOut);
            }
            if response.status().is_success() {
                let value = read_json(response, cancellation, "device polling").await?;
                let code = required_string(&value, "authorization_code", "device polling")?;
                let verifier = required_string(&value, "code_verifier", "device polling")?;
                let credential = self
                    .exchange_code(
                        &code,
                        &verifier,
                        "https://auth.openai.com/deviceauth/callback",
                        cancellation,
                    )
                    .await?;
                let _lock = self.lock(cancellation).await?;
                if cancellation.is_cancelled() {
                    return Err(AuthError::Cancelled);
                }
                let _generation_lock = self.lock_login_generation(cancellation).await?;
                if cancellation.is_cancelled() {
                    return Err(AuthError::Cancelled);
                }
                self.ensure_current_login(device.generation)?;
                self.save(&credential)?;
                return Ok(credential.public());
            }
            if response.status() != StatusCode::FORBIDDEN
                && response.status() != StatusCode::NOT_FOUND
            {
                let status = response.status();
                let error = read_json(response, cancellation, "device polling")
                    .await
                    .ok();
                match oauth_error_code(error.as_ref()) {
                    Some("deviceauth_authorization_pending") => {}
                    Some("slow_down") => interval = interval.saturating_add(SLOW_DOWN_INCREMENT),
                    Some("access_denied" | "authorization_declined") => {
                        return Err(AuthError::AuthorizationDenied);
                    }
                    Some("expired_token" | "deviceauth_expired") => {
                        return Err(AuthError::AuthorizationExpired);
                    }
                    _ => return Err(http_error("device polling", status)),
                }
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(AuthError::TimedOut);
            }
            tokio::select! {
                () = cancellation.cancelled() => return Err(AuthError::Cancelled),
                () = tokio::time::sleep(interval.min(remaining)) => {}
            }
        }
    }

    pub fn has_saved_login(&self) -> Result<bool, AuthError> {
        match std::fs::symlink_metadata(self.credential_path()) {
            Ok(_) => self.read().map(|credentials| credentials.is_some()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(_) => Err(AuthError::InvalidCredentialFile),
        }
    }

    pub async fn resolve_credentials(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<Credentials, AuthError> {
        self.resolve_credentials_with_refresh_gate(cancellation, || true)
            .await
    }

    pub async fn resolve_credentials_with_refresh_gate<F>(
        &self,
        cancellation: &CancellationToken,
        refresh_gate: F,
    ) -> Result<Credentials, AuthError>
    where
        F: FnOnce() -> bool,
    {
        if cancellation.is_cancelled() {
            return Err(AuthError::Cancelled);
        }
        let stored = self.read()?.ok_or(AuthError::NotLoggedIn)?;
        if !stored.expires_soon() {
            return Ok(stored.public());
        }
        let _lock = self.lock(cancellation).await?;
        let current = self.read()?.ok_or(AuthError::NotLoggedIn)?;
        if !current.expires_soon() {
            return Ok(current.public());
        }
        if cancellation.is_cancelled() {
            return Err(AuthError::Cancelled);
        }
        if !refresh_gate() {
            return Err(AuthError::RefreshBudgetExhausted);
        }
        let refreshed = self.refresh(&current.refresh_token, cancellation).await?;
        if cancellation.is_cancelled() {
            return Err(AuthError::Cancelled);
        }
        self.save(&refreshed)?;
        Ok(refreshed.public())
    }

    pub async fn logout(&self, cancellation: &CancellationToken) -> Result<(), AuthError> {
        let _lock = self.lock(cancellation).await?;
        if cancellation.is_cancelled() {
            return Err(AuthError::Cancelled);
        }
        let _generation_lock = self.lock_login_generation(cancellation).await?;
        if cancellation.is_cancelled() {
            return Err(AuthError::Cancelled);
        }
        self.login_generation_store()
            .save(&tickloop_ids::generate())
            .map_err(|_| AuthError::Storage("invalidate login generation"))?;
        let path = self.credential_path();
        match self.open_credential_file()? {
            Some(_) => std::fs::remove_file(path).map_err(|_| AuthError::Storage("logout")),
            None => Ok(()),
        }
    }

    fn login_generation_store(&self) -> provider_store::ProviderStore {
        provider_store::ProviderStore::new(&self.state_dir, LOGIN_GENERATION_STORE)
    }

    async fn begin_login(&self, cancellation: &CancellationToken) -> Result<Uuid, AuthError> {
        if cancellation.is_cancelled() {
            return Err(AuthError::Cancelled);
        }
        let _lock = self.lock_login_generation(cancellation).await?;
        if cancellation.is_cancelled() {
            return Err(AuthError::Cancelled);
        }
        let generation = tickloop_ids::generate();
        self.login_generation_store()
            .save(&generation)
            .map_err(|_| AuthError::Storage("start login generation"))?;
        Ok(generation)
    }

    async fn lock_login_generation(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<File, AuthError> {
        self.directory()?;
        self.login_generation_store()
            .lock(cancellation)
            .await
            .map_err(|error| match error {
                crate::ModelError::Cancelled => AuthError::Cancelled,
                _ => AuthError::Storage("lock login generation"),
            })
    }

    fn ensure_current_login(&self, generation: Uuid) -> Result<(), AuthError> {
        let current = self
            .login_generation_store()
            .read::<Uuid>()
            .map_err(|_| AuthError::Storage("read login generation"))?;
        if current != Some(generation) {
            return Err(AuthError::SupersededLogin);
        }
        Ok(())
    }

    async fn exchange_code(
        &self,
        code: &str,
        verifier: &str,
        redirect_uri: &str,
        cancellation: &CancellationToken,
    ) -> Result<StoredCredential, AuthError> {
        self.token_request(
            &[
                ("grant_type", "authorization_code"),
                ("client_id", CLIENT_ID),
                ("code", code),
                ("code_verifier", verifier),
                ("redirect_uri", redirect_uri),
            ],
            cancellation,
            "code exchange",
        )
        .await
    }

    async fn refresh(
        &self,
        refresh_token: &str,
        cancellation: &CancellationToken,
    ) -> Result<StoredCredential, AuthError> {
        self.token_request(
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token),
                ("client_id", CLIENT_ID),
            ],
            cancellation,
            "refresh",
        )
        .await
        .map_err(|error| match error {
            AuthError::HttpStatus {
                status: 400 | 401, ..
            } => AuthError::RefreshFailed,
            other => other,
        })
    }

    async fn token_request(
        &self,
        form: &[(&str, &str)],
        cancellation: &CancellationToken,
        operation: &'static str,
    ) -> Result<StoredCredential, AuthError> {
        let url = reqwest::Url::parse(&format!("{}/oauth/token", self.oauth_origin))
            .expect("validated OAuth origin and fixed path");
        let request = self.http.post(url.clone()).form(form);
        let started = Instant::now();
        let response = tokio::select! {
            () = cancellation.cancelled() => return Err(AuthError::Cancelled),
            response = request.send() => response,
        };
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                NetworkDiagnostic::new(
                    operation,
                    &url,
                    self.network_route,
                    None,
                    started.elapsed(),
                )
                .emit();
                return Err(network_error(operation, &error));
            }
        };
        if !response.status().is_success() {
            NetworkDiagnostic::new(
                operation,
                &url,
                self.network_route,
                Some(response.status().as_u16()),
                started.elapsed(),
            )
            .emit();
            return Err(http_error(operation, response.status()));
        }
        let value = read_json(response, cancellation, operation).await?;
        let access_token = required_string(&value, "access_token", operation)?;
        let refresh_token = required_string(&value, "refresh_token", operation)?;
        let expires_in = value
            .get("expires_in")
            .and_then(Value::as_u64)
            .filter(|seconds| *seconds > 0)
            .ok_or(AuthError::InvalidResponse(operation))?;
        let account_id =
            account_id_from_jwt(&access_token).ok_or(AuthError::InvalidResponse(operation))?;
        Ok(StoredCredential {
            access_token,
            refresh_token,
            account_id,
            expires_at_ms: unix_time_milliseconds().saturating_add(expires_in.saturating_mul(1000)),
        })
    }

    async fn post_json(
        &self,
        path: &str,
        body: Value,
        cancellation: &CancellationToken,
        operation: &'static str,
    ) -> Result<reqwest::Response, AuthError> {
        let url = reqwest::Url::parse(&format!("{}{path}", self.oauth_origin))
            .expect("validated OAuth origin and fixed path");
        let request = self.http.post(url.clone()).json(&body);
        let started = Instant::now();
        let response = tokio::select! {
            () = cancellation.cancelled() => Err(AuthError::Cancelled),
            response = request.send() => response.map_err(|error| network_error(operation, &error)),
        };
        if !response
            .as_ref()
            .is_ok_and(|response| response.status().is_success())
        {
            NetworkDiagnostic::new(
                operation,
                &url,
                self.network_route,
                response
                    .as_ref()
                    .ok()
                    .map(|response| response.status().as_u16()),
                started.elapsed(),
            )
            .emit();
        }
        response
    }

    fn directory(&self) -> Result<PathBuf, AuthError> {
        let path = self.state_dir.join("auth");
        open_without_following_links(
            &path,
            OpenTarget::Directory {
                create_mode: Some(PRIVATE_DIRECTORY_MODE),
            },
        )
        .map_err(|_| AuthError::Storage("open private directory"))?;
        verify_private_directory(&path, "Codex credential directory")
            .map_err(|_| AuthError::Storage("unsafe private directory"))?;
        Ok(path)
    }

    fn credential_path(&self) -> PathBuf {
        self.state_dir.join("auth").join(CREDENTIAL_FILE)
    }

    fn read(&self) -> Result<Option<StoredCredential>, AuthError> {
        let Some(file) = self.open_credential_file()? else {
            return Ok(None);
        };
        let mut bytes = Vec::new();
        file.take(MAX_CREDENTIAL_FILE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| AuthError::InvalidCredentialFile)?;
        if bytes.len() > MAX_CREDENTIAL_FILE_BYTES {
            return Err(AuthError::InvalidCredentialFile);
        }
        let credential: StoredCredential =
            serde_json::from_slice(&bytes).map_err(|_| AuthError::InvalidCredentialFile)?;
        if credential.access_token.is_empty()
            || credential.refresh_token.is_empty()
            || credential.account_id.is_empty()
            || credential.expires_at_ms == 0
        {
            return Err(AuthError::InvalidCredentialFile);
        }
        Ok(Some(credential))
    }

    fn save(&self, credential: &StoredCredential) -> Result<(), AuthError> {
        self.directory()?;
        let path = self.credential_path();
        self.open_credential_file()?;
        let bytes = serde_json::to_vec(credential).map_err(|_| AuthError::Storage("encode"))?;
        replace_file_atomically(&path, &bytes, PRIVATE_FILE_MODE)
            .map_err(|_| AuthError::Storage("save"))
    }

    fn open_credential_file(&self) -> Result<Option<File>, AuthError> {
        self.directory()?;
        let path = self.credential_path();
        let file = match open_without_following_links(&path, OpenTarget::Entry) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(AuthError::InvalidCredentialFile),
        };
        verify_private_file(&file)?;
        Ok(Some(file))
    }

    async fn lock(&self, cancellation: &CancellationToken) -> Result<File, AuthError> {
        let directory = self.directory()?;
        let path = directory.join(LOCK_FILE);
        let deadline = Instant::now() + LOCK_TIMEOUT;
        loop {
            if cancellation.is_cancelled() {
                return Err(AuthError::Cancelled);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(AuthError::Storage("lock timed out"));
            }
            let file = match OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(PRIVATE_FILE_MODE)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&path)
            {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => OpenOptions::new()
                    .read(true)
                    .write(true)
                    .custom_flags(libc::O_NOFOLLOW)
                    .open(&path)
                    .map_err(|_| AuthError::Storage("open lock"))?,
                Err(_) => return Err(AuthError::Storage("create lock")),
            };
            verify_private_file(&file)?;
            match file.try_lock() {
                Ok(()) => return Ok(file),
                Err(std::fs::TryLockError::WouldBlock) => {
                    tokio::select! {
                        () = cancellation.cancelled() => return Err(AuthError::Cancelled),
                        () = tokio::time::sleep(LOCK_RETRY.min(remaining)) => {}
                    }
                }
                Err(_) => return Err(AuthError::Storage("acquire lock")),
            }
        }
    }
}

fn oauth_http_client(target: &reqwest::Url) -> (Client, NetworkRoute) {
    let mut policy = HttpPolicy::new(Policy::none());
    policy.timeout = Some(HTTP_TIMEOUT);
    build_http_client(target, policy).expect("valid HTTP client")
}

fn new_pkce_verifier() -> String {
    let mut random = [0_u8; 32];
    random[..16].copy_from_slice(&tickloop_ids::random_u128().to_be_bytes());
    random[16..].copy_from_slice(&tickloop_ids::random_u128().to_be_bytes());
    encode_base64url(&random)
}

fn encode_base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for group in bytes.chunks(3) {
        let first = group[0] >> 2;
        let second = ((group[0] & 0b11) << 4) | (group.get(1).copied().unwrap_or(0) >> 4);
        encoded.push(ALPHABET[usize::from(first)] as char);
        encoded.push(ALPHABET[usize::from(second)] as char);
        if let Some(&middle) = group.get(1) {
            let third = ((middle & 0b1111) << 2) | (group.get(2).copied().unwrap_or(0) >> 6);
            encoded.push(ALPHABET[usize::from(third)] as char);
        }
        if let Some(&last) = group.get(2) {
            encoded.push(ALPHABET[usize::from(last & 0b11_1111)] as char);
        }
    }
    encoded
}

fn browser_callback_error(error: CallbackError) -> AuthError {
    match error {
        CallbackError::PortUnavailable(_) => AuthError::CallbackPortUnavailable,
        CallbackError::InvalidCallback => AuthError::InvalidCallback,
        CallbackError::StateMismatch => AuthError::StateMismatch,
        CallbackError::Denied => AuthError::AuthorizationDenied,
        CallbackError::AuthorizationFailed => AuthError::BrowserAuthorizationFailed,
        CallbackError::Ended => AuthError::BrowserLoginEnded,
        CallbackError::Cancelled => AuthError::Cancelled,
        CallbackError::TimedOut => AuthError::TimedOut,
    }
}

fn verify_private_file(file: &File) -> Result<(), AuthError> {
    let metadata = file
        .metadata()
        .map_err(|_| AuthError::InvalidCredentialFile)?;
    if !metadata.is_file()
        || metadata.uid() != current_uid()
        || metadata.mode() & 0o077 != 0
        || metadata.nlink() != 1
    {
        return Err(AuthError::InvalidCredentialFile);
    }
    Ok(())
}

async fn read_json(
    mut response: reqwest::Response,
    cancellation: &CancellationToken,
    operation: &'static str,
) -> Result<Value, AuthError> {
    let mut bytes = Vec::new();
    loop {
        let chunk = tokio::select! {
            () = cancellation.cancelled() => return Err(AuthError::Cancelled),
            chunk = response.chunk() => chunk.map_err(|_| AuthError::InvalidResponse(operation))?,
        };
        let Some(chunk) = chunk else {
            break;
        };
        if bytes.len().saturating_add(chunk.len()) > MAX_OAUTH_RESPONSE_BYTES {
            return Err(AuthError::InvalidResponse(operation));
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| AuthError::InvalidResponse(operation))
}

fn required_string(value: &Value, key: &str, operation: &'static str) -> Result<String, AuthError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
        .ok_or(AuthError::InvalidResponse(operation))
}

fn oauth_error_code(value: Option<&Value>) -> Option<&str> {
    let error = value?.get("error")?;
    error.as_str().or_else(|| error.get("code")?.as_str())
}

fn http_error(operation: &'static str, status: StatusCode) -> AuthError {
    AuthError::HttpStatus {
        operation,
        status: status.as_u16(),
    }
}

fn network_error(operation: &'static str, error: &reqwest::Error) -> AuthError {
    if error.is_timeout() {
        return AuthError::NetworkTimeout { operation };
    }
    let reason = if error.is_connect() {
        "connection failed before receiving an HTTP response (DNS, TLS, or proxy connection)"
    } else {
        "transport failed before receiving an HTTP response"
    };
    AuthError::Network { operation, reason }
}

fn account_id_from_jwt(access_token: &str) -> Option<String> {
    let payload = access_token.split('.').nth(1)?;
    let bytes = decode_base64url(payload)?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    value
        .get("https://api.openai.com/auth")?
        .get("chatgpt_account_id")?
        .as_str()
        .filter(|account_id| !account_id.is_empty())
        .map(str::to_owned)
}

fn decode_base64url(value: &str) -> Option<Vec<u8>> {
    let mut output = Vec::with_capacity(value.len() * 3 / 4);
    let mut buffer = 0_u32;
    let mut bits = 0_u8;
    for byte in value.bytes() {
        if byte == b'=' {
            break;
        }
        let digit = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return None,
        };
        buffer = (buffer << 6) | u32::from(digit);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    Some(output)
}

/// The provider credentials visible to this process: the names of usable environment variables,
/// and under `config_root` the Google service account, so a View can explain which providers
/// can authenticate.
pub fn current_provider_credential_scope(config_root: Option<PathBuf>) -> ProviderCredentialScope {
    let environment_keys = std::env::vars_os()
        .filter_map(|(name, value)| {
            let name = name.into_string().ok()?;
            let value = value.into_string().ok()?;
            (!value.trim().is_empty() && reqwest::header::HeaderValue::from_str(&value).is_ok())
                .then_some(name)
        })
        .collect();
    let (service_account_path, service_account_configured) = config_root
        .as_ref()
        .map(|root| {
            let auth = google::GoogleAuth::new(root);
            (
                auth.service_account_path()
                    .map_err(|error| error.to_string()),
                auth.service_account_configured()
                    .map_err(|error| error.to_string()),
            )
        })
        .unwrap_or((Ok(None), Ok(false)));
    ProviderCredentialScope {
        config_root,
        environment_keys,
        service_account_path,
        service_account_configured,
    }
}

/// Whether `config`'s provider can authenticate in `scope`: a saved login, a saved or
/// environment API key, or a connection that needs none.
pub fn provider_signed_in(
    config: &tickloop_harness_protocol::ModelConfig,
    scope: &ProviderCredentialScope,
) -> Result<bool, String> {
    use tickloop_harness_protocol::ModelAuthSource;
    let root = scope.config_root.as_deref();
    if let Some(region) = minimax::MiniMaxRegion::from_auth(config.auth_source) {
        return root.map_or(Ok(false), |root| {
            minimax::MiniMaxAuth::new(root, region)
                .has_saved_login()
                .map_err(|error| error.to_string())
        });
    }
    match config.auth_source {
        ModelAuthSource::GoogleCloud => return scope.service_account_configured.clone(),
        ModelAuthSource::GoogleOAuth => {
            return root.map_or(Ok(false), |root| {
                google::GoogleAuth::new(root)
                    .has_saved_login()
                    .map_err(|error| error.to_string())
            });
        }
        _ => {}
    }
    if let Some(kind) = device_subscriptions::DeviceSubscription::from_auth(config.auth_source) {
        return root.map_or(Ok(false), |root| {
            device_subscriptions::SubscriptionAuth::new(root, kind)
                .has_saved_login()
                .map_err(|error| error.to_string())
        });
    }
    match config.auth_source {
        ModelAuthSource::GithubCopilot => {
            return root.map_or(Ok(false), |root| {
                copilot::CopilotAuth::new(root)
                    .has_saved_login()
                    .map_err(|error| error.to_string())
            });
        }
        ModelAuthSource::CodexSubscription => {
            return root.map_or(Ok(false), |root| {
                CodexAuth::new(root)
                    .has_saved_login()
                    .map_err(|error| error.to_string())
            });
        }
        _ => {}
    }
    if config.api_key_env.is_empty() {
        return Ok(true);
    }
    if let Some(root) = root
        && crate::credentials::load_api_key(root, &config.api_key_env)
            .map_err(|error| error.to_string())?
            .is_some()
    {
        return Ok(true);
    }
    Ok(scope.environment_keys.contains(&config.api_key_env))
}

#[cfg(test)]
mod network_tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener as StdTcpListener,
        process::Command,
        thread,
    };
    use tokio::net::TcpListener;

    #[test]
    fn oauth_requests_use_environment_proxy() {
        const CHILD: &str = "TICKLOOP_OAUTH_PROXY_TEST_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let directory = tempfile::tempdir().unwrap();
                let auth = CodexAuth::new(directory.path().canonicalize().unwrap());
                assert_eq!(
                    auth.network_route.proxy_source,
                    tickloop_harness_base::network::ProxySource::Environment
                );
                let result = auth.start_device_login(&CancellationToken::new()).await;
                assert!(
                    matches!(result, Err(AuthError::Network { .. })),
                    "{result:?}"
                );
            });
            return;
        }

        let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let proxy = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut bytes = Vec::new();
            loop {
                let mut chunk = [0; 1024];
                let count = stream.read(&mut chunk).unwrap();
                assert!(count > 0);
                bytes.extend_from_slice(&chunk[..count]);
                if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            stream
                .write_all(
                    b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
            String::from_utf8(bytes).unwrap()
        });
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "auth::network_tests::oauth_requests_use_environment_proxy",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("HTTPS_PROXY", format!("http://{address}"))
            .env("https_proxy", format!("http://{address}"))
            .env("NO_PROXY", "")
            .env("no_proxy", "")
            .env_remove("ALL_PROXY")
            .env_remove("all_proxy")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let request = proxy.join().unwrap();
        assert!(request.starts_with("CONNECT auth.openai.com:443 HTTP/1.1"));
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    #[ignore = "requires a macOS HTTP(S) system proxy, no proxy environment variables, and network access"]
    async fn oauth_uses_system_proxy_without_proxy_environment() {
        for name in [
            "http_proxy",
            "https_proxy",
            "all_proxy",
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "no_proxy",
            "NO_PROXY",
        ] {
            assert!(
                std::env::var_os(name).is_none(),
                "unset {name} before running this check"
            );
        }
        let url = format!("{PRODUCTION_ORIGIN}/oauth/token").parse().unwrap();
        assert!(
            hyper_util::client::proxy::matcher::Matcher::from_system()
                .intercept(&url)
                .is_some(),
            "the enabled macOS system proxy must be available to the HTTP client"
        );
        let directory = tempfile::tempdir().unwrap();
        let auth = CodexAuth::new(directory.path());
        let result = auth
            .exchange_code(
                "tickloop-invalid-diagnostic-code",
                "invalid-diagnostic-verifier",
                "http://localhost:1455/auth/callback",
                &CancellationToken::new(),
            )
            .await;
        assert!(
            matches!(
                result,
                Err(AuthError::HttpStatus {
                    status: 400 | 401,
                    ..
                })
            ),
            "expected the token endpoint to reject the synthetic code, got {:?}",
            result.err()
        );
        assert!(!auth.credential_path().exists());
    }

    #[tokio::test]
    async fn token_exchange_reports_connection_failure_without_secrets() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let directory = tempfile::tempdir().unwrap();
        let auth = CodexAuth::new_for_loopback_test(
            directory.path().to_path_buf(),
            &format!("http://{address}"),
        );
        let auth = auth.unwrap();
        let error = auth
            .exchange_code(
                "secret-code",
                "secret-verifier",
                "http://localhost/auth/callback",
                &CancellationToken::new(),
            )
            .await
            .err()
            .expect("exchange must fail");
        let message = error.to_string();
        assert!(message.contains("connection failed"), "{message}");
        assert!(!format!("{error:?}").contains("secret"));
    }

    #[tokio::test]
    async fn token_exchange_reports_timeout_without_secrets() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        });
        let directory = tempfile::tempdir().unwrap();
        let mut auth = CodexAuth::new_for_loopback_test(
            directory.path().to_path_buf(),
            &format!("http://{address}"),
        )
        .unwrap();
        auth.http = Client::builder()
            .no_proxy()
            .timeout(Duration::from_millis(100))
            .build()
            .unwrap();
        let error = auth
            .exchange_code(
                "secret-code",
                "secret-verifier",
                "http://localhost/auth/callback",
                &CancellationToken::new(),
            )
            .await
            .err()
            .expect("exchange must fail");
        server.abort();
        let message = error.to_string();
        assert!(message.contains("timed out"), "{message}");
        assert!(matches!(
            error,
            AuthError::NetworkTimeout {
                operation: "code exchange"
            }
        ));
        assert!(!format!("{error:?}").contains("secret"));
    }
}

use std::{
    fmt,
    fs::{File, OpenOptions},
    io::{self, Read},
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
    time::{Duration, Instant},
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::{Client, redirect::Policy};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use tickloop_agent_base::{
    network::{HttpPolicy, build_http_client, is_loopback},
    private_fs::{
        OpenTarget, PRIVATE_FILE_MODE, open_without_following_links, replace_file_atomically,
        unix_time_milliseconds, verify_private_directory,
    },
};
use tickloop_agent_protocol::ModelAuthSource;

const CLIENT_ID: &str = "78257093-7e40-4613-99e0-527b14b39113";
const SCOPE: &str = "group_id profile model.completion";
const USER_CODE_GRANT: &str = "urn:ietf:params:oauth:grant-type:user_code";
const RESPONSE_LIMIT: usize = 64 * 1024;
const REFRESH_SKEW_MS: u64 = 60_000;
const MAX_LOGIN_MS: u64 = 15 * 60_000;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MiniMaxRegion {
    China,
    Global,
}

impl MiniMaxRegion {
    pub fn from_auth(source: ModelAuthSource) -> Option<Self> {
        match source {
            ModelAuthSource::MiniMaxOAuthCn => Some(Self::China),
            ModelAuthSource::MiniMaxOAuthGlobal => Some(Self::Global),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::China => "MiniMax China",
            Self::Global => "MiniMax Global",
        }
    }

    fn origin(self) -> &'static str {
        match self {
            Self::China => "https://account.minimaxi.com",
            Self::Global => "https://account.minimax.io",
        }
    }

    fn slug(self) -> &'static str {
        match self {
            Self::China => "cn",
            Self::Global => "global",
        }
    }

    pub(crate) fn validate_resource(self, value: &str) -> Result<(), MiniMaxAuthError> {
        let url = reqwest::Url::parse(value).map_err(|_| MiniMaxAuthError::InvalidResponse)?;
        let allowed = match self {
            Self::China => matches!(url.host_str(), Some("api.minimax.cn" | "api.minimaxi.com")),
            Self::Global => url.host_str() == Some("api.minimax.io"),
        };
        if !allowed
            || url.scheme() != "https"
            || url.port().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || !matches!(
                url.path().trim_end_matches('/'),
                "" | "/anthropic" | "/anthropic/v1"
            )
        {
            return Err(MiniMaxAuthError::InvalidResponse);
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MiniMaxAuthError {
    #[error("MiniMax credential refresh exceeded the request attempt budget")]
    BudgetExhausted,
    #[error("MiniMax sign-in cancelled")]
    Cancelled,
    #[error("MiniMax device authorization expired; retry from /provider")]
    Expired,
    #[error("MiniMax authorization was denied")]
    Denied,
    #[error("MiniMax OAuth response is invalid or belongs to another authorization")]
    InvalidResponse,
    #[error("MiniMax OAuth returned an invalid {0} field")]
    InvalidField(&'static str),
    #[error("MiniMax OAuth returned service error {0}")]
    Service(u64),
    #[error("MiniMax OAuth request failed with HTTP {0}")]
    Http(u16),
    #[error("MiniMax OAuth connection failed or timed out")]
    Network,
    #[error("MiniMax is not signed in; select its OAuth Provider in /model")]
    NotLoggedIn,
    #[error("MiniMax credentials need renewed authorization; select the Provider and press r")]
    RefreshFailed,
    #[error("MiniMax credential storage is invalid or unsafe")]
    Storage,
    #[error("MiniMax sign-in was superseded or signed out; start a new authorization")]
    Superseded,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct MiniMaxCredentials {
    pub access_token: String,
    refresh_token: String,
    pub expires_at_ms: u64,
    pub identity: Uuid,
}

impl fmt::Debug for MiniMaxCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MiniMaxCredentials")
            .field("tokens", &"[redacted]")
            .field("expires_at_ms", &self.expires_at_ms)
            .finish()
    }
}

#[derive(Deserialize, Serialize)]
struct StoredState {
    region: MiniMaxRegion,
    generation: Uuid,
    credentials: Option<MiniMaxCredentials>,
}

pub struct MiniMaxDeviceLogin {
    pub verification_uri: String,
    pub user_code: String,
    verifier: String,
    generation: Uuid,
    expires_at_ms: u64,
    interval_ms: u64,
}

#[derive(Clone)]
pub struct MiniMaxAuth {
    root: PathBuf,
    region: MiniMaxRegion,
    origin: String,
    client: Client,
}

impl MiniMaxAuth {
    pub fn new(root: impl Into<PathBuf>, region: MiniMaxRegion) -> Self {
        Self::at_origin(root.into(), region, region.origin().into())
    }

    fn at_origin(root: PathBuf, region: MiniMaxRegion, origin: String) -> Self {
        let mut policy = HttpPolicy::new(Policy::none());
        policy.timeout = Some(Duration::from_secs(30));
        let (client, _) = build_http_client(
            &reqwest::Url::parse(&origin).expect("validated OAuth origin"),
            policy,
        )
        .expect("valid OAuth HTTP client");
        Self {
            root,
            region,
            origin,
            client,
        }
    }

    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub fn new_for_loopback_test(
        root: impl Into<PathBuf>,
        region: MiniMaxRegion,
        origin: &str,
    ) -> Result<Self, MiniMaxAuthError> {
        let url = reqwest::Url::parse(origin).map_err(|_| MiniMaxAuthError::InvalidResponse)?;
        if url.scheme() != "http"
            || !is_loopback(&url)
            || url.port().is_none()
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(MiniMaxAuthError::InvalidResponse);
        }
        Ok(Self::at_origin(
            root.into(),
            region,
            origin.trim_end_matches('/').into(),
        ))
    }

    pub fn has_saved_login(&self) -> Result<bool, MiniMaxAuthError> {
        match std::fs::symlink_metadata(self.path()) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(_) => return Err(MiniMaxAuthError::Storage),
        }
        Ok(self
            .read()?
            .is_some_and(|state| state.credentials.is_some()))
    }

    pub async fn start_device_login(
        &self,
        cancel: &CancellationToken,
    ) -> Result<MiniMaxDeviceLogin, MiniMaxAuthError> {
        let generation = Uuid::new_v4();
        {
            let _lock = self.lock(cancel).await?;
            let credentials = self.read()?.and_then(|state| state.credentials);
            self.save(&StoredState {
                region: self.region,
                generation,
                credentials,
            })?;
        }
        let verifier = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let state = Uuid::new_v4().to_string();
        let payload = self
            .post(
                "/oauth2/device/code",
                &[
                    ("response_type", "code"),
                    ("client_id", CLIENT_ID),
                    ("scope", SCOPE),
                    ("code_challenge", &challenge),
                    ("code_challenge_method", "S256"),
                    ("state", &state),
                ],
                cancel,
            )
            .await?;
        if payload["state"].as_str() != Some(&state) {
            return Err(MiniMaxAuthError::InvalidResponse);
        }
        let verification_uri = field(&payload, "verification_uri")?;
        let url = reqwest::Url::parse(&verification_uri)
            .map_err(|_| MiniMaxAuthError::InvalidResponse)?;
        if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
            return Err(MiniMaxAuthError::InvalidResponse);
        }
        let expires_at_ms = payload["expired_in"]
            .as_u64()
            .filter(|deadline| *deadline > unix_time_milliseconds())
            .ok_or(MiniMaxAuthError::Expired)?;
        Ok(MiniMaxDeviceLogin {
            verification_uri,
            user_code: field(&payload, "user_code")?,
            verifier,
            generation,
            expires_at_ms: expires_at_ms.min(unix_time_milliseconds().saturating_add(MAX_LOGIN_MS)),
            interval_ms: payload["interval"]
                .as_u64()
                .unwrap_or(2000)
                .clamp(100, 60_000),
        })
    }

    pub async fn finish_device_login(
        &self,
        login: MiniMaxDeviceLogin,
        cancel: &CancellationToken,
    ) -> Result<MiniMaxCredentials, MiniMaxAuthError> {
        let mut interval = login.interval_ms;
        loop {
            if unix_time_milliseconds() >= login.expires_at_ms {
                return Err(MiniMaxAuthError::Expired);
            }
            let payload = self
                .post(
                    "/oauth2/token",
                    &[
                        ("grant_type", USER_CODE_GRANT),
                        ("client_id", CLIENT_ID),
                        ("user_code", &login.user_code),
                        ("code_verifier", &login.verifier),
                    ],
                    cancel,
                )
                .await?;
            if payload["status"] == "success" {
                let credential = self.parse_credentials(&payload, login.generation, None)?;
                let _lock = self.lock(cancel).await?;
                let current = self.read()?.ok_or(MiniMaxAuthError::Superseded)?;
                if current.generation != login.generation {
                    return Err(MiniMaxAuthError::Superseded);
                }
                if cancel.is_cancelled() {
                    return Err(MiniMaxAuthError::Cancelled);
                }
                self.save(&StoredState {
                    region: self.region,
                    generation: login.generation,
                    credentials: Some(credential.clone()),
                })?;
                return Ok(credential);
            }
            match error_code(&payload).or_else(|| payload["status"].as_str()) {
                Some("authorization_pending" | "pending") => {}
                Some("slow_down") => interval = interval.saturating_add(5000).min(60_000),
                Some("access_denied" | "denied") => return Err(MiniMaxAuthError::Denied),
                Some("expired_token" | "expired") => return Err(MiniMaxAuthError::Expired),
                _ => return Err(MiniMaxAuthError::InvalidResponse),
            }
            let remaining = login.expires_at_ms.saturating_sub(unix_time_milliseconds());
            tokio::select! {
                biased;
                () = cancel.cancelled() => return Err(MiniMaxAuthError::Cancelled),
                () = tokio::time::sleep(Duration::from_millis(interval.min(remaining))) => {},
            }
        }
    }

    pub async fn resolve_credentials(
        &self,
        cancel: &CancellationToken,
    ) -> Result<MiniMaxCredentials, MiniMaxAuthError> {
        self.resolve_credentials_with_refresh_gate(cancel, || true)
            .await
    }

    pub(crate) async fn resolve_credentials_with_refresh_gate(
        &self,
        cancel: &CancellationToken,
        allow_refresh: impl FnOnce() -> bool,
    ) -> Result<MiniMaxCredentials, MiniMaxAuthError> {
        let _lock = self.lock(cancel).await?;
        let mut state = self.read()?.ok_or(MiniMaxAuthError::NotLoggedIn)?;
        let current = state
            .credentials
            .as_ref()
            .ok_or(MiniMaxAuthError::NotLoggedIn)?;
        if current.expires_at_ms > unix_time_milliseconds().saturating_add(REFRESH_SKEW_MS) {
            return Ok(current.clone());
        }
        if !allow_refresh() {
            return Err(MiniMaxAuthError::BudgetExhausted);
        }
        let payload = self
            .post(
                "/oauth2/token",
                &[
                    ("grant_type", "refresh_token"),
                    ("client_id", CLIENT_ID),
                    ("refresh_token", &current.refresh_token),
                ],
                cancel,
            )
            .await?;
        if payload["status"] != "success" {
            if matches!(
                error_code(&payload),
                Some("invalid_grant" | "invalid_refresh_token" | "refresh_token_reused")
            ) {
                state.credentials = None;
                self.save(&state)?;
                return Err(MiniMaxAuthError::RefreshFailed);
            }
            return Err(MiniMaxAuthError::InvalidResponse);
        }
        let refreshed =
            self.parse_credentials(&payload, current.identity, Some(&current.refresh_token))?;
        if cancel.is_cancelled() {
            return Err(MiniMaxAuthError::Cancelled);
        }
        state.credentials = Some(refreshed.clone());
        self.save(&state)?;
        Ok(refreshed)
    }

    pub async fn logout(&self, cancel: &CancellationToken) -> Result<(), MiniMaxAuthError> {
        let _lock = self.lock(cancel).await?;
        self.save(&StoredState {
            region: self.region,
            generation: Uuid::new_v4(),
            credentials: None,
        })
    }

    fn parse_credentials(
        &self,
        payload: &Value,
        identity: Uuid,
        previous_refresh: Option<&str>,
    ) -> Result<MiniMaxCredentials, MiniMaxAuthError> {
        if payload
            .get("token_type")
            .filter(|kind| !kind.is_null())
            .is_some_and(|kind| {
                !kind
                    .as_str()
                    .is_some_and(|value| value.eq_ignore_ascii_case("bearer"))
            })
        {
            return Err(MiniMaxAuthError::InvalidField("token_type"));
        }
        if let Some(resource) = payload.get("resource_url").filter(|value| !value.is_null()) {
            self.region
                .validate_resource(
                    resource
                        .as_str()
                        .ok_or(MiniMaxAuthError::InvalidField("resource_url"))?,
                )
                .map_err(|_| MiniMaxAuthError::InvalidField("resource_url"))?;
        }
        let refresh_token = payload["refresh_token"]
            .as_str()
            .filter(|token| !token.is_empty())
            .or(previous_refresh)
            .ok_or(MiniMaxAuthError::InvalidField("refresh_token"))?
            .to_owned();
        let access_token = field(payload, "access_token")?;
        let expiry = payload["expired_in"]
            .as_u64()
            .or_else(|| payload["expires_in"].as_u64())
            .ok_or(MiniMaxAuthError::InvalidField("expired_in"))?;
        let expires_at_ms =
            normalize_expiry(expiry).ok_or(MiniMaxAuthError::InvalidField("expired_in"))?;
        Ok(MiniMaxCredentials {
            access_token,
            refresh_token,
            expires_at_ms,
            identity,
        })
    }

    async fn post(
        &self,
        path: &str,
        form: &[(&str, &str)],
        cancel: &CancellationToken,
    ) -> Result<Value, MiniMaxAuthError> {
        let request = self
            .client
            .post(format!("{}{path}", self.origin))
            .header("accept", "application/json")
            .header("x-request-id", Uuid::new_v4().to_string())
            .form(form);
        tokio::select! {
            biased;
            () = cancel.cancelled() => Err(MiniMaxAuthError::Cancelled),
            result = async {
                let mut response = request.send().await.map_err(|_| MiniMaxAuthError::Network)?;
                let status = response.status();
                let mut bytes = Vec::new();
                while let Some(chunk) = response.chunk().await.map_err(|_| MiniMaxAuthError::Network)? {
                    if bytes.len().saturating_add(chunk.len()) > RESPONSE_LIMIT { return Err(MiniMaxAuthError::InvalidResponse); }
                    bytes.extend_from_slice(&chunk);
                }
                let value: Value = serde_json::from_slice(&bytes).map_err(|_| if status.is_success() { MiniMaxAuthError::InvalidResponse } else { MiniMaxAuthError::Http(status.as_u16()) })?;
                if std::env::var_os("TICKLOOP_NETWORK_DIAGNOSTICS").is_some() {
                    let known_status = match value["status"].as_str() {
                        Some("success" | "pending" | "error" | "denied" | "expired") => value["status"].as_str().unwrap(),
                        _ => "unrecognized",
                    };
                    eprintln!("minimax OAuth response: http={} status={} access_present={} refresh_present={} expiry_numeric={} resource_present={} base_code={:?}", status.as_u16(), known_status, value["access_token"].is_string(), value["refresh_token"].is_string(), value["expired_in"].is_u64(), !value["resource_url"].is_null(), value["base_resp"]["status_code"].as_u64());
                }
                if let Some(code) = value["base_resp"]["status_code"].as_u64().filter(|code| *code != 0) {
                    return Err(MiniMaxAuthError::Service(code));
                }
                if !status.is_success() && error_code(&value).is_none() { return Err(MiniMaxAuthError::Http(status.as_u16())); }
                Ok(value)
            } => result,
        }
    }

    fn directory(&self) -> Result<PathBuf, MiniMaxAuthError> {
        let directory = self.root.join("auth");
        open_without_following_links(
            &directory,
            OpenTarget::Directory {
                create_mode: Some(tickloop_agent_base::private_fs::PRIVATE_DIRECTORY_MODE),
            },
        )
        .map_err(|_| MiniMaxAuthError::Storage)?;
        verify_private_directory(&directory, "MiniMax credential directory")
            .map_err(|_| MiniMaxAuthError::Storage)?;
        Ok(directory)
    }

    fn path(&self) -> PathBuf {
        self.root
            .join("auth")
            .join(format!("minimax-{}.json", self.region.slug()))
    }

    fn read(&self) -> Result<Option<StoredState>, MiniMaxAuthError> {
        self.directory()?;
        let file = match open_without_following_links(&self.path(), OpenTarget::Entry) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(MiniMaxAuthError::Storage),
        };
        super::verify_private_file(&file).map_err(|_| MiniMaxAuthError::Storage)?;
        let mut bytes = Vec::new();
        file.take(RESPONSE_LIMIT as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| MiniMaxAuthError::Storage)?;
        if bytes.len() > RESPONSE_LIMIT {
            return Err(MiniMaxAuthError::Storage);
        }
        let state: StoredState =
            serde_json::from_slice(&bytes).map_err(|_| MiniMaxAuthError::Storage)?;
        if state.region != self.region
            || state.credentials.as_ref().is_some_and(|credential| {
                credential.access_token.is_empty()
                    || credential.refresh_token.is_empty()
                    || credential.expires_at_ms == 0
            })
        {
            return Err(MiniMaxAuthError::Storage);
        }
        Ok(Some(state))
    }

    fn save(&self, state: &StoredState) -> Result<(), MiniMaxAuthError> {
        self.directory()?;
        self.read()?;
        replace_file_atomically(
            &self.path(),
            &serde_json::to_vec(state).map_err(|_| MiniMaxAuthError::Storage)?,
            PRIVATE_FILE_MODE,
        )
        .map_err(|_| MiniMaxAuthError::Storage)
    }

    async fn lock(&self, cancel: &CancellationToken) -> Result<File, MiniMaxAuthError> {
        let directory = self.directory()?;
        let deadline = Instant::now() + Duration::from_secs(35);
        loop {
            if cancel.is_cancelled() {
                return Err(MiniMaxAuthError::Cancelled);
            }
            let path = directory.join(format!("minimax-{}.lock", self.region.slug()));
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(PRIVATE_FILE_MODE)
                .custom_flags(libc::O_NOFOLLOW)
                .open(path)
                .map_err(|_| MiniMaxAuthError::Storage)?;
            super::verify_private_file(&file).map_err(|_| MiniMaxAuthError::Storage)?;
            match file.try_lock() {
                Ok(()) => return Ok(file),
                Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                    tokio::select! {
                        biased;
                        () = cancel.cancelled() => return Err(MiniMaxAuthError::Cancelled),
                        () = tokio::time::sleep(Duration::from_millis(50)) => {},
                    }
                }
                _ => return Err(MiniMaxAuthError::Storage),
            }
        }
    }
}

fn field(payload: &Value, name: &str) -> Result<String, MiniMaxAuthError> {
    payload[name]
        .as_str()
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
        .ok_or(MiniMaxAuthError::InvalidResponse)
}

fn error_code(payload: &Value) -> Option<&str> {
    payload["error"]
        .as_str()
        .or_else(|| payload["error"]["code"].as_str())
}

fn normalize_expiry(expiry: u64) -> Option<u64> {
    let now = unix_time_milliseconds();
    let deadline = if expiry >= 1_000_000_000_000 {
        expiry
    } else if expiry >= 1_000_000_000 {
        expiry.checked_mul(1000)?
    } else {
        now.checked_add(expiry.checked_mul(1000)?)?
    };
    (deadline > now).then_some(deadline)
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

    async fn server(
        replies: Vec<Value>,
    ) -> (
        String,
        tokio::task::JoinHandle<Vec<(String, HashMap<String, String>)>>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let mut requests = Vec::new();
            for mut reply in replies {
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
                    .unwrap();
                let mut bytes = vec![0; length];
                reader.read_exact(&mut bytes).await.unwrap();
                let url = reqwest::Url::parse(&format!(
                    "http://localhost/?{}",
                    String::from_utf8(bytes).unwrap()
                ))
                .unwrap();
                let form = url
                    .query_pairs()
                    .map(|(key, value)| (key.into_owned(), value.into_owned()))
                    .collect::<HashMap<_, _>>();
                if reply["user_code"].is_string() {
                    reply["state"] = json!(form.get("state").unwrap());
                }
                requests.push((headers.lines().next().unwrap().to_owned(), form));
                let body = reply.to_string();
                reader.into_inner().write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            }
            requests
        });
        (address, task)
    }

    fn device_reply() -> Value {
        json!({"user_code":"test-user-code","verification_uri":"https://account.minimaxi.com/authorize","expired_in":unix_time_milliseconds()+60_000,"interval":100})
    }

    fn token(access: &str, expires: u64) -> Value {
        json!({"status":"success","access_token":access,"refresh_token":"test-refresh","expired_in":expires,"token_type":"Bearer","resource_url":"https://api.minimaxi.com/anthropic"})
    }

    #[tokio::test]
    async fn device_pkce_pending_refresh_deduplication_and_regional_logout() {
        let directory = tempfile::tempdir().unwrap();
        let (origin, requests) = server(vec![
            device_reply(),
            json!({"status":"pending"}),
            token("test-access", 1),
            json!({"status":"success","access_token":"test-refreshed","expired_in":3600}),
        ])
        .await;
        let auth = MiniMaxAuth::new_for_loopback_test(
            directory.path().canonicalize().unwrap(),
            MiniMaxRegion::China,
            &origin,
        )
        .unwrap();
        let cancel = CancellationToken::new();
        let login = auth.start_device_login(&cancel).await.unwrap();
        assert_eq!(login.user_code, "test-user-code");
        let credential = auth.finish_device_login(login, &cancel).await.unwrap();
        assert!(matches!(
            auth.resolve_credentials_with_refresh_gate(&cancel, || false)
                .await,
            Err(MiniMaxAuthError::BudgetExhausted)
        ));
        let (first, second) = tokio::join!(
            auth.resolve_credentials(&cancel),
            auth.resolve_credentials(&cancel)
        );
        let first = first.unwrap();
        let second = second.unwrap();
        assert_eq!(first.identity, credential.identity);
        assert_eq!(second.identity, credential.identity);
        assert_eq!(first.access_token, "test-refreshed");
        assert_eq!(second.access_token, first.access_token);
        assert_eq!(first.refresh_token, "test-refresh");
        let requests = requests.await.unwrap();
        assert_eq!(requests.len(), 4);
        assert!(requests[0].0.starts_with("POST /oauth2/device/code "));
        assert_eq!(requests[0].1["client_id"], CLIENT_ID);
        assert_eq!(requests[1].1["grant_type"], USER_CODE_GRANT);
        assert_eq!(
            requests[0].1["code_challenge"],
            URL_SAFE_NO_PAD.encode(Sha256::digest(requests[1].1["code_verifier"].as_bytes()))
        );
        assert_eq!(requests[3].1["grant_type"], "refresh_token");
        assert!(auth.has_saved_login().unwrap());
        assert!(
            !MiniMaxAuth::new(
                directory.path().canonicalize().unwrap(),
                MiniMaxRegion::Global
            )
            .has_saved_login()
            .unwrap()
        );
        auth.logout(&cancel).await.unwrap();
        assert!(!auth.has_saved_login().unwrap());
        assert!(matches!(
            auth.resolve_credentials(&cancel).await,
            Err(MiniMaxAuthError::NotLoggedIn)
        ));
        assert!(!format!("{first:?}").contains("test-refreshed"));
    }

    #[tokio::test]
    async fn logout_invalidates_pending_authorization_and_cancellation_does_not_save_tokens() {
        let directory = tempfile::tempdir().unwrap();
        let (origin, requests) = server(vec![
            device_reply(),
            token("discarded-access", 3600),
            device_reply(),
        ])
        .await;
        let auth = MiniMaxAuth::new_for_loopback_test(
            directory.path().canonicalize().unwrap(),
            MiniMaxRegion::China,
            &origin,
        )
        .unwrap();
        let cancel = CancellationToken::new();
        let login = auth.start_device_login(&cancel).await.unwrap();
        auth.logout(&cancel).await.unwrap();
        assert!(matches!(
            auth.finish_device_login(login, &cancel).await,
            Err(MiniMaxAuthError::Superseded)
        ));
        assert!(!auth.has_saved_login().unwrap());
        let login = auth.start_device_login(&cancel).await.unwrap();
        cancel.cancel();
        assert!(matches!(
            auth.finish_device_login(login, &cancel).await,
            Err(MiniMaxAuthError::Cancelled)
        ));
        assert!(!auth.has_saved_login().unwrap());
        assert_eq!(requests.await.unwrap().len(), 3);
    }

    #[test]
    fn resource_origin_is_bound_to_region_and_token_expiry_units_are_distinct() {
        assert!(
            MiniMaxRegion::China
                .validate_resource("https://api.minimaxi.com/anthropic/v1")
                .is_ok()
        );
        assert!(
            MiniMaxRegion::China
                .validate_resource("https://api.minimaxi.com/anthropic")
                .is_ok()
        );
        assert!(
            MiniMaxRegion::China
                .validate_resource("https://api.minimax.io/anthropic")
                .is_err()
        );
        assert!(
            MiniMaxRegion::Global
                .validate_resource("https://api.minimax.io/anthropic?key=leak")
                .is_err()
        );
        assert!(
            MiniMaxRegion::Global
                .validate_resource("https://api.minimax.io.attacker.test/anthropic")
                .is_err()
        );
        assert!(
            MiniMaxRegion::Global
                .validate_resource("https://api.minimax.io/mavis/api/v1/llm/v1")
                .is_err()
        );
        let now = unix_time_milliseconds();
        assert!(normalize_expiry(3600).unwrap() >= now + 3_600_000);
        let future_seconds = now / 1000 + 3600;
        assert_eq!(
            normalize_expiry(future_seconds),
            Some(future_seconds * 1000)
        );
        assert_eq!(normalize_expiry(now + 3600), Some(now + 3600));
        assert_eq!(normalize_expiry(0), None);
    }

    #[tokio::test]
    #[ignore = "requires an explicitly provided private real Portal response fixture"]
    async fn import_real_portal_fixture_after_resource_contract_validation() {
        let root =
            PathBuf::from(std::env::var_os("MINIMAX_PROBE_STATE").expect("probe state directory"))
                .canonicalize()
                .unwrap();
        let auth = MiniMaxAuth::new(&root, MiniMaxRegion::China);
        let file =
            open_without_following_links(&root.join("portal-response.json"), OpenTarget::Entry)
                .unwrap();
        super::super::verify_private_file(&file).unwrap();
        let payload: Value = serde_json::from_reader(file.take(RESPONSE_LIMIT as u64)).unwrap();
        assert_eq!(payload["status"], "success");
        let generation = Uuid::new_v4();
        let credentials = auth.parse_credentials(&payload, generation, None).unwrap();
        let cancel = CancellationToken::new();
        let _lock = auth.lock(&cancel).await.unwrap();
        auth.save(&StoredState {
            region: MiniMaxRegion::China,
            generation,
            credentials: Some(credentials),
        })
        .unwrap();
    }
}

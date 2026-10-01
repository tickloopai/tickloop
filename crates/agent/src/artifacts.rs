//! Authentication for the independent Artifact Service.

use std::{
    fs::{File, OpenOptions},
    io::{self, Read},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use reqwest::{Client, Method, StatusCode, Url, header, redirect::Policy};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::{
    network::{HttpPolicy, build_http_client, is_loopback},
    private_fs::{
        OpenTarget, PRIVATE_DIRECTORY_MODE, PRIVATE_FILE_MODE, open_without_following_links,
        replace_file_atomically, verify_private_directory,
    },
};

mod catalog;
pub use catalog::{ArtifactMetadata, ArtifactOperation, ArtifactPage, ArtifactTick};

pub const DEFAULT_ORIGIN: &str = "https://docs.tickloop.ai";
const ORIGIN_ENV: &str = "TICKLOOP_ARTIFACT_ORIGIN";
const MAX_RESPONSE_BYTES: usize = 16 * 1024;
const MAX_CREDENTIAL_BYTES: usize = 16 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const SLOW_DOWN_SECONDS: u64 = 5;
const MAX_DEVICE_LIFETIME_SECONDS: u64 = 60 * 60;
const LOCK_TIMEOUT: Duration = Duration::from_secs(5);
const LOCK_RETRY: Duration = Duration::from_millis(25);
const USER_AGENT: &str = concat!(
    "TickLoop-Agent/",
    env!("CARGO_PKG_VERSION"),
    " ArtifactClient/1"
);

#[derive(Debug, thiserror::Error)]
pub enum ArtifactError {
    #[error(
        "invalid Artifact Service origin; use an HTTPS origin or an HTTP loopback test endpoint"
    )]
    InvalidOrigin,
    #[error("Artifact Service connection failed")]
    Connection,
    #[error("Artifact Service returned an invalid response")]
    InvalidResponse,
    #[error("Artifact Service authorization was denied")]
    AccessDenied,
    #[error("Artifact Service device code expired")]
    Expired,
    #[error("Artifact Service login cancelled")]
    Cancelled,
    #[error("Artifact Service credential storage is unavailable or unsafe")]
    Storage,
    #[error("Artifact Service returned HTTP {status}: {code}")]
    Service { status: u16, code: String },
    #[error("Artifact Service modification outcome unknown; operation {operation_id}: {outcome}")]
    OutcomeUnknown {
        operation_id: String,
        outcome: String,
    },
    #[error("Artifact Service returned HTTP {0}")]
    Http(u16),
    #[error(
        "Artifact Service remote revocation is unconfirmed; local credential is retained for an explicit retry"
    )]
    RevocationUnknown,
}

pub enum LogoutOutcome {
    AlreadySignedOut,
    Removed,
    NewerLoginPresent,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct ArtifactUser {
    pub id: String,
    pub name: String,
    pub email: String,
}

impl ArtifactUser {
    pub fn display_name(&self) -> &str {
        if self.email.is_empty() {
            &self.name
        } else {
            &self.email
        }
    }
}

#[derive(Deserialize)]
pub struct DeviceAuthorization {
    device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    expires_in: u64,
    interval: u64,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    token_type: String,
    expires_in: u64,
    user: ArtifactUser,
}

#[derive(Deserialize)]
struct SessionResponse {
    user: ArtifactUser,
    expires_at: String,
}

#[derive(Deserialize)]
struct LogoutResponse {
    revoked: bool,
}

#[derive(Deserialize)]
struct ErrorEnvelope {
    error: ServiceError,
}

#[derive(Deserialize)]
struct ServiceError {
    code: String,
}

#[derive(Serialize, Deserialize)]
struct StoredCredential {
    origin: String,
    access_token: String,
    expires_at: u64,
    user: ArtifactUser,
}

#[derive(Clone)]
pub struct ArtifactClient {
    origin: Url,
    http: Client,
    state_root: PathBuf,
}

pub(crate) struct CredentialSnapshot {
    pub(crate) origin: String,
    pub(crate) account_id: String,
    pub(crate) access_token: String,
}

impl CredentialSnapshot {
    pub(crate) fn identity_matches(&self, other: &Self) -> bool {
        self.origin == other.origin
            && self.account_id == other.account_id
            && self.access_token == other.access_token
    }
}

impl ArtifactClient {
    pub(crate) fn credential_snapshot(&self) -> Result<Option<CredentialSnapshot>, ArtifactError> {
        Ok(self
            .read_credential()?
            .filter(|saved| saved.expires_at > now_seconds())
            .map(|saved| CredentialSnapshot {
                origin: saved.origin,
                account_id: saved.user.id,
                access_token: saved.access_token,
            }))
    }

    pub(crate) async fn invalidate_credential(&self, token: &str) -> Result<(), ArtifactError> {
        self.remove_if_same(token).await?;
        Ok(())
    }

    pub fn new(state_root: impl Into<PathBuf>) -> Result<Self, ArtifactError> {
        let origin = std::env::var(ORIGIN_ENV).unwrap_or_else(|_| DEFAULT_ORIGIN.to_owned());
        Self::for_origin(state_root, &origin)
    }

    pub fn for_origin(
        state_root: impl Into<PathBuf>,
        configured_origin: &str,
    ) -> Result<Self, ArtifactError> {
        let origin = Url::parse(configured_origin).map_err(|_| ArtifactError::InvalidOrigin)?;
        if (origin.scheme() != "https" && !(origin.scheme() == "http" && is_loopback(&origin)))
            || origin.cannot_be_a_base()
            || origin.host_str().is_none()
            || !origin.username().is_empty()
            || origin.password().is_some()
            || origin.path() != "/"
            || origin.query().is_some()
            || origin.fragment().is_some()
        {
            return Err(ArtifactError::InvalidOrigin);
        }
        let mut policy = HttpPolicy::new(Policy::none());
        policy.timeout = Some(REQUEST_TIMEOUT);
        let (http, _) =
            build_http_client(&origin, policy).map_err(|_| ArtifactError::Connection)?;
        Ok(Self {
            origin,
            http,
            state_root: state_root.into(),
        })
    }

    pub fn origin(&self) -> &str {
        self.origin.as_str().trim_end_matches('/')
    }

    pub async fn start_device_login(&self) -> Result<DeviceAuthorization, ArtifactError> {
        let response = self
            .request(Method::POST, "/api/v1/auth/device")?
            .json(&serde_json::json!({}))
            .send()
            .await
            .map_err(|_| ArtifactError::Connection)?;
        let device: DeviceAuthorization = self.success_json(response).await?;
        let verification =
            Url::parse(&device.verification_uri).map_err(|_| ArtifactError::InvalidResponse)?;
        if device.device_code.is_empty()
            || device.user_code.is_empty()
            || device.expires_in == 0
            || device.expires_in > MAX_DEVICE_LIFETIME_SECONDS
            || device.interval == 0
            || device.interval >= device.expires_in
            || verification.origin() != self.origin.origin()
            || verification.path() != "/device"
            || verification
                .query_pairs()
                .filter(|(key, _)| key == "code")
                .count()
                != 1
            || !verification
                .query_pairs()
                .any(|(key, value)| key == "code" && value == device.user_code)
        {
            return Err(ArtifactError::InvalidResponse);
        }
        Ok(device)
    }

    pub async fn finish_device_login(
        &self,
        device: DeviceAuthorization,
        cancellation: &CancellationToken,
    ) -> Result<ArtifactUser, ArtifactError> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(device.expires_in);
        let mut interval = device.interval;
        loop {
            if tokio::time::Instant::now() + Duration::from_secs(interval) >= deadline {
                return Err(ArtifactError::Expired);
            }
            tokio::select! {
                () = cancellation.cancelled() => return Err(ArtifactError::Cancelled),
                () = tokio::time::sleep(Duration::from_secs(interval)) => {},
            }
            let request = self
                .request(Method::POST, "/api/v1/auth/device/token")?
                .json(&serde_json::json!({"device_code": device.device_code}));
            let response = tokio::select! {
                () = cancellation.cancelled() => return Err(ArtifactError::Cancelled),
                result = request.send() => result.map_err(|_| ArtifactError::Connection)?,
            };
            if response.status().is_success() {
                let token: TokenResponse = tokio::select! {
                    () = cancellation.cancelled() => return Err(ArtifactError::Cancelled),
                    result = self.success_json(response) => result?,
                };
                if token.token_type != "Bearer"
                    || token.access_token.is_empty()
                    || token.expires_in == 0
                    || token.user.id.is_empty()
                {
                    return Err(ArtifactError::InvalidResponse);
                }
                if cancellation.is_cancelled() {
                    return Err(ArtifactError::Cancelled);
                }
                let user = token.user.clone();
                self.save(&StoredCredential {
                    origin: self.origin().to_owned(),
                    access_token: token.access_token,
                    expires_at: now_seconds().saturating_add(token.expires_in),
                    user: token.user,
                })
                .await?;
                return Ok(user);
            }
            let status = response.status();
            let error: ErrorEnvelope = self.read_json(response).await?;
            match error.error.code.as_str() {
                "authorization_pending" if status == StatusCode::BAD_REQUEST => {}
                "slow_down" if status == StatusCode::BAD_REQUEST => {
                    interval = interval.saturating_add(SLOW_DOWN_SECONDS);
                }
                "access_denied" => return Err(ArtifactError::AccessDenied),
                "expired_token" => return Err(ArtifactError::Expired),
                _ => return Err(ArtifactError::Http(status.as_u16())),
            }
        }
    }

    pub async fn status(&self) -> Result<Option<ArtifactUser>, ArtifactError> {
        let Some(credential) = self.read_credential()? else {
            return Ok(None);
        };
        if credential.expires_at <= now_seconds() {
            self.remove_if_same(&credential.access_token).await?;
            return Ok(None);
        }
        let response = self
            .request(Method::GET, "/api/v1/auth/session")?
            .header(header::AUTHORIZATION, bearer(&credential.access_token)?)
            .send()
            .await
            .map_err(|_| ArtifactError::Connection)?;
        if response.status() == StatusCode::UNAUTHORIZED {
            self.remove_if_same(&credential.access_token).await?;
            return Ok(None);
        }
        let session: SessionResponse = self.success_json(response).await?;
        if session.user.id != credential.user.id || session.expires_at.is_empty() {
            return Err(ArtifactError::InvalidResponse);
        }
        Ok(Some(session.user))
    }

    pub async fn logout(&self) -> Result<LogoutOutcome, ArtifactError> {
        let Some(credential) = self.read_credential()? else {
            return Ok(LogoutOutcome::AlreadySignedOut);
        };
        let response = self
            .request(Method::POST, "/api/v1/auth/logout")?
            .header(header::AUTHORIZATION, bearer(&credential.access_token)?)
            .json(&serde_json::json!({}))
            .send()
            .await
            .map_err(|_| ArtifactError::RevocationUnknown)?;
        if !response.status().is_success() && response.status() != StatusCode::UNAUTHORIZED {
            return Err(ArtifactError::RevocationUnknown);
        }
        if response.status().is_success() {
            let revoked: LogoutResponse = self
                .read_json(response)
                .await
                .map_err(|_| ArtifactError::RevocationUnknown)?;
            if !revoked.revoked {
                return Err(ArtifactError::RevocationUnknown);
            }
        }
        if self.remove_if_same(&credential.access_token).await? {
            Ok(LogoutOutcome::Removed)
        } else {
            Ok(LogoutOutcome::NewerLoginPresent)
        }
    }

    fn url(&self, path: &str) -> Result<Url, ArtifactError> {
        self.origin
            .join(path)
            .map_err(|_| ArtifactError::InvalidOrigin)
    }

    fn request(
        &self,
        method: Method,
        path: &str,
    ) -> Result<reqwest::RequestBuilder, ArtifactError> {
        Ok(self
            .http
            .request(method, self.url(path)?)
            .header(header::USER_AGENT, USER_AGENT)
            .header(header::ACCEPT, "application/json"))
    }

    async fn success_json<T: for<'de> Deserialize<'de>>(
        &self,
        response: reqwest::Response,
    ) -> Result<T, ArtifactError> {
        if !response.status().is_success() {
            return Err(ArtifactError::Http(response.status().as_u16()));
        }
        self.read_json(response).await
    }

    async fn read_json<T: for<'de> Deserialize<'de>>(
        &self,
        response: reqwest::Response,
    ) -> Result<T, ArtifactError> {
        self.read_json_with_limit(response, MAX_RESPONSE_BYTES)
            .await
    }

    async fn read_json_with_limit<T: for<'de> Deserialize<'de>>(
        &self,
        mut response: reqwest::Response,
        limit: usize,
    ) -> Result<T, ArtifactError> {
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| ArtifactError::Connection)?
        {
            if bytes.len().saturating_add(chunk.len()) > limit {
                return Err(ArtifactError::InvalidResponse);
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).map_err(|_| ArtifactError::InvalidResponse)
    }

    fn credential_directory(&self) -> Result<PathBuf, ArtifactError> {
        let path = self.state_root.join("artifacts").join("auth");
        open_without_following_links(
            &path,
            OpenTarget::Directory {
                create_mode: Some(PRIVATE_DIRECTORY_MODE),
            },
        )
        .map_err(|_| ArtifactError::Storage)?;
        verify_private_directory(&path, "Artifact Service credential directory")
            .map_err(|_| ArtifactError::Storage)?;
        Ok(path)
    }

    fn credential_path(&self) -> PathBuf {
        let digest = Sha256::digest(self.origin().as_bytes());
        let name = digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        self.state_root
            .join("artifacts")
            .join("auth")
            .join(format!("{name}.json"))
    }

    fn read_credential(&self) -> Result<Option<StoredCredential>, ArtifactError> {
        self.credential_directory()?;
        let path = self.credential_path();
        let file = match open_without_following_links(&path, OpenTarget::Entry) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(ArtifactError::Storage),
        };
        let metadata = file.metadata().map_err(|_| ArtifactError::Storage)?;
        if !metadata.is_file()
            || metadata.uid() != unsafe { libc::getuid() }
            || metadata.permissions().mode() & 0o077 != 0
            || metadata.len() > MAX_CREDENTIAL_BYTES as u64
        {
            return Err(ArtifactError::Storage);
        }
        let mut bytes = Vec::new();
        file.take(MAX_CREDENTIAL_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| ArtifactError::Storage)?;
        let credential: StoredCredential =
            serde_json::from_slice(&bytes).map_err(|_| ArtifactError::Storage)?;
        if credential.origin != self.origin()
            || credential.access_token.is_empty()
            || credential.user.id.is_empty()
            || credential.expires_at == 0
        {
            return Err(ArtifactError::Storage);
        }
        Ok(Some(credential))
    }

    async fn save(&self, credential: &StoredCredential) -> Result<(), ArtifactError> {
        let _lock = self.lock().await?;
        self.credential_directory()?;
        self.read_credential()?;
        let bytes = serde_json::to_vec(credential).map_err(|_| ArtifactError::Storage)?;
        replace_file_atomically(&self.credential_path(), &bytes, PRIVATE_FILE_MODE)
            .map_err(|_| ArtifactError::Storage)
    }

    async fn remove_if_same(&self, token: &str) -> Result<bool, ArtifactError> {
        let _lock = self.lock().await?;
        if self
            .read_credential()?
            .is_none_or(|saved| saved.access_token != token)
        {
            return Ok(false);
        }
        std::fs::remove_file(self.credential_path()).map_err(|_| ArtifactError::Storage)?;
        Ok(true)
    }

    async fn lock(&self) -> Result<File, ArtifactError> {
        self.credential_directory()?;
        let path = self.credential_path().with_extension("lock");
        let deadline = tokio::time::Instant::now() + LOCK_TIMEOUT;
        loop {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .mode(PRIVATE_FILE_MODE)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&path)
                .map_err(|_| ArtifactError::Storage)?;
            let metadata = file.metadata().map_err(|_| ArtifactError::Storage)?;
            if !metadata.is_file()
                || metadata.uid() != unsafe { libc::getuid() }
                || metadata.permissions().mode() & 0o077 != 0
            {
                return Err(ArtifactError::Storage);
            }
            match file.try_lock() {
                Ok(()) => return Ok(file),
                Err(std::fs::TryLockError::WouldBlock) => {
                    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                    if remaining.is_zero() {
                        return Err(ArtifactError::Storage);
                    }
                    tokio::time::sleep(LOCK_RETRY.min(remaining)).await;
                }
                Err(_) => return Err(ArtifactError::Storage),
            }
        }
    }
}

fn bearer(token: &str) -> Result<header::HeaderValue, ArtifactError> {
    let mut value = header::HeaderValue::from_str(&format!("Bearer {token}"))
        .map_err(|_| ArtifactError::Storage)?;
    value.set_sensitive(true);
    Ok(value)
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        os::unix::fs::PermissionsExt,
        sync::{Arc, Mutex},
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    async fn server(
        reply: impl Fn(&str, usize, &str) -> (u16, String) + Send + Sync + 'static,
    ) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let received = Arc::new(Mutex::new(Vec::new()));
        let messages = received.clone();
        let handler = Arc::new(reply);
        let service_origin = origin.clone();
        tokio::spawn(async move {
            let mut count = 0;
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let mut request = Vec::new();
                let mut buffer = [0_u8; 1024];
                while let Ok(size) = stream.read(&mut buffer).await {
                    if size == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..size]);
                    if let Some(header_end) =
                        request.windows(4).position(|bytes| bytes == b"\r\n\r\n")
                    {
                        let headers =
                            String::from_utf8_lossy(&request[..header_end]).to_ascii_lowercase();
                        let content_length = headers
                            .lines()
                            .find_map(|line| line.strip_prefix("content-length: "))
                            .and_then(|value| value.parse::<usize>().ok())
                            .unwrap_or(0);
                        if request.len() >= header_end + 4 + content_length {
                            break;
                        }
                    }
                }
                let request = String::from_utf8_lossy(&request).into_owned();
                messages.lock().unwrap().push(request.clone());
                count += 1;
                let (status, body) = handler(&request, count, &service_origin);
                let label = if status < 300 { "OK" } else { "Error" };
                let response = format!(
                    "HTTP/1.1 {status} {label}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });
        (origin, received)
    }

    fn user_json() -> &'static str {
        r#"{"id":"user-1","name":"A User","email":"a@example.test"}"#
    }

    async fn catalog_client(origin: &str) -> (tempfile::TempDir, ArtifactClient) {
        let directory = tempfile::tempdir().unwrap();
        let client =
            ArtifactClient::for_origin(directory.path().canonicalize().unwrap(), origin).unwrap();
        client
            .save(&StoredCredential {
                origin: origin.into(),
                access_token: "test-token".into(),
                expires_at: now_seconds() + 600,
                user: ArtifactUser {
                    id: "owner".into(),
                    name: "Owner".into(),
                    email: "owner@example.test".into(),
                },
            })
            .await
            .unwrap();
        (directory, client)
    }

    #[tokio::test]
    async fn catalog_allows_larger_pages_but_invalidates_only_rejected_credentials() {
        let (origin, requests) = server(|request, _, _| {
            if request.starts_with("GET /api/v1/artifacts?") {
                (200, serde_json::json!({"artifacts":[{"id":"doc","title":"t".repeat(20000),"format":"markdown","access_role":"owner","url":"https://docs.example/a/doc"}],"next_cursor":"next"}).to_string())
            } else { (401, r#"{"error":{"code":"unauthorized"}}"#.into()) }
        }).await;
        let (_directory, client) = catalog_client(&origin).await;
        let page = client.list("owned", "hello", None).await.unwrap();
        assert_eq!(page.artifacts.len(), 1);
        assert_eq!(page.next_cursor.as_deref(), Some("next"));
        assert!(matches!(
            client.read("doc").await,
            Err(ArtifactError::AccessDenied)
        ));
        assert!(client.credential_snapshot().unwrap().is_none());
        assert!(
            requests.lock().unwrap()[0]
                .contains("ownership=owned&q=hello&limit=20&sort=updated&direction=desc")
        );
    }

    #[tokio::test]
    async fn uncertain_catalog_modification_queries_once_without_replaying_it() {
        let (origin, requests) = server(|request, _, _| {
            if request.starts_with("POST ") { (503, r#"{"error":{"code":"storage_unavailable"}}"#.into()) }
            else { (200, serde_json::json!({"operation_id":request.lines().next().unwrap().split_whitespace().nth(1).unwrap().rsplit('/').next().unwrap(),"status":"committed","result":{"title":"renamed"}}).to_string()) }
        }).await;
        let (_directory, client) = catalog_client(&origin).await;
        let operation = client.rename("doc", "renamed", 2).await.unwrap();
        assert!(operation.recovered);
        assert_eq!(operation.status, "committed");
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let body: serde_json::Value =
            serde_json::from_str(requests[0].split("\r\n\r\n").nth(1).unwrap()).unwrap();
        let id = body["operation_id"].as_str().unwrap();
        assert!(uuid::Uuid::parse_str(id).is_ok());
        assert!(requests[1].starts_with(&format!("GET /api/v1/artifacts/doc/operations/{id} ")));
    }

    #[tokio::test]
    async fn an_unknown_success_response_queries_the_original_operation_once() {
        let (origin, requests) = server(|request, _, _| {
            let operation = if request.starts_with("DELETE ") {
                let body: serde_json::Value =
                    serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
                serde_json::json!({"operation_id":body["operation_id"],"status":"unknown"})
            } else {
                let id = request
                    .lines()
                    .next()
                    .unwrap()
                    .split_whitespace()
                    .nth(1)
                    .unwrap()
                    .rsplit('/')
                    .next()
                    .unwrap();
                serde_json::json!({"operation_id":id,"status":"committed"})
            };
            (202, operation.to_string())
        })
        .await;
        let (_directory, client) = catalog_client(&origin).await;
        let result = client.delete("doc").await.unwrap();
        assert_eq!(result.status, "committed");
        assert!(result.recovered);
        assert_eq!(requests.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn catalog_limits_do_not_expand_metadata_responses_or_allow_unbounded_pages() {
        let (origin, _) = server(|request, _, _| {
            let size = if request.contains("q=oversize") { 300000 } else { 20000 };
            let artifact = serde_json::json!({"id":"doc","title":"t".repeat(size),"format":"markdown","access_role":"owner","url":"https://docs.example/a/doc"});
            (200, if request.starts_with("GET /api/v1/artifacts/pinned ") || request.contains("q=oversize") { serde_json::json!({"artifacts":[artifact],"next_cursor":null}).to_string() } else { artifact.to_string() })
        }).await;
        let (_directory, client) = catalog_client(&origin).await;
        assert_eq!(client.pinned().await.unwrap().artifacts.len(), 1);
        assert!(matches!(
            client.read("doc").await,
            Err(ArtifactError::InvalidResponse)
        ));
        assert!(matches!(
            client.list("all", "oversize", None).await,
            Err(ArtifactError::InvalidResponse)
        ));
        assert!(client.credential_snapshot().unwrap().is_some());
    }

    #[tokio::test]
    async fn catalog_conflict_is_reported_without_query_or_mutation_retry() {
        let (origin, requests) =
            server(|_, _, _| (409, r#"{"error":{"code":"catalog_conflict"}}"#.into())).await;
        let (_directory, client) = catalog_client(&origin).await;
        assert!(
            matches!(client.list("all", "", Some("old")).await, Err(ArtifactError::Service { status:409, code }) if code == "catalog_conflict")
        );
        assert_eq!(requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn login_polls_service_then_status_and_logout_use_the_same_origin() {
        let (origin, requests) = server(|request, count, origin| {
            let path = request.lines().next().unwrap_or("");
            if path.starts_with("POST /api/v1/auth/device ") {
                (200, format!(r#"{{"device_code":"private-device-code","user_code":"ABCD-EFGH","verification_uri":"{origin}/device?code=ABCD-EFGH","expires_in":30,"interval":1}}"#))
            } else if path.starts_with("POST /api/v1/auth/device/token ") && count == 2 {
                (400, r#"{"error":{"code":"authorization_pending","message":"wait"}}"#.into())
            } else if path.starts_with("POST /api/v1/auth/device/token ") {
                (200, format!(r#"{{"access_token":"private-service-token","token_type":"Bearer","expires_in":60,"user":{}}}"#, user_json()))
            } else if path.starts_with("GET /api/v1/auth/session ") {
                (200, format!(r#"{{"user":{},"expires_at":"2099-01-01T00:00:00Z"}}"#, user_json()))
            } else if path.starts_with("POST /api/v1/auth/logout ") {
                (200, r#"{"revoked":true}"#.into())
            } else {
                (404, r#"{"error":{"code":"not_found","message":"unknown"}}"#.into())
            }
        }).await;
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let client = ArtifactClient::for_origin(&root, &origin).unwrap();
        let device = client.start_device_login().await.unwrap();
        assert_eq!(device.user_code, "ABCD-EFGH");
        assert_eq!(
            device.verification_uri,
            format!("{origin}/device?code=ABCD-EFGH")
        );
        let user = client
            .finish_device_login(device, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(user.id, "user-1");
        assert_eq!(
            client.status().await.unwrap().unwrap().email,
            "a@example.test"
        );
        let stored_path = client.credential_path();
        assert_eq!(
            std::fs::metadata(&stored_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(stored_path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            ArtifactClient::for_origin(&root, "https://docs.tickloop.ai")
                .unwrap()
                .status()
                .await
                .unwrap()
                .map(|user| user.id),
            None
        );
        assert!(matches!(
            client.logout().await.unwrap(),
            LogoutOutcome::Removed
        ));
        assert!(!stored_path.exists());
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 5);
        assert!(requests[1].contains("private-device-code"));
        assert!(requests[2].contains("private-device-code"));
        assert!(!requests[0].contains("authorization:"));
        assert!(
            requests[0]
                .to_ascii_lowercase()
                .contains("user-agent: tickloop-agent/")
        );
        assert!(
            requests[0]
                .to_ascii_lowercase()
                .contains("accept: application/json")
        );
        assert!(!requests[1].contains("authorization:"));
        assert!(
            requests[3].contains("authorization: Bearer private-service-token")
                || requests[3].contains("authorization: bearer private-service-token")
        );
        assert!(
            requests[4]
                .to_ascii_lowercase()
                .contains("authorization: bearer private-service-token")
        );
    }

    #[tokio::test]
    async fn cancellation_and_unknown_token_result_stop_without_retry_or_persistence() {
        let (origin, requests) = server(|request, _, origin| {
            if request.starts_with("POST /api/v1/auth/device ") {
                (200, format!(r#"{{"device_code":"private-code","user_code":"SAFE","verification_uri":"{origin}/device?code=SAFE","expires_in":30,"interval":1}}"#))
            } else {
                (503, r#"{"error":{"code":"unavailable","message":"retry later"}}"#.into())
            }
        }).await;
        let directory = tempfile::tempdir().unwrap();
        let client =
            ArtifactClient::for_origin(directory.path().canonicalize().unwrap(), &origin).unwrap();
        let device = client.start_device_login().await.unwrap();
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        assert!(matches!(
            client.finish_device_login(device, &cancellation).await,
            Err(ArtifactError::Cancelled)
        ));
        assert_eq!(requests.lock().unwrap().len(), 1);
        let device = client.start_device_login().await.unwrap();
        assert!(matches!(
            client
                .finish_device_login(device, &CancellationToken::new())
                .await,
            Err(ArtifactError::Http(503))
        ));
        assert_eq!(requests.lock().unwrap().len(), 3);
        assert!(client.status().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn invalid_verification_origin_and_cross_origin_redirect_are_rejected() {
        let (origin, _) = server(|request, _, _| {
            if request.starts_with("POST /api/v1/auth/device ") {
                (200, r#"{"device_code":"secret","user_code":"CODE","verification_uri":"https://other.example/device?code=CODE","expires_in":30,"interval":1}"#.into())
            } else {
                (404, "{}".into())
            }
        }).await;
        let directory = tempfile::tempdir().unwrap();
        let client =
            ArtifactClient::for_origin(directory.path().canonicalize().unwrap(), &origin).unwrap();
        assert!(matches!(
            client.start_device_login().await,
            Err(ArtifactError::InvalidResponse)
        ));
    }
}

#[cfg(test)]
mod protocol_regression_tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    async fn serve_one(listener: TcpListener, response: String) {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut bytes = [0_u8; 2048];
        assert!(stream.read(&mut bytes).await.unwrap() > 0);
        stream.write_all(response.as_bytes()).await.unwrap();
    }

    #[tokio::test]
    async fn bearer_is_not_sent_to_a_redirect_target() {
        let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_url = format!("http://{}/stolen", target.local_addr().unwrap());
        let service = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", service.local_addr().unwrap());
        let redirect_response = format!(
            "HTTP/1.1 302 Found\r\nLocation: {target_url}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        tokio::spawn(serve_one(service, redirect_response));
        let directory = tempfile::tempdir().unwrap();
        let client =
            ArtifactClient::for_origin(directory.path().canonicalize().unwrap(), &origin).unwrap();
        client
            .save(&StoredCredential {
                origin: client.origin().into(),
                access_token: "secret-for-service-only".into(),
                expires_at: now_seconds() + 60,
                user: ArtifactUser {
                    id: "owner".into(),
                    name: "Owner".into(),
                    email: "owner@example.test".into(),
                },
            })
            .await
            .unwrap();
        assert!(matches!(
            client.status().await,
            Err(ArtifactError::Http(302))
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(200), target.accept())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn slow_down_increases_poll_interval_and_expired_token_stops() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let service_origin = origin.clone();
        let polls = Arc::new(Mutex::new(Vec::new()));
        let recorded_polls = polls.clone();
        tokio::spawn(async move {
            for request_number in 0..3 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0_u8; 2048];
                assert!(stream.read(&mut request).await.unwrap() > 0);
                let (status, body) = match request_number {
                    0 => (
                        200,
                        format!(
                            r#"{{"device_code":"opaque","user_code":"CODE","verification_uri":"{service_origin}/device?code=CODE","expires_in":30,"interval":1}}"#
                        ),
                    ),
                    1 => {
                        recorded_polls
                            .lock()
                            .unwrap()
                            .push(tokio::time::Instant::now());
                        (
                            400,
                            r#"{"error":{"code":"slow_down","message":"wait"}}"#.into(),
                        )
                    }
                    _ => {
                        recorded_polls
                            .lock()
                            .unwrap()
                            .push(tokio::time::Instant::now());
                        (
                            400,
                            r#"{"error":{"code":"expired_token","message":"expired"}}"#.into(),
                        )
                    }
                };
                let response = format!(
                    "HTTP/1.1 {status} Result\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let directory = tempfile::tempdir().unwrap();
        let client =
            ArtifactClient::for_origin(directory.path().canonicalize().unwrap(), &origin).unwrap();
        let device = client.start_device_login().await.unwrap();
        assert!(matches!(
            client
                .finish_device_login(device, &CancellationToken::new())
                .await,
            Err(ArtifactError::Expired)
        ));
        {
            let polls = polls.lock().unwrap();
            assert_eq!(polls.len(), 2);
            assert!(polls[1].duration_since(polls[0]) >= Duration::from_secs(6));
        }
        assert!(client.status().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn unauthorized_session_invalidates_only_the_matching_local_credential() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(serve_one(
            listener,
            "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
        ));
        let directory = tempfile::tempdir().unwrap();
        let client =
            ArtifactClient::for_origin(directory.path().canonicalize().unwrap(), &origin).unwrap();
        client
            .save(&StoredCredential {
                origin: client.origin().into(),
                access_token: "revoked-token".into(),
                expires_at: now_seconds() + 60,
                user: ArtifactUser {
                    id: "owner".into(),
                    name: "Owner".into(),
                    email: "owner@example.test".into(),
                },
            })
            .await
            .unwrap();
        assert!(client.status().await.unwrap().is_none());
        assert!(client.read_credential().unwrap().is_none());
    }

    #[tokio::test]
    async fn logout_response_cannot_remove_a_new_login_saved_while_request_is_in_flight() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let (request_seen, seen) = tokio::sync::oneshot::channel();
        let (release, response_allowed) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = [0_u8; 2048];
            assert!(stream.read(&mut bytes).await.unwrap() > 0);
            request_seen.send(()).unwrap();
            response_allowed.await.unwrap();
            let body = r#"{"revoked":true}"#;
            stream.write_all(format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            ).as_bytes()).await.unwrap();
        });
        let directory = tempfile::tempdir().unwrap();
        let client = Arc::new(
            ArtifactClient::for_origin(directory.path().canonicalize().unwrap(), &origin).unwrap(),
        );
        client
            .save(&StoredCredential {
                origin: client.origin().into(),
                access_token: "old-token".into(),
                expires_at: now_seconds() + 60,
                user: ArtifactUser {
                    id: "old-user".into(),
                    name: "Old".into(),
                    email: "old@example.test".into(),
                },
            })
            .await
            .unwrap();
        let signing_out = client.clone();
        let logout = tokio::spawn(async move { signing_out.logout().await });
        seen.await.unwrap();
        client
            .save(&StoredCredential {
                origin: client.origin().into(),
                access_token: "new-token".into(),
                expires_at: now_seconds() + 60,
                user: ArtifactUser {
                    id: "new-user".into(),
                    name: "New".into(),
                    email: "new@example.test".into(),
                },
            })
            .await
            .unwrap();
        release.send(()).unwrap();
        assert!(matches!(
            logout.await.unwrap().unwrap(),
            LogoutOutcome::NewerLoginPresent
        ));
        assert_eq!(
            client.read_credential().unwrap().unwrap().access_token,
            "new-token"
        );
    }

    #[tokio::test]
    async fn uncertain_remote_logout_keeps_credential_for_explicit_retry() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = [0_u8; 2048];
            assert!(stream.read(&mut bytes).await.unwrap() > 0);
        });
        let directory = tempfile::tempdir().unwrap();
        let client =
            ArtifactClient::for_origin(directory.path().canonicalize().unwrap(), &origin).unwrap();
        client
            .save(&StoredCredential {
                origin: client.origin().into(),
                access_token: "maybe-live-token".into(),
                expires_at: now_seconds() + 60,
                user: ArtifactUser {
                    id: "owner".into(),
                    name: "Owner".into(),
                    email: "owner@example.test".into(),
                },
            })
            .await
            .unwrap();
        assert!(matches!(
            client.logout().await,
            Err(ArtifactError::RevocationUnknown)
        ));
        assert_eq!(
            client.read_credential().unwrap().unwrap().access_token,
            "maybe-live-token"
        );
    }
}

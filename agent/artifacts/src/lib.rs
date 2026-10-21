//! Client of the independent Tickloop Artifacts service: device login, stored credentials and the catalog.

use std::{
    fs::{File, OpenOptions},
    io::{self, Read},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use reqwest::{Client, Method, Url, header, redirect::Policy};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use tickloop_agent_base::{
    network::{HttpPolicy, build_http_client, is_loopback},
    private_fs::{
        OpenTarget, PRIVATE_DIRECTORY_MODE, PRIVATE_FILE_MODE, open_without_following_links,
        replace_file_atomically, verify_private_directory,
    },
};

mod accounts;
mod catalog;
#[cfg(test)]
mod credential_tests;
mod credentials;
mod feedback;
mod images;
#[cfg(test)]
mod migration_tests;
pub use catalog::{AgentLinkMetadata, ArtifactMetadata, ArtifactOperation, ArtifactPage};
pub use credentials::ArtifactCredentials;
pub use feedback::{AgentLink, AgentLinkRequest, ClaimedFeedback, FeedbackDelivery, FeedbackPage};
pub use images::{
    ArtifactImage, IMAGE_ARTIFACT_FORMAT, MAX_IMAGE_BYTES, MAX_SVG_BYTES, artifact_image_type,
};

/// One Artifact type: the format name catalogs and Artifact Ticks store, the name the MCP tools
/// use, and the type name people see.
pub struct ArtifactType {
    pub stored_format: &'static str,
    pub tool_format: &'static str,
    pub name: &'static str,
}

pub static ARTIFACT_TYPES: [ArtifactType; 3] = [
    ArtifactType {
        stored_format: "markdown",
        tool_format: "doc",
        name: "Doc",
    },
    ArtifactType {
        stored_format: "html",
        tool_format: "page",
        name: "Page",
    },
    ArtifactType {
        stored_format: "slides",
        tool_format: "slides",
        name: "Slides",
    },
];

/// The type of an Artifact whose catalog or Tick format is `stored_format`.
pub fn artifact_type(stored_format: &str) -> Option<&'static ArtifactType> {
    ARTIFACT_TYPES
        .iter()
        .find(|artifact_type| artifact_type.stored_format == stored_format)
}

/// The type an MCP tool result names by `tool_format`.
pub fn artifact_type_for_tool(tool_format: &str) -> Option<&'static ArtifactType> {
    ARTIFACT_TYPES
        .iter()
        .find(|artifact_type| artifact_type.tool_format == tool_format)
}

pub const DEFAULT_ORIGIN: &str = "https://tickloop.ai";
/// Every page, API route and the MCP endpoint of Tickloop Artifacts live under this path of its origin.
pub const APP_BASE_PATH: &str = "/artifact";
const BASE62_ALPHABET: &[u8; 62] =
    b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
/// 62^22 exceeds 2^128, so every UUID fits in 22 base62 digits, left-padded with "0".
const SHORT_ID_LENGTH: usize = 22;
const ORIGIN_ENV: &str = "TICKLOOP_ARTIFACT_ORIGIN";
const MAX_RESPONSE_BYTES: usize = 16 * 1024;
const MAX_CREDENTIAL_BYTES: usize = 16 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const LOCK_TIMEOUT: Duration = Duration::from_secs(5);
const LOCK_RETRY: Duration = Duration::from_millis(25);
const USER_AGENT: &str = concat!(
    "Tickloop-Agent/",
    env!("CARGO_PKG_VERSION"),
    " ArtifactClient/1"
);

#[derive(Debug, thiserror::Error)]
pub enum ArtifactError {
    #[error(
        "invalid Tickloop Artifacts origin; use an HTTPS origin or an HTTP loopback test endpoint"
    )]
    InvalidOrigin,
    #[error("login interrupted, try again")]
    LoginInterrupted,
    #[error("{0}")]
    Login(tick_oauth::LoginError),
    #[error("Tickloop Artifacts connection failed")]
    Connection,
    #[error("Tickloop Artifacts returned an invalid response")]
    InvalidResponse,
    #[error("Tickloop Accounts authorization was denied")]
    AccessDenied,
    #[error("Tickloop Accounts device code expired")]
    Expired,
    #[error("Tickloop Accounts login cancelled")]
    Cancelled,
    #[error("Artifact credentials changed while verifying the account")]
    AccountChanged,
    #[error("Tickloop Artifacts credential storage is unavailable or unsafe")]
    Storage,
    /// The service refused the request with an error envelope; `message` is its explanation,
    /// empty when the envelope gives none.
    #[error("Tickloop Artifacts returned HTTP {status}: {code}")]
    Service {
        status: u16,
        code: String,
        message: String,
        reason: Option<String>,
    },
    #[error("Tickloop Artifacts modification outcome unknown; operation {operation_id}: {outcome}")]
    OutcomeUnknown {
        operation_id: String,
        outcome: String,
    },
    #[error("Tickloop Artifacts returned HTTP {0}")]
    Http(u16),
    #[error(
        "Tickloop Artifacts remote revocation is unconfirmed; local credential is retained for an explicit retry"
    )]
    RevocationUnknown,
    #[error("Artifact credentials are managed by the host")]
    HostManagedCredentials,
}

pub enum LogoutOutcome {
    AlreadySignedOut,
    Removed,
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

pub struct DeviceAuthorization {
    login: tick_oauth::DeviceLogin,
    pub user_code: String,
    pub verification_uri: String,
}

impl DeviceAuthorization {
    pub fn expires_in(&self) -> Duration {
        self.login.remaining()
    }
}

/// Decodes a service ID from its UUID or fixed-width base62 representation.
pub fn parse_service_id(value: &str) -> Option<Uuid> {
    if value.len() == 36 {
        return Uuid::parse_str(value).ok();
    }
    if value.len() != SHORT_ID_LENGTH {
        return None;
    }
    let mut decoded = 0_u128;
    for digit in value.bytes() {
        let digit = BASE62_ALPHABET
            .iter()
            .position(|candidate| *candidate == digit)?;
        decoded = decoded.checked_mul(62)?.checked_add(digit as u128)?;
    }
    Some(Uuid::from_u128(decoded))
}

/// The stable local identity of an Artifact, independent of its service representation.
pub fn canonical_artifact_id(value: &str) -> String {
    parse_service_id(value).map_or_else(|| value.to_owned(), |id| id.to_string())
}

fn deserialize_service_id<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Uuid, D::Error> {
    let value = String::deserialize(deserializer)?;
    parse_service_id(&value)
        .ok_or_else(|| serde::de::Error::custom("expected a UUID or 22-character base62 ID"))
}

/// The URL form of an artifact ID: a UUID becomes the service's 22-character base62 short ID;
/// any other identifier passes through unchanged.
pub fn short_artifact_id(artifact_id: &str) -> String {
    let hyphenated = artifact_id.len() == 36;
    let Some(uuid) = Uuid::parse_str(artifact_id).ok().filter(|_| hyphenated) else {
        return artifact_id.to_owned();
    };
    let mut value = uuid.as_u128();
    let mut digits = [BASE62_ALPHABET[0]; SHORT_ID_LENGTH];
    for digit in digits.iter_mut().rev() {
        *digit = BASE62_ALPHABET[(value % 62) as usize];
        value /= 62;
    }
    String::from_utf8(digits.to_vec()).expect("base62 digits are ASCII")
}

/// A canonical UUID and its public short ID identify the same artifact.
pub fn artifact_ids_match(expected: &str, actual: &str) -> bool {
    short_artifact_id(expected) == short_artifact_id(actual)
}

/// The page address of one artifact on a service origin.
pub fn artifact_url(origin: &str, artifact_id: &str) -> String {
    format!("{origin}{APP_BASE_PATH}/{}", short_artifact_id(artifact_id))
}

/// The service's MCP endpoint on a service origin.
pub fn mcp_endpoint(origin: &str) -> String {
    format!("{origin}{APP_BASE_PATH}/mcp")
}

struct ArtifactCredential {
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
    hosted_credentials: Option<std::sync::Arc<credentials::HostedCredentials>>,
}

#[derive(Clone)]
pub struct CredentialSnapshot {
    pub origin: String,
    pub account_id: String,
    pub access_token: String,
}

impl CredentialSnapshot {
    pub fn identity_matches(&self, other: &Self) -> bool {
        self.origin == other.origin && self.account_id == other.account_id
    }
}

impl ArtifactClient {
    pub fn credential_snapshot(&self) -> Result<Option<CredentialSnapshot>, ArtifactError> {
        if let Some(source) = &self.hosted_credentials {
            return Ok(source.snapshot());
        }
        Ok(self
            .read_credential()?
            .filter(|saved| saved.expires_at > now_seconds())
            .map(|saved| CredentialSnapshot {
                origin: saved.origin,
                account_id: saved.user.id,
                access_token: saved.access_token,
            }))
    }

    /// The signed-in account, also while its resource token awaits renewal.
    pub fn signed_in_account_id(&self) -> Result<Option<String>, ArtifactError> {
        if let Some(source) = &self.hosted_credentials {
            return Ok(source.snapshot().map(|snapshot| snapshot.account_id));
        }
        Ok(self.read_account()?.map(|account| account.user.id))
    }

    pub async fn invalidate_credential(&self, token: &str) -> Result<(), ArtifactError> {
        if let Some(source) = &self.hosted_credentials {
            return source.invalidate(token).await;
        }
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
            hosted_credentials: None,
        })
    }

    pub fn origin(&self) -> &str {
        self.origin.as_str().trim_end_matches('/')
    }

    fn url(&self, path: &str) -> Result<Url, ArtifactError> {
        self.origin
            .join(&format!("{APP_BASE_PATH}{path}"))
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
        if self.is_host_managed() {
            return Err(ArtifactError::HostManagedCredentials);
        }
        let path = self.state_root.join("artifacts").join("auth");
        open_without_following_links(
            &path,
            OpenTarget::Directory {
                create_mode: Some(PRIVATE_DIRECTORY_MODE),
            },
        )
        .map_err(|_| ArtifactError::Storage)?;
        verify_private_directory(&path, "Tickloop Artifacts credential directory")
            .map_err(|_| ArtifactError::Storage)?;
        Ok(path)
    }

    fn credential_path(&self) -> PathBuf {
        let digest = Sha256::digest(self.accounts_issuer().as_bytes());
        let name = digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        self.state_root
            .join("artifacts")
            .join("auth")
            .join(format!("accounts-{name}.json"))
    }

    fn read_account(&self) -> Result<Option<accounts::AccountCredential>, ArtifactError> {
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
        let credential: accounts::AccountCredential =
            serde_json::from_slice(&bytes).map_err(|_| ArtifactError::Storage)?;
        if credential.oauth.issuer != self.accounts_issuer()
            || credential.oauth.client_id != accounts::CLIENT_ID
            || credential.oauth.access_token.is_empty()
            || !credential.oauth.token_type.eq_ignore_ascii_case("Bearer")
            || credential.oauth.refresh_token().is_none()
            || credential.user.id.is_empty()
            || credential
                .resources
                .values()
                .any(|token| token.access_token.is_empty())
        {
            return Err(ArtifactError::Storage);
        }
        Ok(Some(credential))
    }

    fn read_credential(&self) -> Result<Option<ArtifactCredential>, ArtifactError> {
        Ok(self.read_account()?.and_then(|account| {
            account
                .resources
                .get(&self.resource())
                .map(|token| ArtifactCredential {
                    origin: self.origin().to_owned(),
                    access_token: token.access_token.clone(),
                    expires_at: token.expires_at,
                    user: account.user,
                })
        }))
    }

    fn save_account(&self, credential: &accounts::AccountCredential) -> Result<(), ArtifactError> {
        let bytes = serde_json::to_vec(credential).map_err(|_| ArtifactError::Storage)?;
        if bytes.len() > MAX_CREDENTIAL_BYTES {
            return Err(ArtifactError::Storage);
        }
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

    fn remove_legacy_credential(&self) -> Result<(), ArtifactError> {
        let name = format!("{:x}.json", Sha256::digest(self.origin().as_bytes()));
        let path = self.credential_directory()?.join(name);
        match open_without_following_links(&path, OpenTarget::Entry) {
            Ok(file) => {
                let metadata = file.metadata().map_err(|_| ArtifactError::Storage)?;
                if !metadata.is_file() || metadata.uid() != unsafe { libc::getuid() } {
                    return Err(ArtifactError::Storage);
                }
                std::fs::remove_file(path).map_err(|_| ArtifactError::Storage)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(ArtifactError::Storage),
        }
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
                Ok(()) => {
                    self.remove_legacy_credential()?;
                    return Ok(file);
                }
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

/// One URL path segment naming an artifact or request.
fn path_segment(value: &str) -> Result<&str, ArtifactError> {
    if value.is_empty()
        || value.contains(['/', '?', '#', '%'])
        || value.chars().any(char::is_control)
    {
        return Err(ArtifactError::InvalidResponse);
    }
    Ok(value)
}

/// The code, message and optional conflict reason of a service error envelope `{"error":{"code","message"}}`.
fn service_error(envelope: &serde_json::Value) -> (String, String, Option<String>) {
    let field = |name: &str| {
        envelope
            .pointer(&format!("/error/{name}"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    };
    (
        field("code").unwrap_or_else(|| "invalid_response".into()),
        field("message").unwrap_or_default(),
        field("reason"),
    )
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
    use std::sync::{Arc, Mutex};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    pub(super) async fn server(
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

    pub(super) async fn catalog_client(origin: &str) -> (tempfile::TempDir, ArtifactClient) {
        let directory = tempfile::tempdir().unwrap();
        let client =
            ArtifactClient::for_origin(directory.path().canonicalize().unwrap(), origin).unwrap();
        let _lock = client.lock().await.unwrap();
        client
            .save_account(&accounts::AccountCredential {
                oauth: tick_oauth::OAuthToken {
                    issuer: client.accounts_issuer(),
                    client_id: accounts::CLIENT_ID.into(),
                    access_token: "userinfo-token".into(),
                    token_type: "Bearer".into(),
                    refresh_token: Some("refresh-token".into()),
                    scope: None,
                    expires_at: now_seconds() + 600,
                    token_endpoint: format!("{}/token", client.accounts_issuer()),
                    revocation_endpoint: Some(format!("{}/revoke", client.accounts_issuer())),
                },
                user: ArtifactUser {
                    id: "owner".into(),
                    name: "Owner".into(),
                    email: "owner@example.test".into(),
                },
                resources: std::collections::BTreeMap::from([(
                    client.resource(),
                    accounts::ResourceToken {
                        access_token: "test-token".into(),
                        expires_at: now_seconds() + 600,
                    },
                )]),
            })
            .unwrap();
        (directory, client)
    }

    #[tokio::test]
    async fn catalog_reads_accept_canonical_ids_for_short_ids_but_reject_other_artifacts() {
        let (origin, _) = server(|_, _, _| {
            (200, serde_json::json!({"id":"e1b85a39-a57b-4c28-a8a5-ae9233aead3e","title":"Fixture","format":"markdown","access_role":"owner"}).to_string())
        }).await;
        let (_directory, client) = catalog_client(&origin).await;
        let metadata = client.read("6rvWPvOnhVABug7llbHb26").await.unwrap();
        assert_eq!(metadata.id, "e1b85a39-a57b-4c28-a8a5-ae9233aead3e");
        assert_eq!(
            metadata.url,
            format!("{origin}/artifact/6rvWPvOnhVABug7llbHb26")
        );
        assert!(matches!(
            client.read("another-artifact").await,
            Err(ArtifactError::InvalidResponse)
        ));
        assert!(matches!(
            client.read("e1b85a39a57b4c28a8a5ae9233aead3e").await,
            Err(ArtifactError::InvalidResponse)
        ));
        assert_eq!(
            client
                .read("E1B85A39-A57B-4C28-A8A5-AE9233AEAD3E")
                .await
                .unwrap()
                .id,
            metadata.id
        );
    }

    #[tokio::test]
    async fn catalog_allows_larger_pages_but_invalidates_only_rejected_credentials() {
        let (origin, requests) = server(|request, _, _| {
            if request.starts_with("GET /artifact/api/v1/artifacts?") {
                (200, serde_json::json!({"artifacts":[{"id":"doc","title":"t".repeat(20000),"format":"markdown","access_role":"owner","url":"https://docs.example/artifact/doc"}],"next_cursor":"next"}).to_string())
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
        assert!(requests[1].starts_with(&format!(
            "GET /artifact/api/v1/artifacts/doc/operations/{id} "
        )));
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
            let artifact = serde_json::json!({"id":"doc","title":"t".repeat(size),"format":"markdown","access_role":"owner","url":"https://docs.example/artifact/doc"});
            (200, if request.starts_with("GET /artifact/api/v1/artifacts/pinned ") || request.contains("q=oversize") { serde_json::json!({"artifacts":[artifact],"next_cursor":null}).to_string() } else { artifact.to_string() })
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

    pub(super) fn png_bytes() -> Vec<u8> {
        [
            &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a][..],
            b"pixels",
        ]
        .concat()
    }

    pub(super) fn image_receipt(bytes: &[u8]) -> serde_json::Value {
        serde_json::json!({"asset_id":"asset-1","ref":"asset:asset-1","content_type":"image/png","bytes":bytes.len(),"sha256":format!("{:x}", Sha256::digest(bytes)),"width":4,"height":null})
    }

    #[tokio::test]
    async fn an_upload_sends_the_raw_image_under_its_operation_and_returns_its_reference() {
        let bytes = png_bytes();
        let receipt = image_receipt(&bytes);
        let (origin, requests) = server(move |_, _, _| (201, receipt.to_string())).await;
        let (_directory, client) = catalog_client(&origin).await;
        let credential = client.credential_snapshot().unwrap().unwrap();
        let operation_id = Uuid::new_v4();
        let image = client
            .upload_image(
                "slides-1",
                "image/png",
                bytes.clone(),
                operation_id,
                &credential,
            )
            .await
            .unwrap();
        assert_eq!(image.reference, "asset:asset-1");
        assert_eq!((image.width, image.height), (Some(4), None));
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with(&format!(
            "POST /artifact/api/v1/artifacts/slides-1/assets?operation_id={operation_id} "
        )));
        let headers = requests[0].to_ascii_lowercase();
        assert!(headers.contains("content-type: image/png"));
        assert!(headers.contains("authorization: bearer test-token"));
        assert!(requests[0].ends_with(&*String::from_utf8_lossy(&bytes)));
    }

    #[tokio::test]
    async fn a_lost_upload_response_asks_for_its_operation_once_without_sending_it_again() {
        let bytes = png_bytes();
        let receipt = image_receipt(&bytes);
        let (origin, requests) = server(move |request, _, _| {
            if request.starts_with("POST ") {
                (503, r#"{"error":{"code":"storage_unavailable"}}"#.into())
            } else {
                let operation_id = request.split_whitespace().nth(1).unwrap().rsplit('/').next().unwrap();
                (200, serde_json::json!({"operation_id":operation_id,"status":"committed","result":receipt}).to_string())
            }
        })
        .await;
        let (_directory, client) = catalog_client(&origin).await;
        let credential = client.credential_snapshot().unwrap().unwrap();
        let operation_id = Uuid::new_v4();
        let image = client
            .upload_image("slides-1", "image/png", bytes, operation_id, &credential)
            .await
            .unwrap();
        assert_eq!(image.asset_id, "asset-1");
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[1].starts_with(&format!(
            "GET /artifact/api/v1/artifacts/slides-1/operations/{operation_id} "
        )));
    }

    #[tokio::test]
    async fn uploads_report_refusals_and_unconfirmed_or_mismatched_receipts_without_retrying() {
        let bytes = png_bytes();
        let (origin, requests) = server(|request, _, _| {
            if request.contains("assets?operation_id=") && request.contains("refused") {
                (413, r#"{"error":{"code":"asset_limit","message":"full"}}"#.into())
            } else if request.starts_with("POST ") {
                (201, serde_json::json!({"asset_id":"asset-1","ref":"asset:asset-1","content_type":"image/jpeg","bytes":1,"sha256":"0","width":null,"height":null}).to_string())
            } else {
                (200, r#"{"operation_id":"other","status":"committed"}"#.into())
            }
        })
        .await;
        let (_directory, client) = catalog_client(&origin).await;
        let credential = client.credential_snapshot().unwrap().unwrap();
        assert!(matches!(
            client
                .upload_image("refused", "image/png", bytes.clone(), Uuid::new_v4(), &credential)
                .await,
            Err(ArtifactError::Service { status: 413, code, message, .. }) if code == "asset_limit" && message == "full"
        ));
        assert_eq!(requests.lock().unwrap().len(), 1);
        assert!(matches!(
            client
                .upload_image("slides-1", "image/png", bytes, Uuid::new_v4(), &credential)
                .await,
            Err(ArtifactError::OutcomeUnknown { .. })
        ));
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests[2].starts_with("GET /artifact/api/v1/artifacts/slides-1/operations/"));
    }

    #[tokio::test]
    async fn a_receipt_for_the_stored_file_without_its_metadata_still_confirms_the_upload() {
        let sent = [png_bytes(), b"tEXt camera serial".to_vec()].concat();
        let stored = image_receipt(&png_bytes());
        let receipt = stored.clone();
        let (origin, requests) = server(move |request, _, _| {
            if request.contains("slides-lost") && request.starts_with("POST ") {
                (503, r#"{"error":{"code":"storage_unavailable"}}"#.into())
            } else if request.starts_with("POST ") {
                (201, receipt.to_string())
            } else {
                let operation_id = request.split_whitespace().nth(1).unwrap().rsplit('/').next().unwrap();
                (200, serde_json::json!({"operation_id":operation_id,"status":"committed","result":receipt}).to_string())
            }
        })
        .await;
        let (_directory, client) = catalog_client(&origin).await;
        let credential = client.credential_snapshot().unwrap().unwrap();
        for artifact in ["slides-1", "slides-lost"] {
            let image = client
                .upload_image(
                    artifact,
                    "image/png",
                    sent.clone(),
                    Uuid::new_v4(),
                    &credential,
                )
                .await
                .unwrap();
            assert_eq!(image.bytes, png_bytes().len() as u64, "{artifact}");
            assert_eq!(image.sha256, stored["sha256"], "{artifact}");
        }
        let requests = requests.lock().unwrap();
        assert_eq!(
            requests.len(),
            3,
            "one send each, and one receipt query for the lost response"
        );
        assert!(requests[2].starts_with("GET /artifact/api/v1/artifacts/slides-lost/operations/"));
    }

    #[tokio::test]
    async fn a_busy_upload_is_refused_with_the_service_message_and_not_retried() {
        let (origin, requests) = server(|_, _, _| {
            (
                429,
                r#"{"error":{"code":"asset_busy","message":"These Slides already have 4 uploads in progress"}}"#.into(),
            )
        })
        .await;
        let (_directory, client) = catalog_client(&origin).await;
        let credential = client.credential_snapshot().unwrap().unwrap();
        assert!(matches!(
            client
                .upload_image("slides-1", "image/png", png_bytes(), Uuid::new_v4(), &credential)
                .await,
            Err(ArtifactError::Service { status: 429, code, message, .. })
                if code == "asset_busy" && message == "These Slides already have 4 uploads in progress"
        ));
        assert_eq!(requests.lock().unwrap().len(), 1);
        assert!(client.credential_snapshot().unwrap().is_some());
    }

    #[tokio::test]
    async fn an_unauthorized_upload_invalidates_the_credential_without_asking_for_its_receipt() {
        let (origin, requests) =
            server(|_, _, _| (401, r#"{"error":{"code":"unauthorized"}}"#.into())).await;
        let (_directory, client) = catalog_client(&origin).await;
        let credential = client.credential_snapshot().unwrap().unwrap();
        assert!(matches!(
            client
                .upload_image(
                    "slides-1",
                    "image/png",
                    png_bytes(),
                    Uuid::new_v4(),
                    &credential
                )
                .await,
            Err(ArtifactError::AccessDenied)
        ));
        assert!(client.credential_snapshot().unwrap().is_none());
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with("POST /artifact/api/v1/artifacts/slides-1/assets?"));
    }

    #[test]
    fn artifact_types_map_stored_and_tool_formats_to_one_type_name() {
        for (stored, tool, name) in [
            ("markdown", "doc", "Doc"),
            ("html", "page", "Page"),
            ("slides", "slides", "Slides"),
        ] {
            assert_eq!(artifact_type(stored).unwrap().name, name);
            assert_eq!(artifact_type_for_tool(tool).unwrap().stored_format, stored);
        }
        assert!(artifact_type("doc").is_none());
        assert!(artifact_type_for_tool("markdown").is_none());
    }

    #[tokio::test]
    async fn catalog_conflict_is_reported_without_query_or_mutation_retry() {
        let (origin, requests) =
            server(|_, _, _| (409, r#"{"error":{"code":"catalog_conflict"}}"#.into())).await;
        let (_directory, client) = catalog_client(&origin).await;
        assert!(
            matches!(client.list("all", "", Some("old")).await, Err(ArtifactError::Service { status:409, code, .. }) if code == "catalog_conflict")
        );
        assert_eq!(requests.lock().unwrap().len(), 1);
    }

    pub(super) fn request_body(request: &str) -> serde_json::Value {
        serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap()
    }

    #[tokio::test]
    async fn feedback_delivery_endpoints_preserve_receiver_and_account_identity() {
        const DELIVERY: &str = "11111111-1111-4111-8111-111111111111";
        const FEEDBACK: &str = "22222222-2222-4222-8222-222222222222";
        let (origin, requests) = server(|request, _, _| {
            let line = request.lines().next().unwrap();
            let body = if line.starts_with("PUT ") {
                serde_json::json!({"artifact_id":"doc","binding_id":request_body(request)["binding_id"],"epoch":3})
            } else if line.starts_with("GET ") {
                serde_json::json!({"items":[{"delivery_id":DELIVERY,"feedback_id":FEEDBACK,"thread_id":"thread-1","created_at":"2026-10-08T10:00:00.000Z"}],"next_cursor":"2"})
            } else if line.contains("/claim ") {
                serde_json::json!({"delivery_id":DELIVERY,"feedback_id":FEEDBACK,"claim_id":"claim-1","fencing_token":"fence-1","lease_expires_at":"2026-10-08T10:01:30.000Z","input":{"artifact_id":"doc","thread_id":"thread-1","text":"@tickloop tighten"}})
            } else if line.contains("/ack ") {
                serde_json::json!({"delivery_id":DELIVERY,"accepted":true})
            } else if line.starts_with("DELETE ") {
                serde_json::json!({"removed":true})
            } else { serde_json::json!({"delivery_id":DELIVERY,"released":true}) };
            (200, body.to_string())
        }).await;
        let (_directory, client) = catalog_client(&origin).await;
        let request = AgentLinkRequest {
            binding_id: uuid::Uuid::new_v4(),
            client_instance_id: uuid::Uuid::new_v4(),
            chat_id: uuid::Uuid::new_v4(),
            loop_id: uuid::Uuid::new_v4(),
            history: tickloop_agent_protocol::ArtifactFeedbackHistory::All,
        };
        let link = client.link_agent("owner", "doc", &request).await.unwrap();
        let page = client
            .pending_feedback("owner", &link, Some("1"))
            .await
            .unwrap();
        assert_eq!(page.items[0].delivery_id, DELIVERY);
        assert_eq!(page.items[0].feedback_id, FEEDBACK);
        assert_eq!(page.next_cursor.as_deref(), Some("2"));
        let claim = client
            .claim_feedback("owner", &link, request.client_instance_id, DELIVERY)
            .await
            .unwrap();
        client
            .acknowledge_feedback("owner", "doc", &claim)
            .await
            .unwrap();
        client
            .release_feedback("owner", "doc", &claim)
            .await
            .unwrap();
        client.unlink_agent("owner", &link).await.unwrap();
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 6);
        assert!(requests.iter().all(|request| {
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer test-token")
        }));
        assert_eq!(request_body(&requests[0])["history"], "all");
        assert!(requests[1].starts_with(&format!(
            "GET /artifact/api/v1/agent/feedback?artifact_id=doc&binding_id={}&epoch=3&cursor=1 ",
            request.binding_id
        )));
        for request in &requests[3..5] {
            assert_eq!(
                request_body(request),
                serde_json::json!({"artifact_id":"doc","claim_id":"claim-1","fencing_token":"fence-1"})
            );
        }
        assert!(requests[5].starts_with("DELETE /artifact/api/v1/agent/links/doc "));
        assert_eq!(
            request_body(&requests[5]),
            serde_json::json!({"binding_id":request.binding_id,"epoch":3})
        );
    }

    #[tokio::test]
    async fn feedback_delivery_rejects_other_accounts_and_mismatched_replies() {
        let (origin, requests) = server(|request, _, _| {
            let body = if request.starts_with("PUT ") {
                serde_json::json!({"artifact_id":"doc","binding_id":uuid::Uuid::new_v4(),"epoch":1})
            } else {
                serde_json::json!({"request_id":"request-1","claim_id":"claim-1","fencing_token":"fence-1","lease_expires_at":"later","input":{"artifact_id":"other"}})
            };
            (200, body.to_string())
        })
        .await;
        let (_directory, client) = catalog_client(&origin).await;
        let request = AgentLinkRequest {
            binding_id: uuid::Uuid::new_v4(),
            client_instance_id: uuid::Uuid::new_v4(),
            chat_id: uuid::Uuid::new_v4(),
            loop_id: uuid::Uuid::new_v4(),
            history: tickloop_agent_protocol::ArtifactFeedbackHistory::NewOnly,
        };
        assert!(matches!(
            client.link_agent("someone-else", "doc", &request).await,
            Err(ArtifactError::AccountChanged)
        ));
        assert!(requests.lock().unwrap().is_empty());
        assert!(matches!(
            client.link_agent("owner", "doc", &request).await,
            Err(ArtifactError::InvalidResponse)
        ));
        let link = AgentLink {
            artifact_id: "doc".into(),
            binding_id: request.binding_id,
            epoch: 1,
        };
        assert!(matches!(
            client
                .claim_feedback("owner", &link, request.client_instance_id, "request-1")
                .await,
            Err(ArtifactError::InvalidResponse)
        ));
        assert!(matches!(
            client
                .claim_feedback("owner", &link, request.client_instance_id, "a/b")
                .await,
            Err(ArtifactError::InvalidResponse)
        ));
        assert_eq!(requests.lock().unwrap().len(), 2);
    }
}

#[cfg(test)]
mod address_tests {
    use super::*;

    #[test]
    fn artifact_urls_use_the_services_fixed_width_short_ids_under_its_base_path() {
        // The Artifacts service pins the same values, so both encoders agree.
        assert_eq!(
            short_artifact_id("e1b85a39-a57b-4c28-a8a5-ae9233aead3e"),
            "6rvWPvOnhVABug7llbHb26"
        );
        assert_eq!(
            short_artifact_id("00000000-0000-0000-0000-000000000000"),
            "0".repeat(22)
        );
        assert_eq!(short_artifact_id("doc"), "doc");
        assert_eq!(
            artifact_url(
                "https://tickloop.ai",
                "e1b85a39-a57b-4c28-a8a5-ae9233aead3e"
            ),
            "https://tickloop.ai/artifact/6rvWPvOnhVABug7llbHb26"
        );
        assert_eq!(
            mcp_endpoint("https://tickloop.ai"),
            "https://tickloop.ai/artifact/mcp"
        );
    }
}

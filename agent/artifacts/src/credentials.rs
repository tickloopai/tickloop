//! Host-owned credentials for one Chat. The regular constructors retain the file-backed Accounts login.

use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use reqwest::{RequestBuilder, Response, StatusCode, header};
use tokio::sync::Mutex;

use super::*;

const MAX_AUTH_RETRIES: usize = 1;

/// A trusted host's credential source, bound to the account and Chat it constructs the client for.
/// The host verifies token identity and scope; the client never decodes an unverified JWT.
#[async_trait]
pub trait ArtifactCredentials: Send + Sync {
    async fn bearer(&self) -> Result<Option<String>, ArtifactError>;

    /// Rejects this specific token, so a delayed 401 cannot evict a newer token.
    async fn invalidate(&self, _rejected_token: &str) -> Result<(), ArtifactError> {
        Ok(())
    }

    /// The same account's credentials for one Chat of the client's family, for requests that
    /// act as that Chat (its links); `None` keeps these.
    fn for_chat(&self, _chat_id: Uuid) -> Option<Arc<dyn ArtifactCredentials>> {
        None
    }
}

pub(super) struct HostedCredentials {
    account_id: String,
    source: Arc<dyn ArtifactCredentials>,
    cached: RwLock<Option<CredentialSnapshot>>,
    renewal: Mutex<()>,
    identity: Uuid,
}

impl HostedCredentials {
    pub(super) fn snapshot(&self) -> Option<CredentialSnapshot> {
        self.cached.read().unwrap().clone()
    }

    pub(super) async fn invalidate(&self, token: &str) -> Result<(), ArtifactError> {
        let _renewal = self.renewal.lock().await;
        {
            let mut cached = self.cached.write().unwrap();
            if cached
                .as_ref()
                .is_some_and(|current| current.access_token == token)
            {
                *cached = None;
            }
        }
        self.source.invalidate(token).await
    }
}

impl ArtifactClient {
    /// A host-owned client for one Chat. No login files are read or written by this client.
    /// `account_id` is the host's verified account identity, not a claim decoded from the bearer.
    pub fn with_credentials(
        origin: &str,
        account_id: impl Into<String>,
        source: Arc<dyn ArtifactCredentials>,
    ) -> Result<Self, ArtifactError> {
        let account_id = account_id.into();
        if account_id.is_empty() || account_id.chars().any(char::is_control) {
            return Err(ArtifactError::InvalidResponse);
        }
        let mut client = Self::for_origin(PathBuf::new(), origin)?;
        client.hosted_credentials = Some(Arc::new(HostedCredentials {
            account_id,
            source,
            cached: RwLock::new(None),
            renewal: Mutex::new(()),
            identity: Uuid::new_v4(),
        }));
        Ok(client)
    }

    /// This client acting as `chat_id`, when the host's credential source scopes credentials
    /// by Chat; otherwise the client itself.
    pub fn for_chat(&self, chat_id: Uuid) -> Self {
        let Some(hosted) = &self.hosted_credentials else {
            return self.clone();
        };
        let Some(source) = hosted.source.for_chat(chat_id) else {
            return self.clone();
        };
        let mut client = self.clone();
        client.hosted_credentials = Some(Arc::new(HostedCredentials {
            account_id: hosted.account_id.clone(),
            source,
            cached: RwLock::new(None),
            renewal: Mutex::new(()),
            identity: Uuid::new_v4(),
        }));
        client
    }

    pub fn is_host_managed(&self) -> bool {
        self.hosted_credentials.is_some()
    }

    pub(super) async fn host_snapshot(&self) -> Result<Option<CredentialSnapshot>, ArtifactError> {
        let Some(source) = &self.hosted_credentials else {
            return Err(ArtifactError::HostManagedCredentials);
        };
        let _renewal = source.renewal.lock().await;
        let snapshot = source
            .source
            .bearer()
            .await?
            .map(|access_token| CredentialSnapshot {
                origin: self.origin().into(),
                account_id: source.account_id.clone(),
                access_token,
            });
        if let Some(snapshot) = &snapshot {
            bearer(&snapshot.access_token)?;
            if snapshot.access_token.is_empty() {
                return Err(ArtifactError::InvalidResponse);
            }
        }
        *source.cached.write().unwrap() = snapshot.clone();
        Ok(snapshot)
    }

    /// Token renewal preserves the account authority: a host-owned client stays bound to its
    /// fixed account, a file-backed login to its origin and account.
    pub fn credentials_match(
        &self,
        expected: &CredentialSnapshot,
        current: &CredentialSnapshot,
    ) -> bool {
        if self.is_host_managed() {
            expected.origin == self.origin()
                && expected.origin == current.origin
                && expected.account_id == current.account_id
                && self
                    .hosted_credentials
                    .as_ref()
                    .is_some_and(|source| source.account_id == expected.account_id)
        } else {
            expected.identity_matches(current)
        }
    }

    /// Scopes MCP caches to the account, and a host-owned source to this client, independently
    /// of renewable token bytes.
    pub fn credential_key(&self, snapshot: &CredentialSnapshot) -> [u8; 32] {
        let mut digest = Sha256::new();
        digest.update(snapshot.origin.as_bytes());
        digest.update([0]);
        digest.update(snapshot.account_id.as_bytes());
        if let Some(source) = &self.hosted_credentials {
            digest.update([0]);
            digest.update(source.identity.as_bytes());
        }
        digest.finalize().into()
    }

    /// Sends a buffered request to this Artifacts origin. Only an explicit 401 is replayed,
    /// at most once and only for a host-owned source; operation IDs and bodies stay identical.
    pub async fn send_authenticated(
        &self,
        request: RequestBuilder,
        expected: &CredentialSnapshot,
    ) -> Result<Response, ArtifactError> {
        let (http, template) = request.build_split();
        let template = template.map_err(|_| ArtifactError::InvalidResponse)?;
        if template.url().origin() != self.origin.origin()
            || !(template.url().path() == APP_BASE_PATH
                || template
                    .url()
                    .path()
                    .starts_with(&format!("{APP_BASE_PATH}/")))
        {
            return Err(ArtifactError::InvalidOrigin);
        }
        let mut current = self
            .credential_snapshot()?
            .filter(|current| self.credentials_match(expected, current))
            .ok_or(ArtifactError::AccountChanged)?;
        for attempt in 0..=MAX_AUTH_RETRIES {
            let mut request = template.try_clone().ok_or(ArtifactError::InvalidResponse)?;
            request
                .headers_mut()
                .insert(header::AUTHORIZATION, bearer(&current.access_token)?);
            let response = http.execute(request).await.map_err(|error| {
                if error.is_timeout() {
                    ArtifactError::TimedOut
                } else {
                    ArtifactError::Connection
                }
            })?;
            if response.status() != StatusCode::UNAUTHORIZED {
                return Ok(response);
            }
            self.invalidate_credential(&current.access_token).await?;
            if !self.is_host_managed() || attempt == MAX_AUTH_RETRIES {
                return Err(ArtifactError::AccessDenied);
            }
            current = self
                .access_snapshot()
                .await?
                .filter(|current| self.credentials_match(expected, current))
                .ok_or(ArtifactError::AccessDenied)?;
        }
        unreachable!("each attempt returns or renews the rejected bearer")
    }
}

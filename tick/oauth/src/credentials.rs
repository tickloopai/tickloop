use async_trait::async_trait;

use crate::{LoginError, OAuthToken};

/// The caller owns persistence, permissions and locking. A login only replaces a token after
/// authorization succeeds; refresh callers must hold their store's lock across exchange and save.
#[async_trait]
pub trait CredentialStore: Send + Sync {
    async fn client_id(&self, _issuer: &str) -> Result<Option<String>, LoginError> {
        Ok(None)
    }

    async fn save_client_id(&self, _issuer: &str, _client_id: &str) -> Result<(), LoginError> {
        Err(LoginError::Failed(
            "this store does not support dynamic registration".into(),
        ))
    }

    async fn save_token(&self, token: &OAuthToken) -> Result<(), LoginError>;
}

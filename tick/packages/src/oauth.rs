use crate::{LockedOAuth, PackageCredentials, PackageError};
use async_trait::async_trait;
use tick_oauth::{CredentialStore, LoginError, OAuthToken};

#[async_trait]
impl CredentialStore for PackageCredentials {
    async fn client_id(&self, issuer: &str) -> Result<Option<String>, LoginError> {
        let issuer = issuer.to_owned();
        locked(self, move |files| files.client_id(&issuer)).await
    }

    async fn save_client_id(&self, issuer: &str, client_id: &str) -> Result<(), LoginError> {
        let (issuer, client_id) = (issuer.to_owned(), client_id.to_owned());
        locked(self, move |files| {
            files.set_client_id(&issuer, Some(&client_id))
        })
        .await
    }

    async fn save_token(&self, token: &OAuthToken) -> Result<(), LoginError> {
        let token = token.clone();
        locked(self, move |files| files.save_token(&token)).await
    }
}

async fn locked<T: Send + 'static>(
    credentials: &PackageCredentials,
    change: impl FnOnce(&LockedOAuth) -> Result<T, PackageError> + Send + 'static,
) -> Result<T, LoginError> {
    let credentials = credentials.clone();
    tokio::task::spawn_blocking(move || change(&credentials.lock_oauth()?))
        .await
        .map_err(|error| LoginError::Failed(error.to_string()))?
        .map_err(LoginError::from)
}

impl From<PackageError> for LoginError {
    fn from(error: PackageError) -> Self {
        Self::Failed(error.to_string())
    }
}

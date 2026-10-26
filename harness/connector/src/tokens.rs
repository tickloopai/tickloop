//! Using a saved login: the current access token, renewed 30 seconds before it expires or when
//! the server rejects it, and logout, which revokes the refresh token and keeps the client
//! registration.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tick_oauth::{OAuthToken, RefreshError};
use tokio_util::sync::CancellationToken;

use crate::credentials::{ConnectorCredential, CredentialError};
use crate::login::{LoginError, cancellable};
use crate::{AccessToken, Connector};

const RENEWAL_MARGIN: Duration = Duration::from_secs(30);

/// Why no access token is available. The texts never include server responses or tokens.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TokenError {
    /// There is no login for this server address, or the server no longer accepts it.
    #[error("the MCP server needs a login")]
    NeedsLogin,
    /// Renewal failed in a way a later attempt may not, such as a network failure.
    #[error("the MCP server's login could not be renewed right now")]
    RenewalFailed,
    #[error("the operation was cancelled")]
    Cancelled,
    #[error(transparent)]
    Credentials(#[from] CredentialError),
}

/// What logging out did at the authorization server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Logout {
    Revoked,
    /// The server offers no revocation; the token is only forgotten.
    NotRevocable,
    /// The server could not revoke it; the token is forgotten anyway.
    RevocationFailed,
    /// There was no token to log out.
    NotLoggedIn,
}

impl From<LoginError> for TokenError {
    fn from(error: LoginError) -> Self {
        match error {
            LoginError::Credentials(error) => Self::Credentials(error),
            _ => Self::Cancelled,
        }
    }
}

impl Connector {
    /// The token to send, renewed first when it expires within 30 seconds.
    pub async fn access_token(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<AccessToken, TokenError> {
        let token = self.saved_token(cancellation).await?;
        if !expires_soon(&token) || token.refresh_token().is_none() {
            return Ok(AccessToken(token.access_token));
        }
        self.renew(&token.access_token, cancellation).await
    }

    /// Renews after the server answered `rejected` with 401. Another Agent may already have
    /// renewed it, in which case its token is used.
    pub async fn renew_rejected(
        &self,
        rejected: &AccessToken,
        cancellation: &CancellationToken,
    ) -> Result<AccessToken, TokenError> {
        self.renew(rejected.secret(), cancellation).await
    }

    /// Revokes the refresh token where the server allows it and forgets the token, keeping the
    /// client registration for the next login.
    pub async fn logout(&self, cancellation: &CancellationToken) -> Result<Logout, TokenError> {
        let _lock = cancellable(cancellation, self.credentials.lock()).await??;
        let Some(mut credential) = cancellable(cancellation, self.credentials.load()).await??
        else {
            return Ok(Logout::NotLoggedIn);
        };
        let Some(token) = credential.token.take() else {
            return Ok(Logout::NotLoggedIn);
        };
        let outcome = if token.revocation_endpoint.is_none() {
            Logout::NotRevocable
        } else {
            match cancellable(
                cancellation,
                tick_oauth::revoke(self.token_client(&token), &token),
            )
            .await?
            {
                Ok(()) => Logout::Revoked,
                Err(_) => Logout::RevocationFailed,
            }
        };
        self.credentials.save(&credential).await?;
        Ok(outcome)
    }

    async fn saved_token(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<OAuthToken, TokenError> {
        let credential = cancellable(cancellation, self.credentials.load()).await??;
        self.token_of(credential).ok_or(TokenError::NeedsLogin)
    }

    /// The token of `credential` when it was granted for this server's address.
    fn token_of(&self, credential: Option<ConnectorCredential>) -> Option<OAuthToken> {
        credential
            .filter(|credential| credential.server_url == self.server_url.as_str())
            .and_then(|credential| credential.token)
    }

    async fn renew(
        &self,
        seen_access_token: &str,
        cancellation: &CancellationToken,
    ) -> Result<AccessToken, TokenError> {
        let _lock = cancellable(cancellation, self.credentials.lock()).await??;
        let credential = cancellable(cancellation, self.credentials.load()).await??;
        let resource = credential
            .as_ref()
            .and_then(|credential| credential.resource.clone());
        let token = self
            .token_of(credential.clone())
            .ok_or(TokenError::NeedsLogin)?;
        if token.access_token != seen_access_token && !expires_soon(&token) {
            return Ok(AccessToken(token.access_token));
        }
        let Some(refresh_token) = token.refresh_token().map(str::to_owned) else {
            self.forget_token(credential).await?;
            return Err(TokenError::NeedsLogin);
        };
        let renewal = tick_oauth::refresh_for_resource(
            self.token_client(&token),
            &token,
            &refresh_token,
            resource.as_deref(),
            |request| request,
        );
        match cancellable(cancellation, renewal).await? {
            Ok(renewed) => {
                let access_token = renewed.access_token.clone();
                let mut credential = credential.expect("a token implies a credential");
                credential.token = Some(renewed);
                self.credentials.save(&credential).await?;
                Ok(AccessToken(access_token))
            }
            Err(RefreshError::Refused(_)) => {
                let reread = self.credentials.load().await?;
                if let Some(current) = self.token_of(reread.clone())
                    && current.refresh_token() != Some(refresh_token.as_str())
                {
                    return Ok(AccessToken(current.access_token));
                }
                self.forget_token(reread).await?;
                Err(TokenError::NeedsLogin)
            }
            Err(RefreshError::Failed(_)) => Err(TokenError::RenewalFailed),
        }
    }

    async fn forget_token(
        &self,
        credential: Option<ConnectorCredential>,
    ) -> Result<(), CredentialError> {
        match credential {
            Some(mut credential) if credential.token.is_some() => {
                credential.token = None;
                self.credentials.save(&credential).await
            }
            _ => Ok(()),
        }
    }

    fn token_client(&self, token: &OAuthToken) -> &reqwest::Client {
        let address = reqwest::Url::parse(&token.token_endpoint).unwrap_or(self.server_url.clone());
        self.http.for_address(&address)
    }
}

fn expires_soon(token: &OAuthToken) -> bool {
    now_seconds().saturating_add(RENEWAL_MARGIN.as_secs()) >= token.expires_at
}

pub(crate) fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

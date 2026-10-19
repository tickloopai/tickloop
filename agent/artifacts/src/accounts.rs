use std::collections::BTreeMap;

use async_trait::async_trait;
use tick_oauth::{
    ClientRegistration, CredentialStore, DeviceLogin, LoginError, OAuthToken, RefreshError,
};

use super::*;

const ACCOUNTS_BASE_PATH: &str = "/account";
pub(crate) const CLIENT_ID: &str = "tickloop-cli";
const SCOPES: &str = "openid profile email offline_access artifacts.agent artifacts.console";
const REFRESH_MARGIN_SECONDS: u64 = 60;

#[derive(Serialize, Deserialize)]
pub(super) struct ResourceToken {
    pub(super) access_token: String,
    pub(super) expires_at: u64,
}

#[derive(Serialize, Deserialize)]
pub(super) struct AccountCredential {
    // This access token is for userinfo. Only its refresh token rotates when another resource
    // is refreshed; an Artifacts token must never replace the issuer-audience access token.
    pub(super) oauth: OAuthToken,
    pub(super) user: ArtifactUser,
    pub(super) resources: BTreeMap<String, ResourceToken>,
}

#[async_trait]
impl CredentialStore for ArtifactClient {
    async fn save_token(&self, token: &OAuthToken) -> Result<(), LoginError> {
        self.complete_login(token.clone(), false, &CancellationToken::new())
            .await
            .map(|_| ())
            .map_err(|error| LoginError::Failed(error.to_string()))
    }
}

impl ArtifactClient {
    pub(super) fn accounts_issuer(&self) -> String {
        format!("{}{ACCOUNTS_BASE_PATH}", self.origin())
    }

    pub(super) fn resource(&self) -> String {
        format!("{}{APP_BASE_PATH}", self.origin())
    }

    pub async fn start_device_login(&self) -> Result<DeviceAuthorization, ArtifactError> {
        let scopes = SCOPES
            .split_whitespace()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let login = DeviceLogin::start_with(
            self.http.clone(),
            self,
            &self.accounts_issuer(),
            &scopes,
            ClientRegistration::Preregistered(CLIENT_ID),
            &[],
        )
        .await
        .map_err(login_error)?;
        Ok(DeviceAuthorization {
            user_code: login.user_code().to_owned(),
            verification_uri: login.link().to_owned(),
            login,
        })
    }

    pub async fn finish_device_login(
        &self,
        device: DeviceAuthorization,
        cancellation: &CancellationToken,
    ) -> Result<ArtifactUser, ArtifactError> {
        let token = device
            .login
            .authorize(cancellation, |_| {})
            .await
            .map_err(login_error)?;
        self.complete_login(token, false, cancellation)
            .await?
            .ok_or(ArtifactError::AccountChanged)
    }

    /// A sign-in completed in the background never replaces an account signed in meanwhile.
    pub async fn finish_background_device_login(
        &self,
        device: DeviceAuthorization,
        cancellation: &CancellationToken,
    ) -> Result<Option<ArtifactUser>, ArtifactError> {
        let token = device
            .login
            .authorize(cancellation, |_| {})
            .await
            .map_err(login_error)?;
        self.complete_login(token, true, cancellation).await
    }

    async fn complete_login(
        &self,
        token: OAuthToken,
        only_if_absent: bool,
        cancellation: &CancellationToken,
    ) -> Result<Option<ArtifactUser>, ArtifactError> {
        if cancellation.is_cancelled() {
            return Err(ArtifactError::Cancelled);
        }
        if token.issuer != self.accounts_issuer()
            || token.client_id != CLIENT_ID
            || !token.token_type.eq_ignore_ascii_case("Bearer")
            || token.refresh_token().is_none()
        {
            return Err(ArtifactError::InvalidResponse);
        }
        let user = tick_oauth::user_info(&self.http, &token)
            .await
            .map_err(refresh_error)?;
        let mut account = AccountCredential {
            oauth: token,
            user: ArtifactUser {
                id: user.sub,
                name: user.name,
                email: user.email,
            },
            resources: BTreeMap::new(),
        };
        self.renew_resource(&mut account, &self.resource())
            .await
            .map_err(refresh_error)?;
        let _lock = self.lock().await?;
        if cancellation.is_cancelled() {
            let _ = tick_oauth::revoke(&self.http, &account.oauth).await;
            return Err(ArtifactError::Cancelled);
        }
        if only_if_absent && self.read_account()?.is_some() {
            // The unused device family should not outlive the abandoned sign-in.
            let _ = tick_oauth::revoke(&self.http, &account.oauth).await;
            return Ok(None);
        }
        self.save_account(&account)?;
        Ok(Some(account.user))
    }

    async fn renew_resource(
        &self,
        account: &mut AccountCredential,
        resource: &str,
    ) -> Result<(), RefreshError> {
        let token = tick_oauth::refresh_for_resource(
            &self.http,
            &account.oauth,
            account
                .oauth
                .refresh_token()
                .ok_or_else(|| RefreshError::Refused("invalid_grant".into()))?,
            Some(resource),
            |request| request,
        )
        .await?;
        if !token.token_type.eq_ignore_ascii_case("Bearer") {
            return Err(RefreshError::Failed(
                "the issuer returned an unsupported token type".into(),
            ));
        }
        account.oauth.refresh_token = token.refresh_token;
        account.resources.insert(
            resource.to_owned(),
            ResourceToken {
                access_token: token.access_token,
                expires_at: token.expires_at,
            },
        );
        Ok(())
    }

    /// Obtains a usable resource token under the issuer's cross-process lock. Snapshot checks
    /// for already-prepared operations stay synchronous and never replace their authority.
    pub async fn access_snapshot(&self) -> Result<Option<CredentialSnapshot>, ArtifactError> {
        let _lock = self.lock().await?;
        let Some(mut account) = self.read_account()? else {
            return Ok(None);
        };
        let resource = self.resource();
        if account.resources.get(&resource).is_none_or(|token| {
            token.expires_at <= now_seconds().saturating_add(REFRESH_MARGIN_SECONDS)
        }) {
            match self.renew_resource(&mut account, &resource).await {
                Ok(()) => self.save_account(&account)?,
                Err(RefreshError::Refused(code)) if invalidates_login(&code) => {
                    std::fs::remove_file(self.credential_path())
                        .map_err(|_| ArtifactError::Storage)?;
                    return Ok(None);
                }
                Err(error) => return Err(refresh_error(error)),
            }
        }
        self.credential_snapshot()
    }

    pub async fn status(&self) -> Result<Option<ArtifactUser>, ArtifactError> {
        Ok(self.verified_snapshot().await?.map(|(user, _)| user))
    }

    /// Verifies identity without blocking a new login, then returns its current resource credential under the lock.
    pub async fn verified_snapshot(
        &self,
    ) -> Result<Option<(ArtifactUser, CredentialSnapshot)>, ArtifactError> {
        let _lock = self.lock().await?;
        let Some(mut account) = self.read_account()? else {
            return Ok(None);
        };
        if account.oauth.expires_at <= now_seconds().saturating_add(REFRESH_MARGIN_SECONDS) {
            let refreshed = tick_oauth::refresh_for_resource(
                &self.http,
                &account.oauth,
                account
                    .oauth
                    .refresh_token()
                    .ok_or(ArtifactError::Storage)?,
                None,
                |request| request,
            )
            .await;
            match refreshed {
                Ok(token) if token.token_type.eq_ignore_ascii_case("Bearer") => {
                    account.oauth = token;
                    self.save_account(&account)?;
                }
                Ok(_) => return Err(ArtifactError::InvalidResponse),
                Err(RefreshError::Refused(code)) if invalidates_login(&code) => {
                    std::fs::remove_file(self.credential_path())
                        .map_err(|_| ArtifactError::Storage)?;
                    return Ok(None);
                }
                Err(error) => return Err(refresh_error(error)),
            }
        }
        drop(_lock);
        let verified = tick_oauth::user_info(&self.http, &account.oauth).await;
        let _lock = self.lock().await?;
        let Some(mut current) = self.read_account()? else {
            return Err(ArtifactError::AccountChanged);
        };
        if current.oauth.access_token != account.oauth.access_token
            || current.user.id != account.user.id
        {
            return Err(ArtifactError::AccountChanged);
        }
        match verified {
            Ok(user) if user.sub == current.user.id => {
                current.user = ArtifactUser {
                    id: user.sub,
                    name: user.name,
                    email: user.email,
                };
            }
            Ok(_) => return Err(ArtifactError::InvalidResponse),
            Err(RefreshError::Refused(code)) if invalidates_login(&code) => {
                std::fs::remove_file(self.credential_path()).map_err(|_| ArtifactError::Storage)?;
                return Ok(None);
            }
            Err(error) => return Err(refresh_error(error)),
        }
        let mut account = current;
        let resource = self.resource();
        if account.resources.get(&resource).is_none_or(|token| {
            token.expires_at <= now_seconds().saturating_add(REFRESH_MARGIN_SECONDS)
        }) {
            match self.renew_resource(&mut account, &resource).await {
                Ok(()) => {}
                Err(RefreshError::Refused(code)) if invalidates_login(&code) => {
                    std::fs::remove_file(self.credential_path())
                        .map_err(|_| ArtifactError::Storage)?;
                    return Ok(None);
                }
                Err(error) => return Err(refresh_error(error)),
            }
        }
        self.save_account(&account)?;
        let snapshot = self
            .credential_snapshot()?
            .ok_or(ArtifactError::AccountChanged)?;
        Ok(Some((account.user, snapshot)))
    }

    pub async fn logout(&self) -> Result<LogoutOutcome, ArtifactError> {
        let _lock = self.lock().await?;
        let Some(account) = self.read_account()? else {
            return Ok(LogoutOutcome::AlreadySignedOut);
        };
        if account.oauth.revocation_endpoint.is_none() {
            return Err(ArtifactError::RevocationUnknown);
        }
        tick_oauth::revoke(&self.http, &account.oauth)
            .await
            .map_err(|_| ArtifactError::RevocationUnknown)?;
        std::fs::remove_file(self.credential_path()).map_err(|_| ArtifactError::Storage)?;
        Ok(LogoutOutcome::Removed)
    }
}

fn login_error(error: LoginError) -> ArtifactError {
    match error {
        LoginError::Cancelled => ArtifactError::Cancelled,
        LoginError::Denied => ArtifactError::AccessDenied,
        LoginError::Expired => ArtifactError::Expired,
        LoginError::Refused { ref code, .. } if code == "invalid_grant" => {
            ArtifactError::LoginInterrupted
        }
        error => ArtifactError::Login(error),
    }
}

fn refresh_error(error: RefreshError) -> ArtifactError {
    match error {
        RefreshError::Refused(code) if code == "invalid_grant" => ArtifactError::LoginInterrupted,
        RefreshError::Refused(code) => ArtifactError::Login(LoginError::Refused {
            message: format!("Tickloop Accounts refused the request: {code}"),
            code,
        }),
        RefreshError::Failed(_) => ArtifactError::Connection,
    }
}

fn invalidates_login(code: &str) -> bool {
    matches!(
        code,
        "invalid_grant" | "invalid_client" | "invalid_token" | "HTTP 401 Unauthorized"
    )
}

#[cfg(test)]
#[path = "accounts_tests.rs"]
mod tests;

use longbridge::oauth::{OAuth, OAuthBuilder, OAuthResult, StoredToken, TokenStorage};

use super::{
    AuthEndpoints,
    device::{TokenResponse, http_client, response_json},
    storage::{AuthStorage, now_seconds},
};
use crate::{
    config::Region,
    error::{Error, Result},
};

pub(super) const SDK_REFRESH_WINDOW_SECONDS: u64 = 300;

struct SessionStorage {
    initial: StoredToken,
    disk: AuthStorage,
}

impl TokenStorage for SessionStorage {
    fn load(&self, client_id: &str) -> Option<StoredToken> {
        (self.initial.client_id == client_id).then(|| self.initial.clone())
    }
    fn save(&self, token: &StoredToken) -> OAuthResult<()> {
        self.disk.save(token)
    }
}

pub async fn authorized_oauth(
    storage: &AuthStorage,
    region: Region,
    endpoints: Option<AuthEndpoints>,
) -> Result<OAuth> {
    let endpoints = endpoints.unwrap_or_else(|| AuthEndpoints::for_region(region));
    let mut token = storage
        .token()?
        .ok_or_else(|| Error::auth("not authorized"))?
        .stored();
    // OAuthBuilder falls back to a browser callback after a failed refresh.
    // Refresh before building, then seed it with a validated in-memory snapshot.
    if token.expires_at <= now_seconds().saturating_add(SDK_REFRESH_WINDOW_SECONDS + 1) {
        let refresh = token
            .refresh_token
            .as_deref()
            .filter(|token| !token.is_empty())
            .ok_or_else(|| Error::auth("authorization expired"))?;
        let center = longbridge::DcRegion::from_credential(refresh);
        let base = if center == longbridge::DcRegion::Us {
            &endpoints.us
        } else {
            &endpoints.ap
        };
        let response = http_client()?
            .post(format!("{base}/token"))
            .header(longbridge::DC_REGION_HEADER, center.as_str())
            .form(&[
                ("client_id", token.client_id.as_str()),
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh),
            ])
            .send()
            .await
            .map_err(|_| Error::network("token refresh request failed"))?;
        if response.status() == reqwest::StatusCode::BAD_REQUEST
            || response.status() == reqwest::StatusCode::UNAUTHORIZED
        {
            return Err(Error::auth("authorization expired or revoked"));
        }
        let mut refreshed = response_json::<TokenResponse>(response, "token refresh")
            .await?
            .stored(token.client_id.clone())?;
        if refreshed.refresh_token.is_none() {
            refreshed.refresh_token = token.refresh_token;
        }
        storage
            .save(&refreshed)
            .map_err(|error| Error::usage(error.to_string()))?;
        token = refreshed;
    }
    if token.expires_at <= now_seconds().saturating_add(SDK_REFRESH_WINDOW_SECONDS + 1) {
        return Err(Error::auth(
            "token lifetime is too short to initialize the SDK",
        ));
    }
    let browser_attempt = tokio::sync::Notify::new();
    let builder = OAuthBuilder::new(token.client_id.clone())
        .refresh_token_urls(
            format!("{}/token", endpoints.ap),
            format!("{}/token", endpoints.us),
        )
        .map_err(|_| Error::usage("invalid authorization endpoint"))?
        .token_storage(SessionStorage {
            initial: token,
            disk: storage.clone(),
        });
    tokio::select! {
        result = builder.build(|_| browser_attempt.notify_one()) => result.map_err(|_| Error::auth("cannot load authorization")),
        _ = browser_attempt.notified() => Err(Error::auth("device login required")),
    }
}

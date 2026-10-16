//! Using the saved login: the token is renewed when it comes within the SDK's refresh window,
//! by whichever process gets there first. That process holds the token file's lock while it
//! asks the token endpoint and writes the result back, and every other process waiting on the
//! lock then reads the renewed token instead of asking again, since the server may rotate the
//! refresh token and accept the old one no more.

use longbridge::oauth::{
    OAuth, OAuthBuilder, OAuthError, OAuthResult, RefreshFuture, StoredToken, TokenRefresher,
    TokenStorage,
};
use tick_packages::{OAuthToken, PackageCredentials};
use tick_provider::error::{Error, Result};

use super::{
    blocking, is_due, lock, saved_token,
    server::{REFRESH_TOKEN_GRANT, Reply, Server},
};
use crate::{not_authorized, region::Region};

/// The Longbridge login an SDK client uses, and the network it reaches the API through.
pub struct Authorized {
    pub oauth: OAuth,
    pub region: Region,
}

/// An SDK client for the saved login, renewed first when it is due. It renews itself while it
/// runs through the token file, and never starts a browser login.
pub async fn authorized_oauth(credentials: &PackageCredentials) -> Result<Authorized> {
    let token = current_token(credentials).await?;
    if is_due(&token) {
        return Err(not_authorized(
            "token lifetime is too short to initialize the SDK",
        ));
    }
    // A US account's API is reached through the global network, whichever server it logged in
    // with.
    let region =
        if longbridge::DcRegion::from_credential(&token.access_token) == longbridge::DcRegion::Us {
            Region::Global
        } else {
            Region::of_issuer(&token.issuer)
        };
    let browser_attempt = tokio::sync::Notify::new();
    let builder = OAuthBuilder::new(token.client_id.clone())
        .token_storage(Snapshot(stored(&token)))
        .token_refresher(SavedToken(credentials.clone()));
    // Building loads the token just checked, so it neither renews it nor falls back to the
    // browser login a failed renewal would start.
    let oauth = tokio::select! {
        result = builder.build(|_| browser_attempt.notify_one()) => {
            result.map_err(|_| not_authorized("cannot load authorization"))?
        }
        _ = browser_attempt.notified() => return Err(not_authorized("device login required")),
    };
    Ok(Authorized { oauth, region })
}

/// The saved token, renewed first when it is due: under the token file's lock, unless another
/// process renewed it meanwhile.
pub async fn current_token(credentials: &PackageCredentials) -> Result<OAuthToken> {
    let token = saved_token(credentials)?;
    if !is_due(&token) {
        return Ok(token);
    }
    if token.refresh_token().is_none() {
        return Err(not_authorized("authorization expired"));
    }
    let files = lock(credentials).await?;
    let token = saved_token(credentials)?;
    if !is_due(&token) {
        return Ok(token);
    }
    let refresh_token = token
        .refresh_token()
        .ok_or_else(|| not_authorized("authorization expired"))?
        .to_owned();
    let renewed = renew(&token, &refresh_token).await?;
    let saved = renewed.clone();
    blocking(move || files.save_token(&saved)).await?;
    Ok(renewed)
}

/// Asks `token`'s token endpoint for a new access token (RFC 6749 section 6). The refresh token
/// and scope stay unless the server replaced them.
async fn renew(token: &OAuthToken, refresh_token: &str) -> Result<OAuthToken> {
    let server = Server::new(&token.issuer)?;
    let endpoint = server.endpoint(&token.token_endpoint, "token_endpoint")?;
    // Longbridge routes a credential to its data center by this header.
    let data_center = longbridge::DcRegion::from_credential(refresh_token);
    let request = server
        .http()
        .post(endpoint)
        .header(longbridge::DC_REGION_HEADER, data_center.as_str())
        .form(&[
            ("grant_type", REFRESH_TOKEN_GRANT),
            ("refresh_token", refresh_token),
            ("client_id", &token.client_id),
        ]);
    match server.request_token(request).await {
        Ok(Reply::Token(response)) => Ok(OAuthToken {
            refresh_token: response
                .refresh_token()
                .or_else(|| token.refresh_token.clone()),
            scope: response.scope.clone().or_else(|| token.scope.clone()),
            expires_at: response.expires_at(),
            access_token: response.access_token,
            token_type: response.token_type,
            ..token.clone()
        }),
        Ok(Reply::Refused(_)) => Err(not_authorized("authorization expired or revoked")),
        Ok(Reply::Unexpected(status))
            if status == reqwest::StatusCode::BAD_REQUEST
                || status == reqwest::StatusCode::UNAUTHORIZED =>
        {
            Err(not_authorized("authorization expired or revoked"))
        }
        Ok(Reply::Unexpected(status)) => Err(Error::network(format!(
            "token refresh failed: HTTP {status}"
        ))),
        Err(_) => Err(Error::network("token refresh request failed")),
    }
}

fn stored(token: &OAuthToken) -> StoredToken {
    StoredToken {
        client_id: token.client_id.clone(),
        access_token: token.access_token.clone(),
        refresh_token: token.refresh_token.clone(),
        expires_at: token.expires_at,
    }
}

/// The token an SDK client starts with. What the client renews is saved by [`SavedToken`]
/// already, so nothing is saved here.
struct Snapshot(StoredToken);

impl TokenStorage for Snapshot {
    fn load(&self, client_id: &str) -> Option<StoredToken> {
        (self.0.client_id == client_id).then(|| self.0.clone())
    }

    fn save(&self, _: &StoredToken) -> OAuthResult<()> {
        Ok(())
    }
}

/// Renews an SDK client's token through the saved token, as [`current_token`] does.
struct SavedToken(PackageCredentials);

impl TokenRefresher for SavedToken {
    fn refresh<'a>(&'a self, _: &'a StoredToken) -> RefreshFuture<'a> {
        Box::pin(async move {
            current_token(&self.0)
                .await
                .map(|token| stored(&token))
                .map_err(|error| OAuthError::RefreshTokenFailed {
                    message: error.message,
                })
        })
    }
}

/// An OAuth whose token enters the SDK's refresh window without a refresh token, so its next
/// `access_token` fails the way a lapsed login does, with no network involved.
#[cfg(test)]
pub(crate) async fn lapsing_oauth() -> OAuth {
    use super::{SDK_REFRESH_WINDOW_SECONDS, now_seconds};

    struct Memory(StoredToken);

    impl TokenStorage for Memory {
        fn load(&self, _: &str) -> Option<StoredToken> {
            Some(self.0.clone())
        }

        fn save(&self, _: &StoredToken) -> OAuthResult<()> {
            Ok(())
        }
    }

    let expires_at = now_seconds() + SDK_REFRESH_WINDOW_SECONDS + 1;
    let oauth = OAuthBuilder::new("client")
        .token_storage(Memory(StoredToken {
            client_id: "client".into(),
            access_token: "ap_lapsing".into(),
            refresh_token: None,
            expires_at,
        }))
        .build(|_| panic!("a token outside the refresh window never opens a browser"))
        .await
        .unwrap();
    while expires_at.saturating_sub(now_seconds()) >= SDK_REFRESH_WINDOW_SECONDS {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    oauth
}

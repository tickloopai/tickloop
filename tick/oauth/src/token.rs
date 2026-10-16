//! Using a saved token with its server: renewing it (RFC 6749 section 6) and revoking it
//! (RFC 7009).

use reqwest::{Client, RequestBuilder};
use tick_packages::OAuthToken;

use crate::{
    LoginError,
    server::{Answer, REFRESH_TOKEN_GRANT, Server, TokenResponse, parse},
};

/// Why a token was not renewed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RefreshError {
    /// The server refused the refresh token with this OAuth error code: the login is over.
    Refused(String),
    /// The server could not be reached or answered something else; a later renewal may pass.
    Failed(String),
}

impl std::fmt::Display for RefreshError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(code) => write!(formatter, "the server refused the renewal ({code})"),
            Self::Failed(reason) => formatter.write_str(reason),
        }
    }
}

impl std::error::Error for RefreshError {}

/// Asks `token`'s token endpoint, through `http`, for a new access token with `refresh_token`;
/// `adjust` adds what a server needs beyond the standard request, such as a header. The refresh
/// token and scope stay unless the server replaced them.
pub async fn refresh(
    http: &Client,
    token: &OAuthToken,
    refresh_token: &str,
    adjust: impl FnOnce(RequestBuilder) -> RequestBuilder,
) -> Result<OAuthToken, RefreshError> {
    let failed = |error: LoginError| RefreshError::Failed(error.to_string());
    let server = Server::new(http.clone(), &token.issuer).map_err(failed)?;
    let endpoint = server
        .endpoint(&token.token_endpoint, "token_endpoint")
        .map_err(failed)?;
    let request = adjust(server.http.post(endpoint.clone()).form(&[
        ("grant_type", REFRESH_TOKEN_GRANT),
        ("refresh_token", refresh_token),
        ("client_id", &token.client_id),
    ]));
    let body = match server
        .exchange(request, &endpoint)
        .await
        .map_err(RefreshError::Failed)?
    {
        Answer::Success(body) => body,
        Answer::Refused(code) => return Err(RefreshError::Refused(code)),
        Answer::Unexpected(status) => {
            return Err(RefreshError::Failed(format!(
                "{endpoint} answered HTTP {status}"
            )));
        }
    };
    let response: TokenResponse = parse(&body)
        .filter(|response: &TokenResponse| {
            !response.access_token.is_empty() && !response.token_type.is_empty()
        })
        .ok_or_else(|| failed(server.invalid("token")))?;
    Ok(OAuthToken {
        refresh_token: response
            .refresh_token()
            .or_else(|| token.refresh_token.clone()),
        scope: response.scope.clone().or_else(|| token.scope.clone()),
        expires_at: response.expires_at(),
        access_token: response.access_token,
        token_type: response.token_type,
        ..token.clone()
    })
}

/// Revokes the refresh token, or the access token when there is none, through `http` as RFC 7009
/// says; a token whose server offers no revocation needs none.
pub async fn revoke(http: &Client, token: &OAuthToken) -> Result<(), String> {
    let Some(endpoint) = &token.revocation_endpoint else {
        return Ok(());
    };
    let server = Server::new(http.clone(), &token.issuer).map_err(|error| error.to_string())?;
    let endpoint = server
        .endpoint(endpoint, "revocation_endpoint")
        .map_err(|error| error.to_string())?;
    let (secret, hint) = match token.refresh_token() {
        Some(refresh_token) => (refresh_token, REFRESH_TOKEN_GRANT),
        None => (token.access_token.as_str(), "access_token"),
    };
    let request = server.http.post(endpoint.clone()).form(&[
        ("token", secret),
        ("token_type_hint", hint),
        ("client_id", &token.client_id),
    ]);
    match server.exchange(request, &endpoint).await? {
        Answer::Success(_) => Ok(()),
        Answer::Refused(code) => Err(format!("{endpoint} refused the revocation: {code}")),
        Answer::Unexpected(status) => Err(format!("{endpoint} answered HTTP {status}")),
    }
}

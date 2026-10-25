//! Logging a package in to and out of its data source as its manifest declares, through the
//! standard OAuth client of `tick/oauth` reached under the Agent's HTTP policy: no redirects,
//! bounded connection and request times, and the system's proxies for remote servers.

use crate::{HttpClient, HttpRequest};
use reqwest::Url;
use tick_oauth::{DeviceLogin, LoginError};
use tick_packages::{OAuthIssuer, PackageAuth, PackageCredentials};

/// Starts a device login of the package whose credentials are `credentials` at `issuer`: the
/// returned login holds the link and code to show, and waits for the user's approval.
pub async fn start_login(
    http: &HttpClient,
    credentials: &PackageCredentials,
    issuer: &OAuthIssuer,
    scopes: &[String],
) -> Result<DeviceLogin, LoginError> {
    let http = http_client(http, &issuer.issuer).map_err(LoginError::Failed)?;
    DeviceLogin::start(http, credentials, &issuer.issuer, scopes).await
}

/// What logging out did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoggedOut {
    /// Whether there were saved credentials to delete.
    pub had_credentials: bool,
    /// Why the authorization server did not revoke the token; it is deleted all the same.
    pub revocation_failure: Option<String>,
}

/// Deletes the package's saved credentials. An OAuth token is first revoked where its server
/// offers revocation, under the lock, so a program cannot replace it meanwhile.
pub async fn log_out(
    http: &HttpClient,
    credentials: &PackageCredentials,
    auth: &PackageAuth,
) -> Result<LoggedOut, String> {
    match auth {
        PackageAuth::None => Err("the package needs no login".to_owned()),
        PackageAuth::ApiKey { .. } => {
            let had_credentials = credentials
                .remove_api_key()
                .map_err(|error| error.to_string())?;
            Ok(LoggedOut {
                had_credentials,
                revocation_failure: None,
            })
        }
        PackageAuth::OAuth2 { .. } => {
            if !credentials.oauth_token_path().exists() {
                return Ok(LoggedOut {
                    had_credentials: false,
                    revocation_failure: None,
                });
            }
            let files = {
                let credentials = credentials.clone();
                tokio::task::spawn_blocking(move || credentials.lock_oauth())
                    .await
                    .map_err(|error| error.to_string())?
                    .map_err(|error| error.to_string())?
            };
            // A token that cannot be read is deleted without revoking it.
            let revocation_failure = match files.token() {
                Ok(Some(token)) => match http_client(http, &token.issuer) {
                    Ok(http) => tick_oauth::revoke(&http, &token).await.err(),
                    Err(reason) => Some(reason),
                },
                Ok(None) | Err(_) => None,
            };
            let had_credentials = files.remove_token().map_err(|error| error.to_string())?;
            Ok(LoggedOut {
                had_credentials,
                revocation_failure,
            })
        }
    }
}

/// The client for `issuer`'s server: the Agent's HTTP policy without redirects, whose proxy
/// choice follows the issuer.
fn http_client(http: &HttpClient, issuer: &str) -> Result<reqwest::Client, String> {
    let url = Url::parse(issuer).map_err(|_| format!("{issuer:?} is not a valid issuer URL"))?;
    http(&url, HttpRequest::OAuth)
        .map_err(|error| format!("cannot set up the HTTP client: {error}"))
}

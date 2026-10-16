//! The Longbridge login: the standard token file Tickloop keeps for the package, which every
//! `tick-longbridge` process reads and renews under its lock, and the device login of
//! `tick-longbridge auth`, which saves to the same file.

pub mod device;
mod oauth;

pub use device::{DeviceLogin, LoginNotice};
#[cfg(test)]
pub(crate) use oauth::lapsing_oauth;
pub use oauth::{Authorized, authorized_oauth, current_token};

use std::time::Duration;

use reqwest::redirect::Policy;
use tick_packages::{LockedOAuth, OAuthToken, PackageCredentials, PackageError};
use tick_provider::error::{Error, Result};

use crate::{PACKAGE, not_authorized};

/// The SDK renews a token this close to its expiry before using it.
const SDK_REFRESH_WINDOW_SECONDS: u64 = 300;
const AUTHORIZATION_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// Where this run finds the login: the token file Tickloop named when it started the program,
/// else the package's own in the config root, so a program run on its own shares Tickloop's
/// login.
pub fn credentials() -> Result<PackageCredentials> {
    PackageCredentials::for_program(PACKAGE).map_err(|error| Error::usage(error.to_string()))
}

/// The saved token; refused as not authorized when there is none or it cannot be read.
pub fn saved_token(credentials: &PackageCredentials) -> Result<OAuthToken> {
    credentials
        .oauth_token()
        .map_err(credentials_error)?
        .ok_or_else(|| not_authorized("not authorized"))
}

/// Checks the saved login without the network: a token the SDK can use as it is, or one its
/// refresh token can renew.
pub fn check_saved(credentials: &PackageCredentials) -> Result<()> {
    let token = saved_token(credentials)?;
    if is_due(&token) && token.refresh_token().is_none() {
        return Err(not_authorized("authorization expired"));
    }
    Ok(())
}

/// Whether the SDK would renew `token` before using it.
fn is_due(token: &OAuthToken) -> bool {
    token.expires_at <= now_seconds().saturating_add(SDK_REFRESH_WINDOW_SECONDS + 1)
}

/// A saved login that cannot be read is no login; a file system failure is reported as it is.
fn credentials_error(error: PackageError) -> Error {
    match error {
        PackageError::Invalid(_) => not_authorized("the saved login cannot be read"),
        error => Error::usage(error.to_string()),
    }
}

/// Takes the lock of the package's OAuth files off the async threads, since it waits while
/// another process changes the token.
async fn lock(credentials: &PackageCredentials) -> Result<LockedOAuth> {
    let credentials = credentials.clone();
    blocking(move || credentials.lock_oauth()).await
}

/// The client the login and the renewals reach the authorization server with: no redirects,
/// and a bound on each request.
fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(AUTHORIZATION_REQUEST_TIMEOUT)
        .redirect(Policy::none())
        .build()
        .map_err(|_| Error::network("cannot create HTTP client"))
}

/// Runs file work off the async threads.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> std::result::Result<T, PackageError> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| Error::usage(error.to_string()))?
        .map_err(credentials_error)
}

pub fn now_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

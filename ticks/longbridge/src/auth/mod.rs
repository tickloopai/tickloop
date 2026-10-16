pub mod device;
mod oauth;
pub mod storage;
pub use device::{AuthEndpoints, prepare_region, run_login};
pub use oauth::authorized_oauth;
#[cfg(test)]
pub(crate) use oauth::lapsing_oauth;

use crate::not_authorized;
use storage::{AuthStorage, now_seconds};
use tick_provider::error::{Error, Result};

pub const NONINTERACTIVE_AUTH_ENV: &str = "TICKLOOP_NONINTERACTIVE_AUTH";

pub fn check_noninteractive(storage: &AuthStorage) -> Result<()> {
    let token = storage
        .token()?
        .ok_or_else(|| not_authorized("not authorized"))?;
    if token.expires_at <= now_seconds().saturating_add(oauth::SDK_REFRESH_WINDOW_SECONDS + 1) {
        return Err(not_authorized("authorization expired"));
    }
    Ok(())
}

/// Checks the stored login without the network: a token the SDK can use as it is, or one its
/// refresh token can renew.
pub fn check_stored(storage: &AuthStorage) -> Result<()> {
    let token = storage
        .token()?
        .ok_or_else(|| not_authorized("not authorized"))?;
    let renewable = token
        .refresh_token
        .as_deref()
        .is_some_and(|refresh| !refresh.is_empty());
    if !renewable
        && token.expires_at <= now_seconds().saturating_add(oauth::SDK_REFRESH_WINDOW_SECONDS + 1)
    {
        return Err(not_authorized("authorization expired"));
    }
    Ok(())
}

pub fn default_storage() -> Result<AuthStorage> {
    let home =
        std::env::home_dir().ok_or_else(|| Error::usage("cannot determine home directory"))?;
    Ok(AuthStorage::in_home(&home))
}

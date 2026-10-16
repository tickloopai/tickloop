//! Saved Longbridge logins in a temporary config root, as Tickloop keeps them.

use std::path::Path;

use tick_packages::{OAuthToken, PackageCredentials};

/// Seconds since the Unix epoch.
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// The package's credentials in `config_root`.
pub fn credentials(config_root: &Path) -> PackageCredentials {
    PackageCredentials::in_config_root(config_root, tick_longbridge::PACKAGE).unwrap()
}

/// A token of the server `issuer`, whose token endpoint is `<issuer>/token`.
pub fn token(
    issuer: &str,
    access_token: &str,
    refresh_token: Option<&str>,
    expires_at: u64,
) -> OAuthToken {
    OAuthToken {
        access_token: access_token.into(),
        token_type: "Bearer".into(),
        refresh_token: refresh_token.map(str::to_owned),
        scope: None,
        issuer: issuer.into(),
        client_id: "client-1".into(),
        token_endpoint: format!("{issuer}/token"),
        revocation_endpoint: None,
        expires_at,
    }
}

/// Saves `token` as the login in `config_root`.
pub fn save(config_root: &Path, token: &OAuthToken) {
    credentials(config_root)
        .lock_oauth()
        .unwrap()
        .save_token(token)
        .unwrap();
}

/// The saved login in `config_root`.
pub fn saved(config_root: &Path) -> Option<OAuthToken> {
    credentials(config_root).oauth_token().unwrap()
}

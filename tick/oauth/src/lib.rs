//! A caller-configured OAuth 2.0 client: RFC 8414 discovery, preregistered public clients or
//! RFC 7591 registration, RFC 8628 device login, RFC 6749 refresh with RFC 8707 resources,
//! RFC 7009 revocation and OIDC userinfo. HTTPS is required except on this machine.
//!
//! The caller supplies its HTTP policy and credential store. Tick packages use their existing
//! locked files; other clients implement [`CredentialStore`]. Errors never include tokens.

mod credentials;
mod device;
mod server;
mod token;

use std::fmt;

pub use credentials::CredentialStore;
pub use device::{ClientRegistration, DeviceLogin, Progress};
pub use tick_packages::OAuthToken;
pub use token::{RefreshError, UserInfo, refresh, refresh_for_resource, revoke, user_info};

/// Why a login ended without saving a token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoginError {
    Cancelled,
    /// The user refused the authorization.
    Denied,
    /// The device code expired before the user approved it.
    Expired,
    /// The server refused a request (RFC 6749 section 5.2): a 400 with the OAuth error `code`, or
    /// a 401, whose `code` is `HTTP 401 Unauthorized` when it names none.
    Refused {
        code: String,
        message: String,
    },
    /// The server could not be reached, or answered something else.
    Failed(String),
}

impl fmt::Display for LoginError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("the login was cancelled"),
            Self::Denied => formatter.write_str("the authorization was denied"),
            Self::Expired => {
                formatter.write_str("the code expired before the authorization was approved")
            }
            Self::Refused { message, .. } | Self::Failed(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for LoginError {}

impl From<tick_packages::PackageError> for LoginError {
    fn from(error: tick_packages::PackageError) -> Self {
        Self::Failed(error.to_string())
    }
}

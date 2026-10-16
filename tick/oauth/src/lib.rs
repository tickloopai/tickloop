//! The OAuth 2.0 client of Tick package logins, the one Tickloop and a package's own program
//! both use: the server's metadata (RFC 8414), a client registered once per server (RFC 7591),
//! a device code the user approves (RFC 8628), token refresh (RFC 6749 section 6) and revocation
//! (RFC 7009). Server addresses must use https, or http when the server is on this machine.
//!
//! The caller supplies the HTTP client, configured as its own network policy requires, and the
//! package's credentials, where the registered client and the token are kept under their lock.
//! Errors name servers, steps, statuses and OAuth error codes, never codes or tokens.

mod device;
mod server;
mod token;

use std::fmt;

pub use device::{DeviceLogin, Progress};
pub use token::{RefreshError, refresh, revoke};

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

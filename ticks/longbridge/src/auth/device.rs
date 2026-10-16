//! The device login of `tick-longbridge auth`, made by Tickloop's own OAuth client: the same
//! discovery, registration and polling as the Agent's login, saving to the same token file, so
//! the Agent and every `tick-longbridge` use the one login.

use std::time::Duration;

use tick_oauth::{LoginError, Progress};
use tick_packages::PackageCredentials;
use tick_provider::error::{Error, Result};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::http_client;
use crate::not_authorized;

/// A device authorization waiting for the user, who opens [`Self::verification_uri`] and
/// approves [`Self::user_code`]; [`Self::finish`] waits for the approval and saves the token.
pub struct DeviceLogin(tick_oauth::DeviceLogin);

/// What the waiting screen tells the user while the login goes on.
pub enum LoginNotice {
    TemporaryFailureRetry,
    SlowerPolling,
}

impl LoginNotice {
    pub fn message(&self) -> &'static str {
        match self {
            Self::TemporaryFailureRetry => {
                "Longbridge authorization is temporarily unavailable; retrying automatically."
            }
            Self::SlowerPolling => {
                "Longbridge requested less frequent checks; waiting for confirmation."
            }
        }
    }
}

impl DeviceLogin {
    /// Discovers `issuer`'s endpoints, registers there unless Tickloop or this program did
    /// before, and asks for a device code.
    pub async fn start(credentials: &PackageCredentials, issuer: &str) -> Result<Self> {
        tick_oauth::DeviceLogin::start(http_client()?, credentials, issuer, &[])
            .await
            .map(Self)
            .map_err(login_error)
    }

    /// The code the user approves on the verification page.
    pub fn user_code(&self) -> &str {
        self.0.user_code()
    }

    /// The verification page with the user code filled in when Longbridge offers one.
    pub fn verification_uri(&self) -> &str {
        self.0.link()
    }

    /// The verification page where the user enters the user code.
    pub fn plain_verification_uri(&self) -> &str {
        self.0.verification_uri()
    }

    pub fn remaining(&self) -> Duration {
        self.0.remaining()
    }

    /// Polls until the user approves, refuses or the code expires, and saves the token on
    /// approval. While the server cannot answer the polling backs off and `notices` hears so
    /// once; it hears too when the server asks for slower polling.
    pub async fn finish(&self, notices: &mpsc::UnboundedSender<LoginNotice>) -> Result<()> {
        self.0
            .finish(&CancellationToken::new(), |progress| {
                let _ = notices.send(match progress {
                    Progress::Unreachable => LoginNotice::TemporaryFailureRetry,
                    Progress::SlowerPolling => LoginNotice::SlowerPolling,
                });
            })
            .await
            .map_err(login_error)
    }
}

/// A login the user or Longbridge refused asks for a new login; one that failed otherwise is a
/// network failure.
fn login_error(error: LoginError) -> Error {
    match error {
        LoginError::Denied => not_authorized("authorization denied"),
        LoginError::Expired => not_authorized("device code expired"),
        LoginError::Refused { code, .. } => not_authorized(format!(
            "Longbridge refused the device authorization ({code})"
        )),
        LoginError::Cancelled => Error::usage("the login was cancelled"),
        LoginError::Failed(reason) => Error::network(reason),
    }
}

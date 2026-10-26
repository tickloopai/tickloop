use serde::{Deserialize, Serialize};
use std::fmt;

/// What `oauth2.json` holds: the fields of an RFC 6749 token response, with the server that
/// issued it and when the access token expires. A program refreshes it at `token_endpoint` as
/// `client_id` and saves it back under the lock.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthToken {
    pub access_token: String,
    pub token_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    pub issuer: String,
    pub client_id: String,
    pub token_endpoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revocation_endpoint: Option<String>,
    /// Seconds since the Unix epoch.
    pub expires_at: u64,
}

impl OAuthToken {
    /// The refresh token, when the server gave a usable one.
    pub fn refresh_token(&self) -> Option<&str> {
        self.refresh_token
            .as_deref()
            .filter(|token| !token.is_empty())
    }
}

/// Shows where the token came from, never the tokens themselves.
impl fmt::Debug for OAuthToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OAuthToken")
            .field("token_type", &self.token_type)
            .field("has_refresh_token", &self.refresh_token().is_some())
            .field("scope", &self.scope)
            .field("issuer", &self.issuer)
            .field("client_id", &self.client_id)
            .field("token_endpoint", &self.token_endpoint)
            .field("revocation_endpoint", &self.revocation_endpoint)
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

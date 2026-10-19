//! An authorization server as the client treats it: its addresses, its metadata, and what its
//! endpoints answer.

use reqwest::{
    Client, RequestBuilder, StatusCode, Url,
    header::{ACCEPT, USER_AGENT},
};
use serde::{Deserialize, de::DeserializeOwned};
use url::Host;

use crate::LoginError;

pub(crate) const DEVICE_CODE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
pub(crate) const REFRESH_TOKEN_GRANT: &str = "refresh_token";
const METADATA_PATH: &str = "/.well-known/oauth-authorization-server";
const CLIENT_AGENT: &str = concat!("tickloop/", env!("CARGO_PKG_VERSION"));
const JSON: &str = "application/json";
/// Far above any metadata, registration or token response.
const RESPONSE_SIZE_LIMIT: usize = 64 * 1024;
/// RFC 6749 only recommends `expires_in`; a token without it counts as expiring after an hour.
const DEFAULT_TOKEN_LIFETIME_SECONDS: u64 = 3600;
/// Longer error codes than this are not taken for codes.
const MAX_ERROR_CODE_LENGTH: usize = 128;

/// An authorization server, reached through the caller's HTTP client. Its addresses must use
/// https, or http when the issuer itself is on this machine.
pub(crate) struct Server {
    pub(crate) http: Client,
    pub(crate) issuer: String,
    is_on_this_machine: bool,
}

/// The endpoints a server's metadata names, each checked like the issuer.
pub(crate) struct Endpoints {
    pub(crate) token: Url,
    pub(crate) device_authorization: Url,
    pub(crate) registration: Option<Url>,
    pub(crate) revocation: Option<Url>,
    pub(crate) userinfo: Option<Url>,
}

#[derive(Deserialize)]
struct Metadata {
    issuer: String,
    token_endpoint: Option<String>,
    device_authorization_endpoint: Option<String>,
    registration_endpoint: Option<String>,
    revocation_endpoint: Option<String>,
    userinfo_endpoint: Option<String>,
    grant_types_supported: Option<Vec<String>>,
}

/// An RFC 6749 token response with an access token and its type.
#[derive(Deserialize)]
pub(crate) struct TokenResponse {
    pub(crate) access_token: String,
    pub(crate) token_type: String,
    refresh_token: Option<String>,
    pub(crate) scope: Option<String>,
    expires_in: Option<u64>,
}

impl TokenResponse {
    /// When the access token expires, in seconds since the Unix epoch.
    pub(crate) fn expires_at(&self) -> u64 {
        now_seconds().saturating_add(self.expires_in.unwrap_or(DEFAULT_TOKEN_LIFETIME_SECONDS))
    }

    /// The refresh token, when the server gave a usable one.
    pub(crate) fn refresh_token(&self) -> Option<String> {
        self.refresh_token.clone().filter(|token| !token.is_empty())
    }
}

/// What an endpoint answered.
pub(crate) enum Answer {
    Success(Vec<u8>),
    /// An OAuth error response (RFC 6749 section 5.2): a 400 with a valid `error` code, or any
    /// 401; its code, or `HTTP 401 Unauthorized` for a bare 401.
    Refused(String),
    /// Any other status, which may well pass, such as a 503 while the server restarts.
    Unexpected(StatusCode),
}

#[derive(Deserialize)]
struct ErrorResponse {
    error: String,
}

impl Server {
    pub(crate) fn new(http: Client, issuer: &str) -> Result<Self, LoginError> {
        let url = Url::parse(issuer)
            .map_err(|_| LoginError::Failed(format!("{issuer:?} is not a valid issuer URL")))?;
        let server = Self {
            http,
            issuer: issuer.to_owned(),
            is_on_this_machine: is_on_this_machine(&url),
        };
        server.endpoint(issuer, "issuer")?;
        if url.query().is_some() || url.fragment().is_some() {
            return Err(LoginError::Failed(
                "the issuer must have no query or fragment".into(),
            ));
        }
        Ok(server)
    }

    /// Reads the metadata (RFC 8414): it must name this issuer, a token and a device
    /// authorization endpoint, and the device code grant.
    pub(crate) async fn discover(&self) -> Result<Endpoints, LoginError> {
        let mut url = self.endpoint(&self.issuer, "issuer")?;
        let issuer_path = url.path().trim_end_matches('/').to_owned();
        url.set_path(&format!("{METADATA_PATH}{issuer_path}"));
        let failed = |reason: String| {
            LoginError::Failed(format!(
                "cannot read the authorization server metadata: {reason}"
            ))
        };
        let body = match self.exchange(self.http.get(url.clone()), &url).await {
            Ok(Answer::Success(body)) => body,
            Ok(Answer::Refused(code)) => return Err(failed(format!("{url} answered {code}"))),
            Ok(Answer::Unexpected(status)) => {
                return Err(failed(format!("{url} answered HTTP {status}")));
            }
            Err(reason) => return Err(failed(reason)),
        };
        let metadata: Metadata =
            parse(&body).ok_or_else(|| self.invalid("authorization server metadata"))?;
        if metadata.issuer != self.issuer {
            return Err(LoginError::Failed(format!(
                "{url} describes another issuer than {}",
                self.issuer
            )));
        }
        let offers_device_grant = metadata
            .grant_types_supported
            .is_some_and(|grants| grants.iter().any(|grant| grant == DEVICE_CODE_GRANT));
        let (Some(device_authorization), Some(token), true) = (
            metadata.device_authorization_endpoint,
            metadata.token_endpoint,
            offers_device_grant,
        ) else {
            return Err(LoginError::Failed(format!(
                "{} offers no device authorization (RFC 8628), which Tickloop logs in with",
                self.issuer
            )));
        };
        let optional =
            |url: Option<String>, field| url.map(|url| self.endpoint(&url, field)).transpose();
        Ok(Endpoints {
            token: self.endpoint(&token, "token_endpoint")?,
            device_authorization: self
                .endpoint(&device_authorization, "device_authorization_endpoint")?,
            registration: optional(metadata.registration_endpoint, "registration_endpoint")?,
            revocation: optional(metadata.revocation_endpoint, "revocation_endpoint")?,
            userinfo: optional(metadata.userinfo_endpoint, "userinfo_endpoint")?,
        })
    }

    /// Sends `request` to `url` and reads the answer, refusing one longer than
    /// [`RESPONSE_SIZE_LIMIT`]; `Err` says why there was no answer.
    pub(crate) async fn exchange(
        &self,
        request: RequestBuilder,
        url: &Url,
    ) -> Result<Answer, String> {
        let failed = |error: reqwest::Error| format!("{url}: {}", network_failure(&error));
        let mut response = request
            .header(USER_AGENT, CLIENT_AGENT)
            .header(ACCEPT, JSON)
            .send()
            .await
            .map_err(failed)?;
        let status = response.status();
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(failed)? {
            body.extend_from_slice(&chunk);
            if body.len() > RESPONSE_SIZE_LIMIT {
                return Err(format!(
                    "{url} answered more than {} KiB",
                    RESPONSE_SIZE_LIMIT / 1024
                ));
            }
        }
        Ok(if status.is_success() {
            Answer::Success(body)
        } else {
            match oauth_error(status, &body) {
                Some(code) => Answer::Refused(code),
                None => Answer::Unexpected(status),
            }
        })
    }

    /// `text` as an address of this server: https, or http when the issuer is on this machine
    /// and so is the address, and without credentials.
    pub(crate) fn endpoint(&self, text: &str, field: &str) -> Result<Url, LoginError> {
        let url = Url::parse(text)
            .map_err(|_| LoginError::Failed(format!("{field} {text:?} is not a valid URL")))?;
        let is_allowed = url.scheme() == "https"
            || self.is_on_this_machine && url.scheme() == "http" && is_on_this_machine(&url);
        if is_allowed && url.username().is_empty() && url.password().is_none() {
            Ok(url)
        } else {
            Err(LoginError::Failed(format!(
                "{field} {url} is refused: only https is accepted, or http on this machine"
            )))
        }
    }

    pub(crate) fn invalid(&self, response: &str) -> LoginError {
        LoginError::Failed(format!(
            "{} sent an invalid {response} response",
            self.issuer
        ))
    }
}

pub(crate) fn parse<T: DeserializeOwned>(body: &[u8]) -> Option<T> {
    serde_json::from_slice(body).ok()
}

/// What refuses a request (RFC 6749 section 5.2): a 400 whose body names a valid error code,
/// printable ASCII but '"' and '\', or any 401, which a server may send bare for a client or
/// token it no longer accepts; a bare 401 is named by its status.
fn oauth_error(status: StatusCode, body: &[u8]) -> Option<String> {
    let code = parse::<ErrorResponse>(body)
        .map(|response| response.error)
        .filter(|error| {
            !error.is_empty()
                && error.len() <= MAX_ERROR_CODE_LENGTH
                && error
                    .bytes()
                    .all(|byte| (0x20..=0x7e).contains(&byte) && byte != b'"' && byte != b'\\')
        });
    match status {
        StatusCode::UNAUTHORIZED => Some(code.unwrap_or_else(|| format!("HTTP {status}"))),
        StatusCode::BAD_REQUEST => code,
        _ => None,
    }
}

fn is_on_this_machine(url: &Url) -> bool {
    match url.host() {
        Some(Host::Domain(domain)) => domain == "localhost",
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    }
}

/// What went wrong, with the innermost cause, which names the actual failure.
fn network_failure(error: &reqwest::Error) -> String {
    let summary = if error.is_timeout() {
        "timed out"
    } else if error.is_connect() {
        "cannot connect"
    } else {
        "the request failed"
    };
    let mut cause: &dyn std::error::Error = error;
    while let Some(source) = cause.source() {
        cause = source;
    }
    format!("{summary} ({cause})")
}

pub(crate) fn now_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

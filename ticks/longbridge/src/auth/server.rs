//! An OAuth 2.0 authorization server as Tickloop's own login client treats it: its addresses
//! use https, or http when the server is on this machine; its metadata (RFC 8414) names its
//! endpoints; and it answers with RFC 6749 token and error responses. Errors name servers and
//! steps, never codes or tokens.

use std::time::Duration;

use reqwest::{Client, RequestBuilder, StatusCode, Url, header::ACCEPT, redirect::Policy};
use serde::{Deserialize, de::DeserializeOwned};
use tick_provider::error::{Error, Result};

use super::now_seconds;

pub(super) const DEVICE_CODE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
pub(super) const REFRESH_TOKEN_GRANT: &str = "refresh_token";
const METADATA_PATH: &str = "/.well-known/oauth-authorization-server";
const USER_AGENT: &str = concat!("tickloop-ticks/", env!("CARGO_PKG_VERSION"));
const JSON: &str = "application/json";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// Far above any metadata, registration or token response.
const RESPONSE_SIZE_LIMIT: usize = 64 * 1024;
/// RFC 6749 only recommends `expires_in`; a token without it counts as expiring after an hour.
const DEFAULT_TOKEN_LIFETIME_SECONDS: u64 = 3600;
/// Longer error codes than this are not taken for codes.
const MAX_ERROR_CODE_LENGTH: usize = 128;

pub(super) struct Server {
    issuer: String,
    http: Client,
    is_on_this_machine: bool,
}

/// The endpoints a server's metadata names, each checked like the issuer.
pub(super) struct Endpoints {
    pub token: Url,
    pub device_authorization: Url,
    pub registration: Option<Url>,
    pub revocation: Option<Url>,
}

#[derive(Deserialize)]
struct Metadata {
    issuer: String,
    token_endpoint: Option<String>,
    device_authorization_endpoint: Option<String>,
    registration_endpoint: Option<String>,
    revocation_endpoint: Option<String>,
    grant_types_supported: Option<Vec<String>>,
}

/// What a token request answers.
pub(super) enum Reply {
    Token(TokenResponse),
    /// An OAuth error response with a valid error code.
    Refused(String),
    /// Anything else, with its status.
    Unexpected(StatusCode),
}

/// An RFC 6749 token response with an access token and its type.
#[derive(Deserialize)]
pub(super) struct TokenResponse {
    pub access_token: String,
    pub token_type: String,
    pub refresh_token: Option<String>,
    pub scope: Option<String>,
    expires_in: Option<u64>,
}

impl TokenResponse {
    /// When the access token expires, in seconds since the Unix epoch.
    pub fn expires_at(&self) -> u64 {
        now_seconds().saturating_add(self.expires_in.unwrap_or(DEFAULT_TOKEN_LIFETIME_SECONDS))
    }

    /// The refresh token, when the server gave a usable one.
    pub fn refresh_token(&self) -> Option<String> {
        self.refresh_token.clone().filter(|token| !token.is_empty())
    }
}

#[derive(Deserialize)]
struct ErrorResponse {
    error: String,
}

impl Server {
    pub fn new(issuer: &str) -> Result<Self> {
        let url = Url::parse(issuer)
            .map_err(|_| Error::usage(format!("{issuer:?} is not an authorization server")))?;
        let http = Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .redirect(Policy::none())
            .user_agent(USER_AGENT)
            .build()
            .map_err(|_| Error::network("cannot create HTTP client"))?;
        let server = Self {
            issuer: issuer.to_owned(),
            http,
            is_on_this_machine: is_loopback(&url),
        };
        server.endpoint(issuer, "issuer")?;
        Ok(server)
    }

    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    pub fn http(&self) -> &Client {
        &self.http
    }

    /// `text` as an address of this server: https, or http when the server and the address are
    /// both on this machine, and without credentials.
    pub fn endpoint(&self, text: &str, field: &str) -> Result<Url> {
        let url = Url::parse(text)
            .map_err(|_| Error::network(format!("{field} {text:?} is not a valid URL")))?;
        let is_allowed = url.scheme() == "https"
            || self.is_on_this_machine && url.scheme() == "http" && is_loopback(&url);
        if is_allowed && url.username().is_empty() && url.password().is_none() {
            Ok(url)
        } else {
            Err(Error::network(format!(
                "{field} {url} is refused: only https is accepted"
            )))
        }
    }

    /// Reads the metadata (RFC 8414): it must name this issuer, a token and a device
    /// authorization endpoint, and the device code grant.
    pub async fn discover(&self) -> Result<Endpoints> {
        let mut url = self.endpoint(&self.issuer, "issuer")?;
        let issuer_path = url.path().trim_end_matches('/').to_owned();
        url.set_path(&format!("{METADATA_PATH}{issuer_path}"));
        let (status, body) = self
            .exchange(self.http.get(url.clone()))
            .await
            .map_err(|_| {
                Error::network(format!(
                    "cannot read the authorization server metadata at {url}"
                ))
            })?;
        if !status.is_success() {
            return Err(Error::network(format!(
                "cannot read the authorization server metadata: {url} answered HTTP {status}"
            )));
        }
        let metadata: Metadata =
            parse(&body).ok_or_else(|| self.invalid("authorization server metadata"))?;
        if metadata.issuer != self.issuer {
            return Err(Error::network(format!(
                "{url} describes another authorization server than {}",
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
            return Err(Error::network(format!(
                "{} offers no device authorization (RFC 8628)",
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
        })
    }

    /// Sends a token request and reads what it answers.
    pub async fn request_token(&self, request: RequestBuilder) -> reqwest::Result<Reply> {
        let (status, body) = self.exchange(request).await?;
        Ok(if status.is_success() {
            match parse::<TokenResponse>(&body)
                .filter(|token| !token.access_token.is_empty() && !token.token_type.is_empty())
            {
                Some(token) => Reply::Token(token),
                None => Reply::Unexpected(status),
            }
        } else {
            match error_code(&body) {
                Some(code) => Reply::Refused(code),
                None => Reply::Unexpected(status),
            }
        })
    }

    /// Sends `request` and reads its answer, up to [`RESPONSE_SIZE_LIMIT`]; a longer answer is
    /// cut there and so fails to parse.
    pub async fn exchange(
        &self,
        request: RequestBuilder,
    ) -> reqwest::Result<(StatusCode, Vec<u8>)> {
        let mut response = request.header(ACCEPT, JSON).send().await?;
        let status = response.status();
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            body.extend_from_slice(&chunk);
            if body.len() > RESPONSE_SIZE_LIMIT {
                body.truncate(RESPONSE_SIZE_LIMIT);
                break;
            }
        }
        Ok((status, body))
    }

    pub fn invalid(&self, response: &str) -> Error {
        Error::network(format!(
            "{} sent an invalid {response} response",
            self.issuer
        ))
    }
}

pub(super) fn parse<T: DeserializeOwned>(body: &[u8]) -> Option<T> {
    serde_json::from_slice(body).ok()
}

/// The `error` of an OAuth error response, when it is a valid error code (RFC 6749: printable
/// ASCII but '"' and '\').
pub(super) fn error_code(body: &[u8]) -> Option<String> {
    let ErrorResponse { error } = parse(body)?;
    let is_code = !error.is_empty()
        && error.len() <= MAX_ERROR_CODE_LENGTH
        && error
            .bytes()
            .all(|byte| (0x20..=0x7e).contains(&byte) && byte != b'"' && byte != b'\\');
    is_code.then_some(error)
}

fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(domain)) => domain == "localhost",
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    }
}

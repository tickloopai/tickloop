//! Bounded marketplace transfers using the caller's proxy, TLS and redirect policy.

use std::{io, time::Duration};

use reqwest::Url;

use crate::{HttpClient, HttpRequest, MarketplaceSource, PluginError};

pub(crate) const MARKETPLACE_SIZE_LIMIT: u64 = 1024 * 1024;
pub(crate) const DOWNLOAD_SIZE_LIMIT: u64 = 256 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const CLIENT_NAME: &str = concat!("tickloop/", env!("CARGO_PKG_VERSION"));

impl MarketplaceSource {
    /// Remote marketplaces may only download via HTTPS. Local ones can also use loopback HTTP.
    pub fn download_url(&self, text: &str) -> Result<Url, PluginError> {
        let local = match self {
            Self::Directory(_) | Self::File(_) => true,
            Self::Url(url) => Url::parse(url).is_ok_and(|url| is_loopback(&url)),
            Self::Git(source) => source.url.starts_with("file://"),
        };
        secure_url(text, local)
    }
}

pub(crate) fn secure_url(text: &str, allows_local_http: bool) -> Result<Url, PluginError> {
    let url = Url::parse(text)
        .map_err(|_| PluginError::Invalid(format!("{text:?} is not a valid URL")))?;
    if url.scheme() == "https" || allows_local_http && url.scheme() == "http" && is_loopback(&url) {
        Ok(url)
    } else {
        Err(PluginError::Invalid(
            "only HTTPS is accepted, or HTTP on this machine for a local marketplace".into(),
        ))
    }
}

fn is_loopback(url: &Url) -> bool {
    url.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost")
            || host
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    })
}

pub(crate) async fn receive(
    http: &HttpClient,
    url: &Url,
    request: HttpRequest,
    limit: u64,
    mut save: impl FnMut(&[u8]) -> io::Result<()>,
) -> Result<(), PluginError> {
    let client = http(url, request)
        .map_err(|reason| PluginError::Invalid(format!("cannot set up HTTP client: {reason}")))?;
    let mut response = tokio::time::timeout(
        REQUEST_TIMEOUT,
        client
            .get(url.clone())
            .header(reqwest::header::USER_AGENT, CLIENT_NAME)
            .send(),
    )
    .await
    .map_err(|_| PluginError::Invalid("HTTP request timed out".into()))?
    .map_err(|error| PluginError::Invalid(error.to_string()))?;
    secure_url(response.url().as_str(), is_loopback(url))?;
    if !response.status().is_success() {
        return Err(PluginError::Invalid(format!(
            "server answered HTTP {}",
            response.status()
        )));
    }
    if response.content_length().is_some_and(|size| size > limit) {
        return Err(PluginError::Invalid(format!(
            "download exceeds {} MiB",
            limit / 1024 / 1024
        )));
    }
    let mut received = 0;
    loop {
        let chunk = tokio::time::timeout(REQUEST_TIMEOUT, response.chunk())
            .await
            .map_err(|_| PluginError::Invalid("HTTP download stalled".into()))?
            .map_err(|error| PluginError::Invalid(error.to_string()))?;
        let Some(chunk) = chunk else { break };
        received += chunk.len() as u64;
        if received > limit {
            return Err(PluginError::Invalid(format!(
                "download exceeds {} MiB",
                limit / 1024 / 1024
            )));
        }
        save(&chunk).map_err(PluginError::io("cannot save download"))?;
    }
    Ok(())
}

//! The HTTP client every OAuth request of a Connector goes through: the Harness HTTP policy
//! (TLS, proxies, no redirects), https only except on this machine, and bounded responses.

use futures_util::StreamExt;
use reqwest::{Client, Url, redirect::Policy};
use rmcp::transport::{
    OAuthHttpClient, OAuthHttpClientError, OAuthHttpClientFuture, OAuthHttpRequest,
};
use std::time::Duration;
use tickloop_harness_base::network::{HttpPolicy, build_http_client, is_loopback};

const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_RESPONSE_BYTES: usize = 256 * 1024;

#[derive(Debug, thiserror::Error)]
enum RefusedRequest {
    #[error("only https addresses, or http on this machine, are accepted")]
    Insecure,
    #[error("the response is larger than allowed")]
    TooLarge,
}

/// `url` may be requested: https, or http to this machine, without user information.
pub(crate) fn is_allowed_address(url: &Url) -> bool {
    let secure = url.scheme() == "https" || url.scheme() == "http" && is_loopback(url);
    secure && url.username().is_empty() && url.password().is_none() && url.host_str().is_some()
}

#[derive(Clone)]
pub(crate) struct PolicyClient {
    remote: Client,
    loopback: Client,
}

impl PolicyClient {
    pub(crate) fn new() -> Self {
        let build = |target: &str| {
            let mut policy = HttpPolicy::new(Policy::none());
            policy.timeout = Some(HTTP_TIMEOUT);
            policy.connect_timeout = Some(CONNECT_TIMEOUT);
            build_http_client(&Url::parse(target).expect("fixed URL"), policy)
                .expect("valid HTTP client")
                .0
        };
        Self {
            remote: build("https://mcp.invalid/"),
            loopback: build("http://127.0.0.1/"),
        }
    }

    /// The client for requests to `url`, which bypasses proxies only on this machine.
    pub(crate) fn for_address(&self, url: &Url) -> &Client {
        if is_loopback(url) {
            &self.loopback
        } else {
            &self.remote
        }
    }

    pub(crate) async fn get_bounded(
        &self,
        url: &Url,
    ) -> Result<(u16, Vec<u8>), OAuthHttpClientError> {
        if !is_allowed_address(url) {
            return Err(Box::new(RefusedRequest::Insecure));
        }
        let response = self.for_address(url).get(url.clone()).send().await?;
        let status = response.status().as_u16();
        Ok((status, read_bounded(response).await?))
    }
}

async fn read_bounded(response: reqwest::Response) -> Result<Vec<u8>, OAuthHttpClientError> {
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(Box::new(RefusedRequest::TooLarge));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

impl OAuthHttpClient for PolicyClient {
    fn execute(&self, request: OAuthHttpRequest) -> OAuthHttpClientFuture<'_> {
        let request = request.request;
        Box::pin(async move {
            let url = Url::parse(&request.uri().to_string())?;
            if !is_allowed_address(&url) {
                return Err(Box::new(RefusedRequest::Insecure) as OAuthHttpClientError);
            }
            let request = reqwest::Request::try_from(request)?;
            let response = self.for_address(&url).execute(request).await?;
            let mut builder = http::Response::builder()
                .status(response.status())
                .version(response.version());
            for (name, value) in response.headers() {
                builder = builder.header(name, value);
            }
            let body = read_bounded(response).await?;
            Ok(builder.body(body)?)
        })
    }
}

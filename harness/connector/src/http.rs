//! The HTTP client every OAuth request of a Connector goes through: the Harness HTTP policy
//! (TLS, proxies, no redirects), bounded responses, and https only. This machine is reachable
//! only when the configuration itself points there, so a remote server's metadata cannot send
//! requests to local services.

use futures_util::StreamExt;
use reqwest::{Client, Url, redirect::Policy};
use rmcp::transport::{
    OAuthHttpClient, OAuthHttpClientError, OAuthHttpClientFuture, OAuthHttpRequest,
    auth::WWWAuthenticateParams,
};
use std::time::Duration;
use tickloop_harness_base::network::{HttpPolicy, build_http_client, is_loopback};

const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// Through a proxy this covers the tunnel and the TLS handshake as well; a process's first
/// connection through a local proxy has been seen to take about ten seconds.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_RESPONSE_BYTES: usize = 256 * 1024;
/// Sample targets that choose each client's proxy route: remote addresses may use proxies,
/// this machine never does.
const REMOTE_ROUTE: &str = "https://mcp.invalid/";
const THIS_MACHINE_ROUTE: &str = "http://127.0.0.1/";

#[derive(Debug, thiserror::Error)]
enum RefusedRequest {
    #[error("the address is not allowed for this server's login")]
    Address,
    #[error("the response is larger than allowed")]
    TooLarge,
    #[error("protected resource metadata must have the MCP server's origin")]
    CrossOriginMetadata,
}

/// `url` may be configured: https, or http to this machine, without user information.
pub(crate) fn is_allowed_address(url: &Url) -> bool {
    let secure = url.scheme() == "https" || url.scheme() == "http" && is_loopback(url);
    secure && url.username().is_empty() && url.password().is_none() && url.host_str().is_some()
}

#[derive(Clone)]
pub(crate) struct PolicyClient {
    server_url: Url,
    remote: Client,
    loopback: Client,
    reaches_this_machine: bool,
}

impl PolicyClient {
    /// `reaches_this_machine` when a configured address is on this machine; otherwise every
    /// request must go to a remote https address.
    pub(crate) fn new(server_url: Url, reaches_this_machine: bool) -> Self {
        let build = |target: &str| {
            let mut policy = HttpPolicy::new(Policy::none());
            policy.timeout = Some(HTTP_TIMEOUT);
            policy.connect_timeout = Some(CONNECT_TIMEOUT);
            build_http_client(&Url::parse(target).expect("fixed URL"), policy)
                .expect("valid HTTP client")
                .0
        };
        Self {
            server_url,
            remote: build(REMOTE_ROUTE),
            loopback: build(THIS_MACHINE_ROUTE),
            reaches_this_machine,
        }
    }

    pub(crate) fn validate_challenge(&self, challenge: &str) -> Result<(), OAuthHttpClientError> {
        // rmcp 3.8 permits cross-origin resource metadata; retain the host's stricter policy.
        let metadata =
            WWWAuthenticateParams::parse(challenge, &self.server_url).resource_metadata_url;
        if metadata.is_some_and(|url| url.origin() != self.server_url.origin()) {
            return Err(Box::new(RefusedRequest::CrossOriginMetadata));
        }
        Ok(())
    }

    fn permits(&self, url: &Url) -> bool {
        if !is_allowed_address(url) {
            return false;
        }
        if is_loopback(url) {
            self.reaches_this_machine
        } else {
            url.scheme() == "https"
        }
    }

    /// The client for requests to `endpoint`, when the policy permits them; it bypasses proxies
    /// only on this machine.
    pub(crate) fn client_for(&self, endpoint: &str) -> Option<&Client> {
        let url = Url::parse(endpoint).ok()?;
        self.permitted_client(&url).ok()
    }

    fn permitted_client(&self, url: &Url) -> Result<&Client, OAuthHttpClientError> {
        if !self.permits(url) {
            return Err(Box::new(RefusedRequest::Address));
        }
        Ok(if is_loopback(url) {
            &self.loopback
        } else {
            &self.remote
        })
    }

    pub(crate) async fn get_bounded(
        &self,
        url: &Url,
    ) -> Result<(u16, Vec<u8>), OAuthHttpClientError> {
        let response = self.permitted_client(url)?.get(url.clone()).send().await?;
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
            let client = self.permitted_client(&url)?;
            let request = reqwest::Request::try_from(request)?;
            let response = client.execute(request).await?;
            if response.status() == reqwest::StatusCode::UNAUTHORIZED {
                for challenge in response
                    .headers()
                    .get_all(reqwest::header::WWW_AUTHENTICATE)
                {
                    if let Ok(challenge) = challenge.to_str() {
                        self.validate_challenge(challenge)?;
                    }
                }
            }
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

#[cfg(test)]
mod tests {
    use tick_oauth_test_server::{AuthorizationServer, METADATA_PATH};

    use super::*;

    /// Every request, including the OAuth requests of discovery, registration and the code
    /// exchange, goes through the same check as these.
    #[tokio::test]
    async fn a_remote_servers_login_never_requests_this_machine() {
        let local = AuthorizationServer::start();
        let metadata = Url::parse(&format!("{}{METADATA_PATH}", local.issuer())).unwrap();
        let remote_login = PolicyClient::new(Url::parse(REMOTE_ROUTE).unwrap(), false);

        assert!(remote_login.get_bounded(&metadata).await.is_err());
        assert!(remote_login.client_for(metadata.as_str()).is_none());
        assert!(remote_login.client_for("https://localhost/token").is_none());
        assert!(
            remote_login
                .client_for("http://auth.example.test/token")
                .is_none()
        );
        assert!(
            remote_login
                .client_for("https://auth.example.test/token")
                .is_some()
        );
        assert!(local.requests().is_empty());

        let local_login = PolicyClient::new(Url::parse(local.issuer()).unwrap(), true);
        let (status, _) = local_login.get_bounded(&metadata).await.unwrap();
        assert_eq!(status, 200);
    }
}

//! Code's proxy, redirect and timeout policy for Tick package operations.
use reqwest::{Client, Url, redirect::Policy};
use std::time::Duration;
use tickloop_harness_base::network::{HttpPolicy, build_http_client};
use tickloop_tick_installer::HttpRequest;

const MAX_REDIRECTS: usize = 10;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub fn client(url: &Url, request: HttpRequest) -> Result<Client, String> {
    let redirect = match request {
        HttpRequest::OAuth => Policy::none(),
        _ => Policy::custom(|attempt| {
            if attempt.previous().len() > MAX_REDIRECTS {
                attempt.error("too many redirects")
            } else if attempt.url().scheme() != "https" {
                attempt.error("it redirects to an address that does not use https")
            } else {
                attempt.follow()
            }
        }),
    };
    let mut policy = HttpPolicy::new(redirect);
    policy.connect_timeout = Some(CONNECT_TIMEOUT);
    match request {
        HttpRequest::Download => policy.read_timeout = Some(REQUEST_TIMEOUT),
        _ => policy.timeout = Some(REQUEST_TIMEOUT),
    }
    build_http_client(url, policy)
        .map(|(client, _)| client)
        .map_err(|error| error.to_string())
}

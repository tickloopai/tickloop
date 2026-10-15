//! Reading the Tick marketplace and downloading the package archives it lists, through the
//! Agent's HTTP client policy. Remote addresses must use https. http is accepted only for this
//! machine, and for downloads only when the marketplace is on this machine too, so a remote one
//! cannot point downloads at plain-http services on this machine.

use std::{
    env,
    error::Error,
    fmt,
    fs::File,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use reqwest::{Url, header::USER_AGENT, redirect::Policy};
use tick_packages::{
    DEFAULT_MARKETPLACE_URL, DownloadFile, MARKETPLACE_ENVIRONMENT_VARIABLE, Marketplace,
};
use tickloop_agent_base::network::{HttpPolicy, build_http_client, is_loopback};

const MEBIBYTE: u64 = 1024 * 1024;
const MARKETPLACE_SIZE_LIMIT: u64 = MEBIBYTE;
/// Far above a Tick package's size, and below what tick_packages agrees to unpack.
const DOWNLOAD_SIZE_LIMIT: u64 = 256 * MEBIBYTE;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const MARKETPLACE_TIMEOUT: Duration = Duration::from_secs(30);
/// A download has no overall deadline on a slow link, but fails once no data arrives this long.
const DOWNLOAD_STALL_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_REDIRECTS: usize = 10;
const CLIENT_NAME: &str = concat!("tickloop/", env!("CARGO_PKG_VERSION"));

/// Where the Tick marketplace is read from.
pub enum MarketplaceSource {
    Url(Url),
    File(PathBuf),
}

impl fmt::Display for MarketplaceSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Url(url) => write!(formatter, "{url}"),
            Self::File(path) => write!(formatter, "{}", path.display()),
        }
    }
}

impl MarketplaceSource {
    /// The official marketplace, or the https URL or local file `TICKLOOP_TICK_MARKETPLACE` names.
    pub fn configured() -> Result<Self, String> {
        let Some(configured) =
            env::var_os(MARKETPLACE_ENVIRONMENT_VARIABLE).filter(|value| !value.is_empty())
        else {
            return secure_url(DEFAULT_MARKETPLACE_URL, true).map(Self::Url);
        };
        match configured.to_str() {
            Some(text) if text.contains("://") => secure_url(text, true)
                .map(Self::Url)
                .map_err(|reason| format!("{MARKETPLACE_ENVIRONMENT_VARIABLE}: {reason}")),
            _ => Ok(Self::File(PathBuf::from(configured))),
        }
    }

    pub async fn read(&self) -> Result<Marketplace, String> {
        let bytes = match self {
            Self::Url(url) => fetch(url).await,
            Self::File(path) => read_file(path),
        }
        .map_err(|reason| format!("cannot read the Tick marketplace at {self}: {reason}"))?;
        Marketplace::parse(&bytes)
            .map_err(|error| format!("the Tick marketplace at {self} is invalid: {error}"))
    }

    /// `text` as the URL of a download this marketplace lists.
    pub fn download_url(&self, text: &str) -> Result<Url, String> {
        let is_on_this_machine = match self {
            Self::Url(url) => is_loopback(url),
            Self::File(_) => true,
        };
        secure_url(text, is_on_this_machine)
    }
}

/// `text` as an https URL, or as an http one on this machine when `allows_local_http`.
fn secure_url(text: &str, allows_local_http: bool) -> Result<Url, String> {
    let url = Url::parse(text).map_err(|_| format!("{text:?} is not a valid URL"))?;
    if url.scheme() == "https" || allows_local_http && url.scheme() == "http" && is_loopback(&url) {
        Ok(url)
    } else if allows_local_http {
        Err(format!(
            "{url} is refused: only https is accepted, or http on this machine"
        ))
    } else {
        Err(format!(
            "{url} is refused: a marketplace that is not on this machine may list only https downloads"
        ))
    }
}

/// Downloads the archive at `url` into `file`.
pub async fn download(url: &Url, file: &mut DownloadFile) -> Result<(), String> {
    let mut policy = http_policy();
    policy.read_timeout = Some(DOWNLOAD_STALL_TIMEOUT);
    receive(url, policy, DOWNLOAD_SIZE_LIMIT, |chunk| {
        file.write_all(chunk)
    })
    .await
}

/// The policy for the marketplace and downloads alike: redirects are followed only to https.
fn http_policy() -> HttpPolicy {
    let mut policy = HttpPolicy::new(Policy::custom(|attempt| {
        if attempt.previous().len() > MAX_REDIRECTS {
            attempt.error("too many redirects")
        } else if attempt.url().scheme() != "https" {
            attempt.error("it redirects to an address that does not use https")
        } else {
            attempt.follow()
        }
    }));
    policy.connect_timeout = Some(CONNECT_TIMEOUT);
    policy
}

/// Requests `url` and hands its body to `save` as it arrives, failing once it exceeds `limit`.
async fn receive(
    url: &Url,
    policy: HttpPolicy,
    limit: u64,
    mut save: impl FnMut(&[u8]) -> io::Result<()>,
) -> Result<(), String> {
    let (client, _) = build_http_client(url, policy)
        .map_err(|error| format!("cannot set up the HTTP client: {error}"))?;
    let mut response = client
        .get(url.clone())
        .header(USER_AGENT, CLIENT_NAME)
        .send()
        .await
        .map_err(|error| network_failure(&error))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("the server answered HTTP {status}"));
    }
    if response
        .content_length()
        .is_some_and(|length| length > limit)
    {
        return Err(larger_than(limit));
    }
    let mut received = 0;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| network_failure(&error))?
    {
        received += chunk.len() as u64;
        if received > limit {
            return Err(larger_than(limit));
        }
        save(&chunk).map_err(|error| format!("cannot save it: {error}"))?;
    }
    Ok(())
}

async fn fetch(url: &Url) -> Result<Vec<u8>, String> {
    let mut policy = http_policy();
    policy.timeout = Some(MARKETPLACE_TIMEOUT);
    let mut bytes = Vec::new();
    receive(url, policy, MARKETPLACE_SIZE_LIMIT, |chunk| {
        bytes.extend_from_slice(chunk);
        Ok(())
    })
    .await?;
    Ok(bytes)
}

fn read_file(path: &Path) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    File::open(path)
        .and_then(|file| {
            file.take(MARKETPLACE_SIZE_LIMIT + 1)
                .read_to_end(&mut bytes)
        })
        .map_err(|error| error.to_string())?;
    if bytes.len() as u64 > MARKETPLACE_SIZE_LIMIT {
        return Err(larger_than(MARKETPLACE_SIZE_LIMIT));
    }
    Ok(bytes)
}

fn larger_than(limit: u64) -> String {
    format!("it is larger than {} MiB", limit / MEBIBYTE)
}

/// What went wrong, with the innermost cause, which names the actual failure.
fn network_failure(error: &reqwest::Error) -> String {
    let summary = if error.is_timeout() {
        "timed out"
    } else if error.is_connect() {
        "cannot connect"
    } else if error.is_redirect() {
        "cannot follow its redirect"
    } else {
        "the transfer failed"
    };
    let mut cause: &dyn Error = error;
    while let Some(source) = cause.source() {
        cause = source;
    }
    format!("{summary} ({cause})")
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCAL_HTTP_DOWNLOAD: &str = "http://127.0.0.1:9/tick-sample-1.0.0.tar.gz";
    const HTTPS_DOWNLOAD: &str = "https://example.com/tick-sample-1.0.0.tar.gz";

    fn source(url: &str) -> MarketplaceSource {
        MarketplaceSource::Url(Url::parse(url).unwrap())
    }

    #[test]
    fn only_a_marketplace_on_this_machine_may_list_http_downloads_from_it() {
        let remote = source(DEFAULT_MARKETPLACE_URL);
        assert_eq!(
            remote.download_url(LOCAL_HTTP_DOWNLOAD).unwrap_err(),
            format!(
                "{LOCAL_HTTP_DOWNLOAD} is refused: \
                 a marketplace that is not on this machine may list only https downloads"
            )
        );
        assert!(
            source("https://localhost/marketplace.json")
                .download_url(LOCAL_HTTP_DOWNLOAD)
                .is_ok()
        );
        for local in [
            source("http://127.0.0.1:8080/marketplace.json"),
            MarketplaceSource::File(PathBuf::from("marketplace.json")),
        ] {
            assert_eq!(
                local.download_url(LOCAL_HTTP_DOWNLOAD).unwrap().as_str(),
                LOCAL_HTTP_DOWNLOAD
            );
            assert_eq!(
                local
                    .download_url("http://example.com/tick-sample-1.0.0.tar.gz")
                    .unwrap_err(),
                "http://example.com/tick-sample-1.0.0.tar.gz is refused: \
                 only https is accepted, or http on this machine"
            );
        }
        for any in [
            remote,
            MarketplaceSource::File(PathBuf::from("marketplace.json")),
        ] {
            assert_eq!(
                any.download_url(HTTPS_DOWNLOAD).unwrap().as_str(),
                HTTPS_DOWNLOAD
            );
            assert!(any.download_url("ftp://example.com/x.tar.gz").is_err());
            assert!(any.download_url("tick-sample.tar.gz").is_err());
        }
    }
}

use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::{Url, redirect::Policy};
use serde::Deserialize;
use tickloop_harness_base::network::{HttpPolicy, build_http_client};

const RELEASE_API: &str = "https://api.github.com/repos/tickloopai/homebrew-tap/releases/latest";
const MAX_METADATA_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_ARCHIVE_BYTES: usize = 256 * 1024 * 1024;

#[derive(Debug, Deserialize)]
pub struct Release {
    pub tag_name: String,
    pub draft: bool,
    pub prerelease: bool,
    pub assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
pub struct Asset {
    pub name: String,
    pub browser_download_url: String,
}

#[derive(Debug)]
pub(crate) struct Candidate {
    pub version: semver::Version,
    pub archive: Url,
    pub checksum: Url,
}

pub(crate) fn target() -> Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-musl"),
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-musl"),
        _ => bail!("Prebuilt Tickloop updates support macOS and Linux on ARM64 and x86_64 only"),
    }
}

pub(crate) fn candidate(release: Release, target: &str) -> Result<Candidate> {
    if release.draft || release.prerelease {
        bail!("The update is not a stable published release");
    }
    let version = semver::Version::parse(
        release
            .tag_name
            .strip_prefix('v')
            .context("Invalid release tag")?,
    )?;
    if !version.pre.is_empty() || !version.build.is_empty() {
        bail!("Expected a stable release version");
    }
    let name = format!("tickloop-{target}.tar.gz");
    let asset_url = |name: &str| -> Result<Url> {
        let mut assets = release.assets.iter().filter(|asset| asset.name == name);
        let asset = assets
            .next()
            .context("The release does not yet have all required update assets")?;
        if assets.next().is_some() {
            bail!("Duplicate release asset {name}");
        }
        let url = Url::parse(&asset.browser_download_url)?;
        let expected = format!(
            "https://github.com/tickloopai/homebrew-tap/releases/download/{}/{name}",
            release.tag_name
        );
        if url.as_str() != expected {
            bail!("Unexpected release asset URL");
        }
        Ok(url)
    };
    Ok(Candidate {
        version,
        archive: asset_url(&name)?,
        checksum: asset_url(&format!("{name}.sha256"))?,
    })
}

pub(crate) async fn latest() -> Result<Candidate> {
    let data = get(&Url::parse(RELEASE_API)?, MAX_METADATA_BYTES).await?;
    candidate(serde_json::from_slice(&data)?, target()?)
}

pub(crate) async fn get(url: &Url, limit: usize) -> Result<Vec<u8>> {
    get_with_progress(url, limit, |_, _| Ok(())).await
}

pub(crate) async fn get_with_progress(
    url: &Url,
    limit: usize,
    progress: impl FnMut(u64, Option<u64>) -> Result<()>,
) -> Result<Vec<u8>> {
    if url.scheme() != "https" {
        bail!("Updates require HTTPS");
    }
    let redirect = Policy::custom(|attempt| {
        if attempt.previous().len() >= 10 {
            attempt.error("too many redirects")
        } else if attempt.url().scheme() != "https" {
            attempt.error("updates cannot redirect away from HTTPS")
        } else {
            attempt.follow()
        }
    });
    let mut policy = HttpPolicy::new(redirect);
    policy.connect_timeout = Some(Duration::from_secs(15));
    policy.read_timeout = Some(Duration::from_secs(30));
    policy.timeout = Some(Duration::from_secs(10 * 60));
    let (client, _) = build_http_client(url, policy)?;
    let response = client
        .get(url.clone())
        .header(reqwest::header::USER_AGENT, "tickloop-updater")
        .send()
        .await?
        .error_for_status()?;
    read_response(response, limit, progress).await
}

async fn read_response(
    mut response: reqwest::Response,
    limit: usize,
    mut progress: impl FnMut(u64, Option<u64>) -> Result<()>,
) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|size| size > limit as u64)
    {
        bail!("Update download is too large");
    }
    let total = response.content_length();
    progress(0, total)?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if bytes.len().saturating_add(chunk.len()) > limit {
            bail!("Update download exceeds its size limit");
        }
        bytes.extend_from_slice(&chunk);
        progress(bytes.len() as u64, total)?;
    }
    Ok(bytes)
}

pub(crate) fn checksum(text: &[u8], archive_name: &str) -> Result<String> {
    let text = std::str::from_utf8(text)?;
    let fields = text.split_whitespace().collect::<Vec<_>>();
    if fields.len() != 2
        || fields[1].trim_start_matches('*') != archive_name
        || fields[0].len() != 64
        || !fields[0].bytes().all(|b| b.is_ascii_hexdigit())
    {
        bail!("Invalid archive checksum file");
    }
    Ok(fields[0].to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn response(header: &str, body: &str) -> reqwest::Response {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let reply = format!("HTTP/1.1 200 OK\r\nConnection: close\r\n{header}\r\n{body}");
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                assert!(request.len() < 4096);
                request.push(stream.read_u8().await.unwrap());
            }
            stream.write_all(reply.as_bytes()).await.unwrap();
        });
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!("http://{address}"))
            .send()
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn streamed_download_reports_size_with_and_without_content_length() {
        for (header, body, total) in [
            ("Content-Length: 6\r\n", "abcdef", Some(6)),
            (
                "Transfer-Encoding: chunked\r\n",
                "3\r\nabc\r\n3\r\ndef\r\n0\r\n\r\n",
                None,
            ),
        ] {
            let mut updates = Vec::new();
            let bytes = read_response(response(header, body).await, 6, |bytes, total| {
                updates.push((bytes, total));
                Ok(())
            })
            .await
            .unwrap();
            assert_eq!(bytes, b"abcdef");
            assert_eq!(updates.first(), Some(&(0, total)));
            assert_eq!(updates.last(), Some(&(6, total)));
            assert!(updates.windows(2).all(|pair| pair[0].0 <= pair[1].0));
        }
    }

    #[tokio::test]
    async fn reporting_preserves_download_limits_and_propagates_observer_failure() {
        let result = read_response(
            response("Content-Length: 6\r\n", "abcdef").await,
            5,
            |_, _| Ok(()),
        )
        .await;
        assert!(result.unwrap_err().to_string().contains("too large"));
        let result = read_response(
            response("Transfer-Encoding: chunked\r\n", "6\r\nabcdef\r\n0\r\n\r\n").await,
            5,
            |_, _| Ok(()),
        )
        .await;
        assert!(result.unwrap_err().to_string().contains("size limit"));
        let result = read_response(
            response("Content-Length: 6\r\n", "abcdef").await,
            6,
            |_, _| bail!("state unavailable"),
        )
        .await;
        assert_eq!(result.unwrap_err().to_string(), "state unavailable");
    }
}

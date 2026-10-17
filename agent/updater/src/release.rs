use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::{Url, redirect::Policy};
use serde::Deserialize;
use tickloop_agent_base::network::{HttpPolicy, build_http_client};

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
        _ => bail!("Prebuilt Tickloop updates currently support macOS ARM and Intel only"),
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
    let mut response = client
        .get(url.clone())
        .header(reqwest::header::USER_AGENT, "tickloop-updater")
        .send()
        .await?
        .error_for_status()?;
    if response
        .content_length()
        .is_some_and(|size| size > limit as u64)
    {
        bail!("Update download is too large");
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if bytes.len().saturating_add(chunk.len()) > limit {
            bail!("Update download exceeds its size limit");
        }
        bytes.extend_from_slice(&chunk);
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

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tickloop_agent_base::private_fs::replace_file_atomically;

use crate::{Build, data_root, private_directory};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Source {
    Native { root: PathBuf },
    Homebrew { executable: PathBuf },
    Cargo { root: PathBuf, repository: String },
    CargoRegistry { root: PathBuf, launcher: PathBuf },
    Checkout { root: PathBuf },
}

impl Source {
    /// Reads installation provenance without executing any program.
    pub(crate) fn detect(executable: &Path) -> Result<Option<Self>> {
        Ok(
            detect_installation(executable)?.map(|detected| match detected {
                DetectedInstallation::Recorded(installation) => installation.source,
                DetectedInstallation::Discovered(source) => source,
            }),
        )
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Native { .. } => "native",
            Self::Homebrew { .. } => "Homebrew",
            Self::Cargo { .. } => "Cargo (Git)",
            Self::CargoRegistry { .. } => "Cargo (official prebuilt)",
            Self::Checkout { .. } => "Cargo (local checkout)",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Installation {
    pub source: Source,
    pub build: Build,
    pub pinned: bool,
}

const RECORD: &str = "installation.json";

enum DetectedInstallation {
    Recorded(Installation),
    Discovered(Source),
}

fn detect_installation(executable: &Path) -> Result<Option<DetectedInstallation>> {
    let bin = executable.parent().context("executable has no parent")?;
    if bin.join(RECORD).exists() {
        return Installation::read(bin).map(|record| Some(DetectedInstallation::Recorded(record)));
    }
    // Homebrew's real Cellar path is immutable even when its public symlink changes.
    if let Some(cellar) = bin.parent().and_then(Path::parent).and_then(Path::parent)
        && cellar.file_name().is_some_and(|name| name == "Cellar")
        && bin
            .parent()
            .and_then(Path::parent)
            .and_then(Path::file_name)
            .is_some_and(|name| name == "tickloop")
        && bin
            .parent()
            .and_then(|prefix| std::fs::read(prefix.join("INSTALL_RECEIPT.json")).ok())
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .is_some_and(|receipt| receipt["source"]["tap"] == "tickloopai/tap")
    {
        let brew = cellar
            .parent()
            .context("Cellar has no prefix")?
            .join("bin/brew");
        if !brew.is_file() {
            bail!("Cannot find Homebrew beside {}", cellar.display());
        }
        return Ok(Some(DetectedInstallation::Discovered(Source::Homebrew {
            executable: brew,
        })));
    }
    if bin.file_name().is_some_and(|name| name == "bin")
        && let Some(root) = bin.parent()
        && let Some((repository, _)) = cargo_source(root)?
    {
        return Ok(Some(DetectedInstallation::Discovered(Source::Cargo {
            root: root.into(),
            repository,
        })));
    }
    if bin.file_name().is_some_and(|name| name == "bin")
        && let Some(root) = bin.parent()
        && let Some(checkout) = cargo_checkout(root)?
    {
        return Ok(Some(DetectedInstallation::Discovered(Source::Checkout {
            root: checkout,
        })));
    }
    Ok(None)
}

impl Installation {
    pub fn detect(executable: &Path) -> Result<Option<Self>> {
        match detect_installation(executable)? {
            Some(DetectedInstallation::Recorded(installation)) => Ok(Some(installation)),
            Some(DetectedInstallation::Discovered(source)) => Ok(Some(Self {
                source,
                build: crate::bundle::read_build(
                    executable.parent().context("executable has no parent")?,
                )?,
                pinned: false,
            })),
            None => Ok(None),
        }
    }

    pub(crate) fn read(directory: &Path) -> Result<Self> {
        let bytes =
            std::fs::read(directory.join(RECORD)).context("Cannot read installation record")?;
        let record: Self = serde_json::from_slice(&bytes).context("Invalid installation record")?;
        semver::Version::parse(&record.build.version)?;
        if record.build.identity.is_empty()
            || record.build.identity.contains('/')
            || record.build.identity.contains("..")
        {
            bail!("Invalid installation build identity");
        }
        Ok(record)
    }

    pub(crate) fn write(&self, directory: &Path) -> Result<()> {
        replace_file_atomically(&directory.join(RECORD), &serde_json::to_vec(self)?, 0o600)?;
        Ok(())
    }

    pub(crate) fn state_directory(&self) -> Result<PathBuf> {
        let key = format!("{:x}", Sha256::digest(serde_json::to_vec(&self.source)?));
        let root = data_root()?;
        private_directory(&root)?;
        let directory = root.join("updates").join(key);
        private_directory(&directory)?;
        Ok(directory)
    }
}

pub(crate) fn cargo_source(root: &Path) -> Result<Option<(String, semver::Version)>> {
    let metadata = root.join(".crates2.json");
    if !metadata.exists() {
        return Ok(None);
    }
    let value: serde_json::Value = serde_json::from_slice(&std::fs::read(metadata)?)?;
    let Some(installs) = value.get("installs").and_then(serde_json::Value::as_object) else {
        return Ok(None);
    };
    let mut sources = Vec::new();
    for name in [
        "tickloop-agent",
        "tickloop-tickhost",
        "tickloop-tick-reader",
    ] {
        let mut found = installs
            .keys()
            .filter(|key| key.starts_with(&format!("{name} ")));
        let Some(key) = found.next() else {
            return Ok(None);
        };
        if found.next().is_some() {
            bail!("Cargo records more than one installation of {name}");
        }
        let mut parts = key.splitn(3, ' ');
        let _ = parts.next();
        let version = semver::Version::parse(parts.next().context("Invalid Cargo version")?)?;
        let source = parts.next().context("Invalid Cargo installation source")?;
        let Some(git) = source
            .strip_prefix("(git+")
            .and_then(|text| text.strip_suffix(')'))
        else {
            return Ok(None);
        };
        let mut url = reqwest::Url::parse(git)?;
        let revision = url
            .fragment()
            .context("Cargo did not record its Git revision")?
            .to_owned();
        url.set_fragment(None);
        url.set_query(None);
        if !matches!(url.scheme(), "https" | "ssh")
            || url.password().is_some()
            || (url.scheme() == "https" && !url.username().is_empty())
        {
            bail!("Cargo updates require an HTTPS or SSH Git source without embedded credentials");
        }
        sources.push((url.to_string(), version, revision));
    }
    if sources.windows(2).any(|pair| pair[0] != pair[1]) {
        bail!(
            "Cargo records different Tickloop versions or source repositories; reinstall the complete suite from one release"
        );
    }
    Ok(sources
        .into_iter()
        .next()
        .map(|(url, version, _)| (url, version)))
}

fn cargo_checkout(root: &Path) -> Result<Option<PathBuf>> {
    let metadata = root.join(".crates2.json");
    if !metadata.exists() {
        return Ok(None);
    }
    let value: serde_json::Value = serde_json::from_slice(&std::fs::read(metadata)?)?;
    let Some(installs) = value["installs"].as_object() else {
        return Ok(None);
    };
    let mut checkout = None;
    for name in [
        "tickloop-agent",
        "tickloop-tickhost",
        "tickloop-tick-reader",
    ] {
        let Some(key) = installs
            .keys()
            .find(|key| key.starts_with(&format!("{name} ")))
        else {
            return Ok(None);
        };
        let Some(source) = key
            .splitn(3, ' ')
            .nth(2)
            .and_then(|source| source.strip_prefix("(path+"))
            .and_then(|source| source.strip_suffix(')'))
        else {
            return Ok(None);
        };
        let path = reqwest::Url::parse(source)?
            .to_file_path()
            .map_err(|_| anyhow::anyhow!("Invalid Cargo checkout path"))?;
        let workspace = path
            .parent()
            .and_then(Path::parent)
            .context("Cargo checkout has no workspace")?
            .to_owned();
        if checkout
            .as_ref()
            .is_some_and(|previous| previous != &workspace)
        {
            bail!("Cargo programs come from different checkouts");
        }
        checkout = Some(workspace);
    }
    Ok(checkout)
}

pub(crate) fn cargo_revision(root: &Path) -> Result<String> {
    let value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join(".crates2.json"))?)?;
    let installs = value["installs"]
        .as_object()
        .context("Missing Cargo install metadata")?;
    let mut revision = None;
    for name in [
        "tickloop-agent",
        "tickloop-tickhost",
        "tickloop-tick-reader",
    ] {
        let key = installs
            .keys()
            .find(|key| key.starts_with(&format!("{name} ")))
            .context("Incomplete Cargo build")?;
        let sha = key
            .rsplit_once('#')
            .context("Cargo did not record a Git revision")?
            .1
            .trim_end_matches(')');
        if sha.len() != 40 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("Invalid Cargo Git revision");
        }
        if revision.as_ref().is_some_and(|previous| previous != sha) {
            bail!("Cargo built mixed source revisions");
        }
        revision = Some(sha.to_owned());
    }
    revision.context("Missing Cargo revision")
}

pub(crate) fn registry_runtime(launcher: &Path) -> Result<(PathBuf, semver::Version)> {
    let launcher = launcher
        .canonicalize()
        .context("Cannot find the Cargo launcher")?;
    let bin = launcher
        .parent()
        .context("Cargo launcher has no directory")?;
    if launcher.file_name().is_none_or(|name| name != "tickloop")
        || bin.file_name().is_none_or(|name| name != "bin")
    {
        bail!("Expected the Cargo-installed tickloop launcher");
    }
    let cargo_root = bin
        .parent()
        .context("Cargo launcher has no installation root")?;
    let metadata: serde_json::Value =
        serde_json::from_slice(&std::fs::read(cargo_root.join(".crates2.json"))?)?;
    let installs = metadata["installs"]
        .as_object()
        .context("Cargo installation metadata is missing")?;
    let registrations = installs
        .keys()
        .filter(|key| key.starts_with("tickloop "))
        .collect::<Vec<_>>();
    if registrations.len() != 1 {
        bail!("Cargo must record exactly one tickloop launcher installation");
    }
    let version = registrations[0]
        .split_whitespace()
        .nth(1)
        .context("Cargo launcher has no version")?;
    Ok((
        cargo_root.join(".tickloop"),
        semver::Version::parse(version)?,
    ))
}

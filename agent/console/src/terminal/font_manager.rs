use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tickloop_agent_base::private_fs::{
    PRIVATE_FILE_MODE, create_private_directory, replace_file_atomically,
};
use uuid::Uuid;

const NERD_FONTS_VERSION: &str = "3.5.1";
const SYMBOLS_ONLY_URL: &str =
    "https://github.com/ryanoasis/nerd-fonts/releases/download/v3.5.1/NerdFontsSymbolsOnly.tar.xz";
const SYMBOLS_ONLY_SHA256: &str =
    "01172f37db8543edb102e5cb5c64101c9f4686630804d49b419aa07b23a69996";
const MAX_SYMBOLS_ARCHIVE_BYTES: usize = 32 * 1024 * 1024;
const HOMEBREW_CASK: &str = "font-symbols-only-nerd-font";
const RECEIPT_FILE: &str = "nerd-font-symbols-only.json";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FontAction {
    Install,
    Uninstall,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum FontOutcome {
    ReadyForPreview { message: String },
    Removed { message: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum FontStatus {
    BuiltIn,
    External,
    OwnedHomebrew,
    OwnedDirect,
    InstallAvailable,
    Unsupported,
}

impl FontStatus {
    pub(super) const fn label(&self) -> &'static str {
        match self {
            Self::BuiltIn => "Built into Ghostty; preview before enabling",
            Self::External => "Available and externally managed; preview before enabling",
            Self::OwnedHomebrew => "Installed by Tickloop through Homebrew",
            Self::OwnedDirect => "Installed and owned by Tickloop",
            Self::InstallAvailable => "Not installed by Tickloop",
            Self::Unsupported => "Automatic installation is available on macOS only",
        }
    }

    pub(super) const fn can_install(&self) -> bool {
        matches!(self, Self::InstallAvailable)
    }

    pub(super) const fn can_uninstall(&self) -> bool {
        matches!(self, Self::OwnedHomebrew | Self::OwnedDirect)
    }
}

#[derive(Debug, thiserror::Error)]
pub(super) enum FontManagerError {
    #[error("automatic Symbols-only Nerd Font installation is supported on macOS only")]
    Unsupported,
    #[error("downloaded Symbols-only archive failed SHA-256 verification")]
    DigestMismatch,
    #[error("cannot {operation}: {message}")]
    Operation {
        operation: &'static str,
        message: String,
    },
    #[error("cannot uninstall a font without a Tickloop ownership receipt")]
    MissingOwnership,
    #[error("cannot read the Nerd Font ownership receipt: {0}")]
    InvalidReceipt(String),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct OwnedFont {
    path: PathBuf,
    sha256: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "method", rename_all = "kebab-case")]
enum InstallReceipt {
    Homebrew {
        version: String,
    },
    Direct {
        version: String,
        asset_sha256: String,
        files: Vec<OwnedFont>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct HomebrewState {
    available: bool,
    cask_installed: bool,
}

#[async_trait(?Send)]
pub(super) trait FontAdapter {
    async fn ghostty_has_built_in_symbols(&self) -> Result<bool, FontManagerError>;
    async fn homebrew_state(&self) -> Result<HomebrewState, FontManagerError>;
    async fn external_symbols_present(&self) -> Result<bool, FontManagerError>;
    async fn install_homebrew(&self) -> Result<String, FontManagerError>;
    async fn uninstall_homebrew(&self) -> Result<(), FontManagerError>;
    async fn download_symbols_archive(&self) -> Result<Vec<u8>, FontManagerError>;
    async fn install_symbols_archive(
        &self,
        archive: &[u8],
    ) -> Result<Vec<OwnedFont>, FontManagerError>;
    async fn remove_owned_fonts(&self, fonts: &[OwnedFont]) -> Result<(), FontManagerError>;
}

pub(super) struct NerdFontManager<A = SystemFontAdapter> {
    root: PathBuf,
    expected_sha256: String,
    adapter: A,
}

impl NerdFontManager<SystemFontAdapter> {
    pub(super) fn production(root: PathBuf) -> Self {
        Self {
            adapter: SystemFontAdapter::new(root.clone()),
            root,
            expected_sha256: SYMBOLS_ONLY_SHA256.into(),
        }
    }
}

impl<A: FontAdapter> NerdFontManager<A> {
    #[cfg(test)]
    fn new(root: PathBuf, adapter: A) -> Self {
        Self {
            root,
            expected_sha256: hex_sha256(b"symbols-only"),
            adapter,
        }
    }

    pub(super) async fn status(&self) -> Result<FontStatus, FontManagerError> {
        if let Some(receipt) = self.read_receipt()? {
            return Ok(match receipt {
                InstallReceipt::Homebrew { .. } => FontStatus::OwnedHomebrew,
                InstallReceipt::Direct { .. } => FontStatus::OwnedDirect,
            });
        }
        if !cfg!(target_os = "macos") {
            return Ok(FontStatus::Unsupported);
        }
        if self.adapter.ghostty_has_built_in_symbols().await? {
            return Ok(FontStatus::BuiltIn);
        }
        let homebrew = self.adapter.homebrew_state().await?;
        if homebrew.cask_installed || self.adapter.external_symbols_present().await? {
            Ok(FontStatus::External)
        } else {
            Ok(FontStatus::InstallAvailable)
        }
    }

    pub(super) async fn apply(&self, action: FontAction) -> Result<FontOutcome, FontManagerError> {
        match action {
            FontAction::Install => self.install().await,
            FontAction::Uninstall => self.uninstall().await,
        }
    }

    async fn install(&self) -> Result<FontOutcome, FontManagerError> {
        if !cfg!(target_os = "macos") && !cfg!(test) {
            return Err(FontManagerError::Unsupported);
        }
        if self.adapter.ghostty_has_built_in_symbols().await? {
            return Ok(FontOutcome::ReadyForPreview {
                message: "Ghostty already provides Symbols Nerd Font. Reload and confirm the preview; no font was installed.".into(),
            });
        }
        let homebrew = self.adapter.homebrew_state().await?;
        if homebrew.cask_installed {
            return Ok(FontOutcome::ReadyForPreview {
                message: "Symbols Nerd Font is already managed outside Tickloop. Reload and confirm the preview.".into(),
            });
        }
        if self.adapter.external_symbols_present().await? {
            return Ok(FontOutcome::ReadyForPreview {
                message: "Symbols Nerd Font is already installed outside Tickloop. Reload and confirm the preview; no font was installed.".into(),
            });
        }
        let receipt = if homebrew.available {
            let version = self.adapter.install_homebrew().await?;
            InstallReceipt::Homebrew { version }
        } else {
            let archive = self.adapter.download_symbols_archive().await?;
            if hex_sha256(&archive) != self.expected_sha256 {
                return Err(FontManagerError::DigestMismatch);
            }
            let files = self.adapter.install_symbols_archive(&archive).await?;
            InstallReceipt::Direct {
                version: NERD_FONTS_VERSION.into(),
                asset_sha256: self.expected_sha256.clone(),
                files,
            }
        };
        if let Err(receipt_error) = self.write_receipt(&receipt) {
            let rollback_result = match &receipt {
                InstallReceipt::Homebrew { .. } => self.adapter.uninstall_homebrew().await,
                InstallReceipt::Direct { files, .. } => {
                    self.adapter.remove_owned_fonts(files).await
                }
            };
            if let Err(rollback_error) = rollback_result {
                return Err(FontManagerError::Operation {
                    operation: "roll back the font installation after the ownership receipt failed",
                    message: format!("{receipt_error}; rollback also failed: {rollback_error}"),
                });
            }
            return Err(receipt_error);
        }
        Ok(FontOutcome::ReadyForPreview {
            message: "Symbols Nerd Font installed. Reload the terminal, inspect the preview, then explicitly confirm Nerd Font.".into(),
        })
    }

    async fn uninstall(&self) -> Result<FontOutcome, FontManagerError> {
        let receipt = self
            .read_receipt()?
            .ok_or(FontManagerError::MissingOwnership)?;
        match &receipt {
            InstallReceipt::Homebrew { .. } => self.adapter.uninstall_homebrew().await?,
            InstallReceipt::Direct { files, .. } => self.adapter.remove_owned_fonts(files).await?,
        }
        std::fs::remove_file(self.receipt_path()).map_err(|error| FontManagerError::Operation {
            operation: "remove the ownership receipt",
            message: error.to_string(),
        })?;
        Ok(FontOutcome::Removed {
            message: "Tickloop-owned Symbols Nerd Font was removed. Unicode remains active.".into(),
        })
    }

    fn receipt_path(&self) -> PathBuf {
        self.root.join("font-installations").join(RECEIPT_FILE)
    }

    fn read_receipt(&self) -> Result<Option<InstallReceipt>, FontManagerError> {
        let bytes = match std::fs::read(self.receipt_path()) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(FontManagerError::InvalidReceipt(error.to_string())),
        };
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| FontManagerError::InvalidReceipt(error.to_string()))
    }

    fn write_receipt(&self, receipt: &InstallReceipt) -> Result<(), FontManagerError> {
        let path = self.receipt_path();
        create_private_directory(path.parent().expect("receipt has a parent")).map_err(
            |error| FontManagerError::Operation {
                operation: "create the receipt directory",
                message: error.to_string(),
            },
        )?;
        let mut bytes = serde_json::to_vec_pretty(receipt).expect("receipt serializes");
        bytes.push(b'\n');
        replace_file_atomically(&path, &bytes, PRIVATE_FILE_MODE).map_err(|error| {
            FontManagerError::Operation {
                operation: "write the ownership receipt",
                message: error.to_string(),
            }
        })
    }

    #[cfg(test)]
    fn adapter(&self) -> &A {
        &self.adapter
    }
}

fn hex_sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub(super) struct SystemFontAdapter {
    root: PathBuf,
    home: Option<PathBuf>,
}

impl SystemFontAdapter {
    fn new(root: PathBuf) -> Self {
        Self {
            root,
            home: std::env::var_os("HOME").map(PathBuf::from),
        }
    }

    fn brew_path(&self) -> Option<PathBuf> {
        ["/opt/homebrew/bin/brew", "/usr/local/bin/brew"]
            .into_iter()
            .map(PathBuf::from)
            .find(|path| path.is_file())
            .or_else(|| {
                std::env::var_os("PATH").and_then(|path| {
                    std::env::split_paths(&path)
                        .map(|directory| directory.join("brew"))
                        .find(|path| path.is_file())
                })
            })
    }

    async fn run_brew(&self, arguments: &[&str]) -> Result<std::process::Output, FontManagerError> {
        let brew = self
            .brew_path()
            .ok_or_else(|| FontManagerError::Operation {
                operation: "run Homebrew",
                message: "brew is unavailable".into(),
            })?;
        tokio::process::Command::new(brew)
            .args(arguments)
            .output()
            .await
            .map_err(|error| FontManagerError::Operation {
                operation: "run Homebrew",
                message: error.to_string(),
            })
    }
}

#[async_trait(?Send)]
impl FontAdapter for SystemFontAdapter {
    async fn ghostty_has_built_in_symbols(&self) -> Result<bool, FontManagerError> {
        if std::env::var("TERM_PROGRAM").ok().as_deref() != Some("ghostty") {
            return Ok(false);
        }
        let output = tokio::process::Command::new("ghostty")
            .arg("+version")
            .output()
            .await;
        let Ok(output) = output else { return Ok(false) };
        let version = String::from_utf8_lossy(&output.stdout);
        let mut parts = version
            .split_whitespace()
            .find(|part| {
                part.chars()
                    .next()
                    .is_some_and(|character| character.is_ascii_digit())
            })
            .unwrap_or_default()
            .split('.')
            .filter_map(|part| part.parse::<u32>().ok());
        Ok(
            matches!((parts.next(), parts.next()), (Some(major), Some(minor)) if major > 1 || major == 1 && minor >= 2),
        )
    }

    async fn homebrew_state(&self) -> Result<HomebrewState, FontManagerError> {
        if self.brew_path().is_none() {
            return Ok(HomebrewState {
                available: false,
                cask_installed: false,
            });
        }
        let installed = self
            .run_brew(&["list", "--cask", HOMEBREW_CASK])
            .await?
            .status
            .success();
        Ok(HomebrewState {
            available: true,
            cask_installed: installed,
        })
    }

    async fn external_symbols_present(&self) -> Result<bool, FontManagerError> {
        let mut directories = vec![PathBuf::from("/Library/Fonts")];
        if let Some(home) = &self.home {
            directories.extend([
                home.join("Library/Fonts"),
                home.join("Library/Fonts/NerdFonts"),
            ]);
        }
        for directory in directories {
            let entries = match std::fs::read_dir(directory) {
                Ok(entries) => entries,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
                let is_font = name.ends_with(".ttf") || name.ends_with(".otf");
                if is_font && name.contains("symbolsnerdfont") {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    async fn install_homebrew(&self) -> Result<String, FontManagerError> {
        let output = self.run_brew(&["install", "--cask", HOMEBREW_CASK]).await?;
        if !output.status.success() {
            return Err(FontManagerError::Operation {
                operation: "install the Homebrew cask",
                message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            });
        }
        let version_output = self
            .run_brew(&["list", "--cask", "--versions", HOMEBREW_CASK])
            .await;
        let version = version_output
            .as_ref()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| {
                String::from_utf8_lossy(&output.stdout)
                    .split_whitespace()
                    .nth(1)
                    .map(str::to_owned)
            });
        if let Some(version) = version {
            return Ok(version);
        }
        let version_error = version_output.map_or_else(
            |error| error.to_string(),
            |output| String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        );
        let rollback = self.run_brew(&["uninstall", "--cask", HOMEBREW_CASK]).await;
        let rollback_error = match rollback {
            Ok(output) if output.status.success() => String::new(),
            Ok(output) => format!(
                "; rollback also failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
            Err(error) => format!("; rollback also failed: {error}"),
        };
        Err(FontManagerError::Operation {
            operation: "record the installed Homebrew cask version",
            message: format!("{version_error}{rollback_error}"),
        })
    }

    async fn uninstall_homebrew(&self) -> Result<(), FontManagerError> {
        let output = self
            .run_brew(&["uninstall", "--cask", HOMEBREW_CASK])
            .await?;
        if output.status.success() {
            Ok(())
        } else {
            Err(FontManagerError::Operation {
                operation: "uninstall the Homebrew cask",
                message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            })
        }
    }

    async fn download_symbols_archive(&self) -> Result<Vec<u8>, FontManagerError> {
        let mut response = reqwest::get(SYMBOLS_ONLY_URL)
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|error| FontManagerError::Operation {
                operation: "download the official Symbols-only release",
                message: error.to_string(),
            })?;
        if response
            .content_length()
            .is_some_and(|length| length > MAX_SYMBOLS_ARCHIVE_BYTES as u64)
        {
            return Err(FontManagerError::Operation {
                operation: "download the official Symbols-only release",
                message: "the archive exceeds the 32 MiB safety limit".into(),
            });
        }
        let mut archive = Vec::new();
        while let Some(chunk) =
            response
                .chunk()
                .await
                .map_err(|error| FontManagerError::Operation {
                    operation: "read the official Symbols-only release",
                    message: error.to_string(),
                })?
        {
            if archive.len().saturating_add(chunk.len()) > MAX_SYMBOLS_ARCHIVE_BYTES {
                return Err(FontManagerError::Operation {
                    operation: "download the official Symbols-only release",
                    message: "the archive exceeds the 32 MiB safety limit".into(),
                });
            }
            archive.extend_from_slice(&chunk);
        }
        Ok(archive)
    }

    async fn install_symbols_archive(
        &self,
        archive: &[u8],
    ) -> Result<Vec<OwnedFont>, FontManagerError> {
        let home = self
            .home
            .as_ref()
            .ok_or_else(|| FontManagerError::Operation {
                operation: "install Symbols-only",
                message: "HOME is unavailable".into(),
            })?;
        let scratch = self.root.join(format!(".font-install-{}", Uuid::new_v4()));
        let extracted = scratch.join("extracted");
        if let Err(error) = create_private_directory(&extracted) {
            let source = operation_error("create install scratch")(error);
            return match std::fs::remove_dir_all(&scratch) {
                Ok(()) => Err(source),
                Err(cleanup_error) if cleanup_error.kind() == std::io::ErrorKind::NotFound => {
                    Err(source)
                }
                Err(cleanup_error) => Err(FontManagerError::Operation {
                    operation: "clean up a failed font install scratch directory",
                    message: format!("{source}; cleanup also failed: {cleanup_error}"),
                }),
            };
        }
        let archive_path = scratch.join("symbols.tar.xz");
        let mut installed = Vec::new();
        let result = async {
            std::fs::write(&archive_path, archive)
                .map_err(operation_error("write install scratch"))?;
            let output = tokio::process::Command::new("/usr/bin/tar")
                .args(["-xJf"])
                .arg(&archive_path)
                .arg("-C")
                .arg(&extracted)
                .output()
                .await
                .map_err(operation_error("extract the Symbols-only archive"))?;
            if !output.status.success() {
                return Err(FontManagerError::Operation {
                    operation: "extract the Symbols-only archive",
                    message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
                });
            }
            let sources = font_files(&extracted)?;
            if sources.is_empty() {
                return Err(FontManagerError::Operation {
                    operation: "install Symbols-only",
                    message: "the verified archive contains no font files".into(),
                });
            }
            let target_directory = home.join("Library/Fonts/NerdFonts");
            std::fs::create_dir_all(&target_directory)
                .map_err(operation_error("create the user font directory"))?;
            for source in sources {
                let target = target_directory.join(source.file_name().expect("font has a name"));
                let mut source_file = std::fs::File::open(&source)
                    .map_err(operation_error("open an extracted Symbols-only font"))?;
                let mut target_file = match std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&target)
                {
                    Ok(file) => file,
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                        return Err(FontManagerError::Operation {
                            operation: "install Symbols-only",
                            message: format!(
                                "refusing to replace existing font {}",
                                target.display()
                            ),
                        });
                    }
                    Err(error) => return Err(operation_error("create a Symbols-only font")(error)),
                };
                if let Err(error) = std::io::copy(&mut source_file, &mut target_file) {
                    drop(target_file);
                    return Err(remove_failed_target(
                        &target,
                        operation_error("copy a Symbols-only font")(error),
                    ));
                }
                drop(target_file);
                let bytes = match std::fs::read(&target) {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        return Err(remove_failed_target(
                            &target,
                            operation_error("verify an installed font")(error),
                        ));
                    }
                };
                installed.push(OwnedFont {
                    path: target,
                    sha256: hex_sha256(&bytes),
                });
            }
            Ok(())
        }
        .await;
        let result = match std::fs::remove_dir_all(&scratch) {
            Ok(()) => result,
            Err(cleanup_error) if cleanup_error.kind() == std::io::ErrorKind::NotFound => result,
            Err(cleanup_error) => Err(FontManagerError::Operation {
                operation: "clean up the font install scratch directory",
                message: match result {
                    Ok(()) => cleanup_error.to_string(),
                    Err(error) => format!("{error}; cleanup also failed: {cleanup_error}"),
                },
            }),
        };
        if let Err(error) = result {
            return match rollback_fonts(&installed) {
                Ok(()) => Err(error),
                Err(rollback_error) => Err(FontManagerError::Operation {
                    operation: "roll back a failed Symbols-only installation",
                    message: format!("{error}; rollback also failed: {rollback_error}"),
                }),
            };
        }
        Ok(installed)
    }

    async fn remove_owned_fonts(&self, fonts: &[OwnedFont]) -> Result<(), FontManagerError> {
        let home = self
            .home
            .as_ref()
            .ok_or_else(|| FontManagerError::Operation {
                operation: "remove Symbols-only",
                message: "HOME is unavailable".into(),
            })?;
        let allowed = home.join("Library/Fonts/NerdFonts");
        let mut removable = Vec::new();
        for font in fonts {
            if font.path.parent() != Some(allowed.as_path()) {
                return Err(FontManagerError::Operation {
                    operation: "remove Symbols-only",
                    message: format!("receipt path is outside {}", allowed.display()),
                });
            }
            let metadata = match std::fs::symlink_metadata(&font.path) {
                Ok(metadata) => metadata,
                Err(error)
                    if error.kind() == std::io::ErrorKind::NotFound
                        && is_staged_removal_path(&font.path) =>
                {
                    continue;
                }
                Err(error) => return Err(operation_error("inspect an owned font")(error)),
            };
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(FontManagerError::Operation {
                    operation: "remove Symbols-only",
                    message: format!("{} is not an owned regular file", font.path.display()),
                });
            }
            let bytes = std::fs::read(&font.path).map_err(operation_error("read an owned font"))?;
            if hex_sha256(&bytes) != font.sha256 {
                return Err(FontManagerError::Operation {
                    operation: "remove Symbols-only",
                    message: format!(
                        "{} was modified; refusing to delete it",
                        font.path.display()
                    ),
                });
            }
            removable.push(font);
        }
        let mut staged: Vec<(PathBuf, OwnedFont)> = Vec::new();
        for font in removable {
            let staged_path = allowed.join(format!(".tickloop-remove-{}", Uuid::new_v4()));
            if let Err(error) = std::fs::rename(&font.path, &staged_path) {
                for (original, staged_font) in staged.iter().rev() {
                    if let Err(rollback_error) = std::fs::rename(&staged_font.path, original) {
                        return Err(FontManagerError::Operation {
                            operation: "roll back an interrupted Symbols-only uninstall",
                            message: format!("{error}; rollback also failed: {rollback_error}"),
                        });
                    }
                }
                return Err(operation_error("stage an owned font for removal")(error));
            }
            staged.push((
                font.path.clone(),
                OwnedFont {
                    path: staged_path,
                    sha256: font.sha256.clone(),
                },
            ));
        }
        if self.receipt_path().is_file()
            && let Err(error) = self.rewrite_direct_receipt(
                staged
                    .iter()
                    .map(|(_, staged_font)| staged_font.clone())
                    .collect(),
            )
        {
            for (original, staged_font) in staged.iter().rev() {
                if let Err(rollback_error) = std::fs::rename(&staged_font.path, original) {
                    return Err(FontManagerError::Operation {
                        operation: "roll back an interrupted Symbols-only uninstall",
                        message: format!("{error}; rollback also failed: {rollback_error}"),
                    });
                }
            }
            return Err(error);
        }
        let mut cleanup_errors = Vec::new();
        for (_, staged_font) in staged {
            if let Err(error) = std::fs::remove_file(&staged_font.path) {
                cleanup_errors.push(format!("{}: {error}", staged_font.path.display()));
            }
        }
        if !cleanup_errors.is_empty() {
            return Err(FontManagerError::Operation {
                operation: "remove staged Symbols-only fonts",
                message: cleanup_errors.join("; "),
            });
        }
        Ok(())
    }
}

impl SystemFontAdapter {
    fn receipt_path(&self) -> PathBuf {
        self.root.join("font-installations").join(RECEIPT_FILE)
    }

    fn rewrite_direct_receipt(&self, files: Vec<OwnedFont>) -> Result<(), FontManagerError> {
        let path = self.receipt_path();
        let bytes = std::fs::read(&path)
            .map_err(|error| FontManagerError::InvalidReceipt(error.to_string()))?;
        let mut receipt: InstallReceipt = serde_json::from_slice(&bytes)
            .map_err(|error| FontManagerError::InvalidReceipt(error.to_string()))?;
        match &mut receipt {
            InstallReceipt::Direct {
                files: owned_files, ..
            } => *owned_files = files,
            InstallReceipt::Homebrew { .. } => {
                return Err(FontManagerError::InvalidReceipt(
                    "Homebrew receipt cannot own direct font files".into(),
                ));
            }
        }
        let mut encoded = serde_json::to_vec_pretty(&receipt).expect("receipt serializes");
        encoded.push(b'\n');
        replace_file_atomically(&path, &encoded, PRIVATE_FILE_MODE).map_err(|error| {
            FontManagerError::Operation {
                operation: "preserve ownership while removing Symbols-only fonts",
                message: error.to_string(),
            }
        })
    }
}

fn is_staged_removal_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with(".tickloop-remove-"))
}

fn operation_error(operation: &'static str) -> impl FnOnce(std::io::Error) -> FontManagerError {
    move |error| FontManagerError::Operation {
        operation,
        message: error.to_string(),
    }
}

fn remove_failed_target(path: &Path, source: FontManagerError) -> FontManagerError {
    match std::fs::remove_file(path) {
        Ok(()) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => source,
        Err(error) => FontManagerError::Operation {
            operation: "clean up a partially installed Symbols-only font",
            message: format!(
                "{source}; cleanup also failed for {}: {error}",
                path.display()
            ),
        },
    }
}

fn font_files(root: &Path) -> Result<Vec<PathBuf>, FontManagerError> {
    let mut pending = vec![root.to_path_buf()];
    let mut fonts = Vec::new();
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(directory)
            .map_err(operation_error("inspect the extracted archive"))?
        {
            let entry = entry.map_err(operation_error("inspect the extracted archive"))?;
            let file_type = entry
                .file_type()
                .map_err(operation_error("inspect the extracted archive"))?;
            if file_type.is_symlink() {
                return Err(FontManagerError::Operation {
                    operation: "inspect the extracted archive",
                    message: "the archive contains a symbolic link".into(),
                });
            }
            if file_type.is_dir() {
                pending.push(entry.path());
            } else if entry
                .path()
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| matches!(extension, "ttf" | "otf"))
            {
                fonts.push(entry.path());
            }
        }
    }
    fonts.sort();
    Ok(fonts)
}

fn rollback_fonts(fonts: &[OwnedFont]) -> Result<(), FontManagerError> {
    let mut errors = Vec::new();
    for font in fonts {
        if let Err(error) = std::fs::remove_file(&font.path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            errors.push(format!("{}: {error}", font.path.display()));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(FontManagerError::Operation {
            operation: "remove partially installed Symbols-only fonts",
            message: errors.join("; "),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    struct FakeFontAdapter {
        archive: Vec<u8>,
        external: bool,
        manual_external: bool,
        remove_fails: Cell<bool>,
        install_calls: Cell<usize>,
        remove_calls: Cell<usize>,
    }

    impl FakeFontAdapter {
        fn with_archive(archive: &[u8]) -> Self {
            Self {
                archive: archive.to_vec(),
                external: false,
                manual_external: false,
                remove_fails: Cell::new(false),
                install_calls: Cell::new(0),
                remove_calls: Cell::new(0),
            }
        }

        fn external_font() -> Self {
            Self {
                external: true,
                ..Self::with_archive(&[])
            }
        }

        fn manually_installed_font() -> Self {
            Self {
                manual_external: true,
                ..Self::with_archive(&[])
            }
        }
    }

    #[async_trait(?Send)]
    impl FontAdapter for FakeFontAdapter {
        async fn ghostty_has_built_in_symbols(&self) -> Result<bool, FontManagerError> {
            Ok(false)
        }

        async fn homebrew_state(&self) -> Result<HomebrewState, FontManagerError> {
            Ok(HomebrewState {
                available: self.external,
                cask_installed: self.external,
            })
        }

        async fn external_symbols_present(&self) -> Result<bool, FontManagerError> {
            Ok(self.manual_external)
        }

        async fn install_homebrew(&self) -> Result<String, FontManagerError> {
            self.install_calls.set(self.install_calls.get() + 1);
            Ok(NERD_FONTS_VERSION.into())
        }

        async fn uninstall_homebrew(&self) -> Result<(), FontManagerError> {
            self.remove_calls.set(self.remove_calls.get() + 1);
            Ok(())
        }

        async fn download_symbols_archive(&self) -> Result<Vec<u8>, FontManagerError> {
            Ok(self.archive.clone())
        }

        async fn install_symbols_archive(
            &self,
            _archive: &[u8],
        ) -> Result<Vec<OwnedFont>, FontManagerError> {
            self.install_calls.set(self.install_calls.get() + 1);
            Ok(vec![OwnedFont {
                path: PathBuf::from("font.ttf"),
                sha256: "font-digest".into(),
            }])
        }

        async fn remove_owned_fonts(&self, _fonts: &[OwnedFont]) -> Result<(), FontManagerError> {
            self.remove_calls.set(self.remove_calls.get() + 1);
            if self.remove_fails.get() {
                Err(FontManagerError::Operation {
                    operation: "remove a fake font",
                    message: "injected failure".into(),
                })
            } else {
                Ok(())
            }
        }
    }

    #[tokio::test]
    async fn verified_install_records_ownership() {
        let directory = tempfile::tempdir().unwrap();
        let adapter = FakeFontAdapter::with_archive(b"symbols-only");
        let manager = NerdFontManager::new(directory.path().to_path_buf(), adapter);

        let outcome = manager.apply(FontAction::Install).await.unwrap();

        assert!(matches!(outcome, FontOutcome::ReadyForPreview { .. }));
        assert!(manager.receipt_path().exists());
    }

    #[tokio::test]
    async fn direct_install_rolls_back_when_the_ownership_receipt_cannot_be_written() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("font-installations"),
            b"not a directory",
        )
        .unwrap();
        let adapter = FakeFontAdapter::with_archive(b"symbols-only");
        let manager = NerdFontManager::new(directory.path().to_path_buf(), adapter);

        let error = manager.apply(FontAction::Install).await.unwrap_err();

        assert!(error.to_string().contains("receipt directory"));
        assert_eq!(manager.adapter().install_calls.get(), 1);
        assert_eq!(manager.adapter().remove_calls.get(), 1);
    }

    #[tokio::test]
    async fn an_archive_with_the_wrong_digest_is_rejected_before_installation() {
        let directory = tempfile::tempdir().unwrap();
        let adapter = FakeFontAdapter::with_archive(b"not-the-release-asset");
        let manager = NerdFontManager::new(directory.path().to_path_buf(), adapter);

        let error = manager.apply(FontAction::Install).await.unwrap_err();

        assert!(error.to_string().contains("SHA-256"));
        assert!(!manager.receipt_path().exists());
        assert_eq!(manager.adapter().install_calls.get(), 0);
    }

    #[tokio::test]
    async fn uninstall_without_a_tickloop_receipt_refuses_external_fonts() {
        let directory = tempfile::tempdir().unwrap();
        let manager = NerdFontManager::new(
            directory.path().to_path_buf(),
            FakeFontAdapter::external_font(),
        );

        let error = manager.apply(FontAction::Uninstall).await.unwrap_err();

        assert!(error.to_string().contains("ownership"));
        assert_eq!(manager.adapter().remove_calls.get(), 0);
    }

    #[tokio::test]
    async fn a_manually_installed_symbols_font_is_external_and_has_no_remove_action() {
        let directory = tempfile::tempdir().unwrap();
        let manager = NerdFontManager::new(
            directory.path().to_path_buf(),
            FakeFontAdapter::manually_installed_font(),
        );

        let status = manager.status().await.unwrap();

        assert_eq!(status, FontStatus::External);
        assert!(!status.can_uninstall());
    }

    #[tokio::test]
    async fn a_failed_uninstall_keeps_the_ownership_receipt() {
        let directory = tempfile::tempdir().unwrap();
        let manager = NerdFontManager::new(
            directory.path().to_path_buf(),
            FakeFontAdapter::with_archive(b"symbols-only"),
        );
        manager.apply(FontAction::Install).await.unwrap();
        manager.adapter().remove_fails.set(true);

        let error = manager.apply(FontAction::Uninstall).await.unwrap_err();

        assert!(error.to_string().contains("injected failure"));
        assert!(manager.receipt_path().exists());
    }

    #[tokio::test]
    async fn direct_uninstall_retries_from_staged_receipt_paths() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().join("home");
        let font_directory = home.join("Library/Fonts/NerdFonts");
        std::fs::create_dir_all(&font_directory).unwrap();
        let staged_font = font_directory.join(".tickloop-remove-retry");
        std::fs::write(&staged_font, b"owned font").unwrap();
        let missing_font = font_directory.join(".tickloop-remove-already-gone");
        let root = directory.path().join("config");
        let manager = NerdFontManager {
            root: root.clone(),
            expected_sha256: SYMBOLS_ONLY_SHA256.into(),
            adapter: SystemFontAdapter {
                root,
                home: Some(home),
            },
        };
        manager
            .write_receipt(&InstallReceipt::Direct {
                version: NERD_FONTS_VERSION.into(),
                asset_sha256: SYMBOLS_ONLY_SHA256.into(),
                files: vec![
                    OwnedFont {
                        path: missing_font,
                        sha256: "already removed".into(),
                    },
                    OwnedFont {
                        path: staged_font.clone(),
                        sha256: hex_sha256(b"owned font"),
                    },
                ],
            })
            .unwrap();

        let outcome = manager.apply(FontAction::Uninstall).await.unwrap();

        assert!(matches!(outcome, FontOutcome::Removed { .. }));
        assert!(!staged_font.exists());
        assert!(!manager.receipt_path().exists());
    }
}

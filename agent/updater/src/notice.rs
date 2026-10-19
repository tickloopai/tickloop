use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

use anyhow::Result;

use crate::{Installation, Source, UpdateActivity, UpdateProgress, UpdateState, source};

const MAX_STATE_BYTES: u64 = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateNotice {
    Checking,
    Installing { progress: UpdateProgress },
    Ready { version: String },
    Activating,
    Deferred { reason: String },
    Failed,
}

pub struct UpdateMonitor {
    installation: Installation,
    directory: PathBuf,
}

impl UpdateMonitor {
    pub fn detect() -> Result<Option<Self>> {
        let executable = std::env::current_exe()?.canonicalize()?;
        let Some(installation) = Installation::detect(&executable)? else {
            return Ok(None);
        };
        if matches!(installation.source, Source::Checkout { .. }) {
            return Ok(None);
        }
        let directory = installation.state_directory()?;
        Ok(Some(Self {
            installation,
            directory,
        }))
    }

    /// Observes local files only; it never starts a check, waits for a lock, or runs a program.
    pub fn notice(&self) -> Option<UpdateNotice> {
        let mut bytes = Vec::new();
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.directory.join("status.json"))
            .ok()?
            .take(MAX_STATE_BYTES + 1)
            .read_to_end(&mut bytes)
            .ok()?;
        if bytes.len() as u64 > MAX_STATE_BYTES {
            return None;
        }
        let state: UpdateState = serde_json::from_slice(&bytes).ok()?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(self.directory.join("update.lock"))
            .ok()?;
        let busy = if unsafe {
            libc::flock(
                std::os::fd::AsRawFd::as_raw_fd(&lock),
                libc::LOCK_EX | libc::LOCK_NB,
            )
        } == 0
        {
            unsafe {
                libc::flock(std::os::fd::AsRawFd::as_raw_fd(&lock), libc::LOCK_UN);
            }
            false
        } else if std::io::Error::last_os_error().kind() == std::io::ErrorKind::WouldBlock {
            true
        } else {
            return None;
        };
        if busy {
            return Some(match state.activity {
                Some(UpdateActivity::Installing) => UpdateNotice::Installing {
                    progress: state.progress.unwrap_or_default(),
                },
                _ => UpdateNotice::Checking,
            });
        }
        let running = semver::Version::parse(&self.installation.build.version).ok()?;
        if let Some(installed) = self
            .installed_version()
            .filter(|version| version > &running)
        {
            return Some(UpdateNotice::Ready {
                version: installed.to_string(),
            });
        }
        state.failed.then_some(UpdateNotice::Failed)
    }

    fn installed_version(&self) -> Option<semver::Version> {
        match &self.installation.source {
            Source::Native { root } | Source::CargoRegistry { root, .. } => {
                let installed = Installation::read(&root.join("current")).ok()?;
                if installed.source != self.installation.source {
                    return None;
                }
                semver::Version::parse(&installed.build.version).ok()
            }
            Source::Homebrew { executable } => {
                let program = executable.parent()?.join("tickloop").canonicalize().ok()?;
                let keg = program.parent()?.parent()?;
                let receipt: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(keg.join("INSTALL_RECEIPT.json")).ok()?)
                        .ok()?;
                let (tap, _) = crate::HOMEBREW_FORMULA.rsplit_once('/')?;
                if receipt["source"]["tap"].as_str()? != tap {
                    return None;
                }
                let version = keg.file_name()?.to_str()?;
                semver::Version::parse(version.split('_').next()?).ok()
            }
            Source::Cargo { root, .. } => {
                source::cargo_source(root).ok()?.map(|(_, version)| version)
            }
            Source::Checkout { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Build, Lock};

    fn fixture() -> (tempfile::TempDir, tempfile::TempDir, UpdateMonitor) {
        let root = tempfile::tempdir().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let installation = Installation {
            source: Source::Native {
                root: root.path().into(),
            },
            build: Build {
                version: "1.0.0".into(),
                identity: "fixture".into(),
            },
            pinned: false,
        };
        std::fs::create_dir(root.path().join("current")).unwrap();
        installation.write(&root.path().join("current")).unwrap();
        std::fs::write(
            directory.path().join("status.json"),
            serde_json::to_vec(&UpdateState::default()).unwrap(),
        )
        .unwrap();
        std::fs::write(directory.path().join("update.lock"), []).unwrap();
        let monitor = UpdateMonitor {
            installation,
            directory: directory.path().into(),
        };
        (root, directory, monitor)
    }

    #[test]
    fn activity_requires_a_live_lock_and_never_blocks_the_owner() {
        let (_root, directory, monitor) = fixture();
        let mut state = UpdateState::default();
        let lock = Lock::acquire(&directory.path().join("update.lock")).unwrap();
        state
            .set_activity(
                &directory.path().join("status.json"),
                UpdateActivity::Checking,
            )
            .unwrap();
        assert_eq!(monitor.notice(), Some(UpdateNotice::Checking));
        state
            .set_activity(
                &directory.path().join("status.json"),
                UpdateActivity::Installing,
            )
            .unwrap();
        assert_eq!(
            monitor.notice(),
            Some(UpdateNotice::Installing {
                progress: UpdateProgress::default()
            })
        );
        drop(lock);
        assert_eq!(
            monitor.notice(),
            None,
            "a crashed updater must not leave a permanent progress notice"
        );
    }

    #[test]
    fn download_progress_is_visible_only_while_the_updater_holds_its_lock() {
        let (_root, directory, monitor) = fixture();
        let path = directory.path().join("status.json");
        let mut state = UpdateState::default();
        let lock = Lock::acquire(&directory.path().join("update.lock")).unwrap();
        let progress = UpdateProgress::download(58, Some(100));
        crate::ProgressReporter::new(&mut state, &path)
            .report(progress)
            .unwrap();
        assert_eq!(
            monitor.notice(),
            Some(UpdateNotice::Installing { progress })
        );
        drop(lock);
        assert_eq!(monitor.notice(), None);
    }

    #[test]
    fn ready_follows_the_installed_version_until_this_console_uses_it() {
        let (root, _directory, mut monitor) = fixture();
        let mut installed = monitor.installation.clone();
        installed.build.version = "1.1.0".into();
        installed.write(&root.path().join("current")).unwrap();
        assert_eq!(
            monitor.notice(),
            Some(UpdateNotice::Ready {
                version: "1.1.0".into()
            })
        );
        monitor.installation.build.version = "1.1.0".into();
        assert_eq!(monitor.notice(), None);
    }

    #[test]
    fn failed_checks_are_visible_but_a_ready_runtime_takes_precedence() {
        let (root, directory, monitor) = fixture();
        let state = UpdateState {
            failed: true,
            ..Default::default()
        };
        std::fs::write(
            directory.path().join("status.json"),
            serde_json::to_vec(&state).unwrap(),
        )
        .unwrap();
        assert_eq!(monitor.notice(), Some(UpdateNotice::Failed));
        let mut installed = monitor.installation.clone();
        installed.build.version = "1.1.0".into();
        installed.write(&root.path().join("current")).unwrap();
        assert!(matches!(monitor.notice(), Some(UpdateNotice::Ready { .. })));
    }

    #[test]
    fn old_state_files_remain_readable_and_invalid_or_oversized_files_are_ignored() {
        let (_root, directory, monitor) = fixture();
        for bytes in [
            br#"{"attempted_at":1,"checked_at":1,"message":"already current"}"#.to_vec(),
            b"invalid json".to_vec(),
            vec![b' '; MAX_STATE_BYTES as usize + 1],
        ] {
            std::fs::write(directory.path().join("status.json"), bytes).unwrap();
            assert_eq!(monitor.notice(), None);
        }
    }

    #[test]
    fn homebrew_versions_are_observed_without_executing_brew_or_the_application() {
        use std::os::unix::fs::symlink;
        let (root, _directory, mut monitor) = fixture();
        let bin = root.path().join("bin");
        let installed = root.path().join("Cellar/tickloop/1.1.0_1/bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&installed).unwrap();
        std::fs::write(installed.join("tickloop"), b"this file must never execute").unwrap();
        symlink(installed.join("tickloop"), bin.join("tickloop")).unwrap();
        std::fs::write(
            installed.parent().unwrap().join("INSTALL_RECEIPT.json"),
            br#"{"source":{"tap":"tickloopai/tap"}}"#,
        )
        .unwrap();
        monitor.installation.source = Source::Homebrew {
            executable: bin.join("brew"),
        };
        assert_eq!(
            monitor.notice(),
            Some(UpdateNotice::Ready {
                version: "1.1.0".into()
            })
        );
        std::fs::write(
            installed.parent().unwrap().join("INSTALL_RECEIPT.json"),
            br#"{"source":{"tap":"other/tap"}}"#,
        )
        .unwrap();
        assert_eq!(monitor.notice(), None);
    }
}

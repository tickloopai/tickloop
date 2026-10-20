#[path = "../bundle.rs"]
mod bundle;

mod embedded {
    include!(concat!(env!("OUT_DIR"), "/payload.rs"));
}

pub fn run() {
    #[cfg(unix)]
    if let Err(error) = unix::launch() {
        eprintln!("Tickloop: {error}");
        std::process::exit(1);
    }
    #[cfg(not(unix))]
    {
        eprintln!("Tickloop currently supports macOS Apple Silicon and Intel only.");
        std::process::exit(1);
    }
}

#[cfg(unix)]
mod unix {
    use crate::bundle::output;
    use std::fs::{File, OpenOptions};
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    use std::os::unix::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    const VERSION: &str = env!("CARGO_PKG_VERSION");
    const PROGRAMS: [&str; 3] = ["tickloop", "tickhost", "tick-reader"];
    const LOCK_TIMEOUT: Duration = Duration::from_secs(15 * 60);

    unsafe extern "C" {
        fn geteuid() -> u32;
    }

    pub fn launch() -> Result<(), String> {
        let executable = std::env::current_exe()
            .map_err(display)?
            .canonicalize()
            .map_err(display)?;
        let program = match executable.file_name().and_then(|name| name.to_str()) {
            Some("tl" | "tickloop") => "tickloop",
            Some("tickhost") => "tickhost",
            Some("tick-reader") => "tick-reader",
            _ => return Err("Run the launcher from its Cargo-installed entry.".into()),
        };
        let arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
        let bin = executable
            .parent()
            .ok_or("The launcher has no installation directory")?;
        if bin.file_name().is_none_or(|name| name != "bin") {
            return Err("Install this launcher with cargo install tickloop.".into());
        }
        let cargo_root = bin.parent().ok_or("The launcher has no Cargo root")?;
        let root = cargo_root.join(".tickloop");
        if !usable_runtime(&root, VERSION) {
            if arguments.len() == 1 && matches!(arguments[0].to_str(), Some("--version" | "-V")) {
                println!("{program} {VERSION}");
                return Ok(());
            }
            let target = crate::embedded::TARGET;
            if !cargo_root.join(".crates2.json").is_file() {
                return Err("Cargo installation metadata is missing; reinstall with cargo install tickloop.".into());
            }
            private_directory(&root)?;
            let _lock = installation_lock(&root)?;
            if !usable_runtime(&root, VERSION) {
                install_release(
                    &root,
                    &bin.join("tickloop"),
                    target,
                    VERSION,
                    crate::embedded::ARCHIVE,
                    crate::embedded::CHECKSUM,
                )?;
            }
        }
        let payload = root
            .join("current")
            .join(program)
            .canonicalize()
            .map_err(display)?;
        let error = Command::new(payload).args(arguments).exec();
        Err(format!("Cannot open the installed program: {error}"))
    }

    fn private_directory(directory: &Path) -> Result<(), String> {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(directory)
            .map_err(display)?;
        let metadata = std::fs::symlink_metadata(directory).map_err(display)?;
        // geteuid has no preconditions and identifies the user who owns this installer process.
        let uid = unsafe { geteuid() };
        if !metadata.is_dir()
            || metadata.file_type().is_symlink()
            || metadata.uid() != uid
            || metadata.permissions().mode() & 0o077 != 0
        {
            return Err(format!(
                "Refusing a non-private installation directory: {}",
                directory.display()
            ));
        }
        Ok(())
    }

    fn installation_lock(root: &Path) -> Result<File, String> {
        let lock_path = root.join("bootstrap.lock");
        if let Ok(metadata) = std::fs::symlink_metadata(&lock_path)
            && (!metadata.is_file() || metadata.file_type().is_symlink())
        {
            return Err("Invalid launcher lock file.".into());
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)
            .map_err(display)?;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(display)?;
        let deadline = Instant::now() + LOCK_TIMEOUT;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(file),
                Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(error) => return Err(format!("Cannot lock the Tickloop installer: {error}")),
            }
        }
    }

    fn usable_runtime(root: &Path, minimum: &str) -> bool {
        if !root.join("current/installation.json").is_file()
            || PROGRAMS
                .iter()
                .any(|program| !root.join("current").join(program).is_file())
        {
            return false;
        }
        let Ok(output) = Command::new(root.join("current/tickloop"))
            .arg("--version")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
        else {
            return false;
        };
        let Some(reported) = std::str::from_utf8(&output.stdout)
            .ok()
            .and_then(|text| text.trim().strip_prefix("tickloop "))
            .and_then(version)
        else {
            return false;
        };
        output.status.success() && version(minimum).is_some_and(|minimum| reported >= minimum)
    }

    fn version(text: &str) -> Option<[u64; 3]> {
        let parts = text
            .split('.')
            .map(str::parse)
            .collect::<Result<Vec<u64>, _>>()
            .ok()?;
        parts.try_into().ok()
    }

    fn install_release(
        root: &Path,
        launcher: &Path,
        target: &str,
        version: &str,
        archive_bytes: &[u8],
        checksum: &str,
    ) -> Result<(), String> {
        let stage = Stage::create(root)?;
        let archive = stage.0.join("release.tar.gz");
        std::fs::write(&archive, archive_bytes).map_err(display)?;
        crate::bundle::unpack(&archive, checksum, target, &stage.0)?;
        let program = stage.0.join("tickloop");
        let reported = output(Command::new(&program).arg("--version"))?;
        if reported.trim() != format!("tickloop {version}") {
            return Err("The release reports an unexpected version.".into());
        }
        let status = Command::new(program)
            .args(["update", "--install-cargo"])
            .arg(launcher)
            .stdin(Stdio::null())
            .status()
            .map_err(display)?;
        if !status.success() {
            return Err("The verified release could not install its complete runtime.".into());
        }
        Ok(())
    }

    fn display(error: impl std::fmt::Display) -> String {
        error.to_string()
    }

    static NEXT_STAGE: AtomicU64 = AtomicU64::new(0);

    struct Stage(PathBuf);
    impl Stage {
        fn create(root: &Path) -> Result<Self, String> {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(display)?
                .as_nanos();
            let sequence = NEXT_STAGE.fetch_add(1, Ordering::Relaxed);
            let directory = root.join(format!(
                ".bootstrap-{}-{nonce}-{sequence}",
                std::process::id()
            ));
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&directory)
                .map_err(display)?;
            Ok(Self(directory))
        }
    }
    impl Drop for Stage {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn versions_are_numeric_and_reject_incomplete_or_prerelease_versions() {
            assert!(version("0.2.10").unwrap() > version("0.2.9").unwrap());
            assert_eq!(version("0.2"), None);
            assert_eq!(version("0.2.3-beta"), None);
        }

        #[test]
        fn lock_excludes_other_installers_and_releases_on_close() {
            let parent = Stage::create(&std::env::temp_dir()).unwrap();
            let first = installation_lock(&parent.0).unwrap();
            let second = OpenOptions::new()
                .read(true)
                .write(true)
                .open(parent.0.join("bootstrap.lock"))
                .unwrap();
            assert!(matches!(
                second.try_lock(),
                Err(std::fs::TryLockError::WouldBlock)
            ));
            drop(first);
            // A child forked by a parallel test briefly shares the first descriptor until it
            // execs, so the flock can outlive the drop by a moment.
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                match second.try_lock() {
                    Ok(()) => break,
                    Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("The released lock stayed unavailable: {error}"),
                }
            }
        }

        #[test]
        fn unverified_bundle_never_executes_or_replaces_runtime() {
            let parent = Stage::create(&std::env::temp_dir()).unwrap();
            std::fs::write(parent.0.join("keep"), b"existing").unwrap();
            let result = install_release(
                &parent.0,
                Path::new("/unused/launcher"),
                "aarch64-apple-darwin",
                "0.2.4",
                b"corrupt",
                &format!("{}  tickloop-aarch64-apple-darwin.tar.gz", "0".repeat(64)),
            );
            assert!(result.unwrap_err().contains("SHA-256 mismatch"));
            assert_eq!(std::fs::read(parent.0.join("keep")).unwrap(), b"existing");
            assert_eq!(std::fs::read_dir(&parent.0).unwrap().count(), 1);
        }

        #[test]
        fn verified_bundle_installs_at_custom_cargo_root_without_replacing_its_launcher() {
            let parent = Stage::create(&std::env::temp_dir()).unwrap();
            let cargo_root = parent.0.join("Cargo root with spaces");
            std::fs::create_dir_all(cargo_root.join("bin")).unwrap();
            let launcher = cargo_root.join("bin/tickloop");
            std::fs::write(&launcher, b"Cargo-owned launcher").unwrap();
            let runtime = cargo_root.join(".tickloop");
            private_directory(&runtime).unwrap();
            let payload = parent.0.join("payload");
            std::fs::create_dir(&payload).unwrap();
            let installer = r#"#!/bin/sh
if [ "$1" = --version ]; then echo 'tickloop 1.2.3'; exit 0; fi
[ "$1" = update ] && [ "$2" = --install-cargo ] || exit 2
runtime="$(dirname "$(dirname "$3")")/.tickloop"
mkdir -p "$runtime/versions/1.2.3"
for program in tickloop tickhost tick-reader; do cp "$(dirname "$0")/$program" "$runtime/versions/1.2.3/$program"; done
printf '{}' > "$runtime/versions/1.2.3/installation.json"
ln -s versions/1.2.3 "$runtime/current"
pwd -P > "$runtime/installer-cwd"
"#;
            for program in PROGRAMS {
                let content = if program == "tickloop" {
                    installer.to_owned()
                } else {
                    format!("#!/bin/sh\necho '{program} 1.2.3'\n")
                };
                std::fs::write(payload.join(program), content).unwrap();
                std::fs::set_permissions(
                    payload.join(program),
                    std::fs::Permissions::from_mode(0o700),
                )
                .unwrap();
            }
            let archive = parent.0.join("bundle.tar.gz");
            assert!(
                Command::new("/usr/bin/tar")
                    .env("COPYFILE_DISABLE", "1")
                    .args(["--format=ustar", "-czf"])
                    .arg(&archive)
                    .arg("-C")
                    .arg(&payload)
                    .args(PROGRAMS)
                    .status()
                    .unwrap()
                    .success()
            );
            let digest = output(
                Command::new("/usr/bin/shasum")
                    .args(["-a", "256"])
                    .arg(&archive),
            )
            .unwrap();
            install_release(
                &runtime,
                &launcher,
                "aarch64-apple-darwin",
                "1.2.3",
                &std::fs::read(&archive).unwrap(),
                &format!(
                    "{}  tickloop-aarch64-apple-darwin.tar.gz\n",
                    digest.split_whitespace().next().unwrap()
                ),
            )
            .unwrap();
            assert_eq!(std::fs::read(launcher).unwrap(), b"Cargo-owned launcher");
            assert!(usable_runtime(&runtime, "1.2.3"));
            assert!(usable_runtime(&runtime, "1.2.2"));
            assert!(!usable_runtime(&runtime, "1.2.4"));
            let observed = std::fs::read_to_string(runtime.join("installer-cwd")).unwrap();
            assert_eq!(
                Path::new(observed.trim()),
                std::env::current_dir().unwrap().canonicalize().unwrap()
            );
        }
    }
}

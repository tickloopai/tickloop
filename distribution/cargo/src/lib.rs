//! A small launcher for official prebuilt Tickloop releases. No application source is bundled.

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
    use std::fs::{File, OpenOptions};
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    use std::os::unix::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    const VERSION: &str = env!("CARGO_PKG_VERSION");
    const PROGRAMS: [&str; 3] = ["tickloop", "tickhost", "tick-reader"];
    const RELEASES: &str = "https://github.com/tickloopai/homebrew-tap/releases/download";
    const LOCK_TIMEOUT: Duration = Duration::from_secs(15 * 60);
    const MAX_DOWNLOAD_BYTES: &str = "268435456";

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
            let target = match (std::env::consts::OS, std::env::consts::ARCH) {
                ("macos", "aarch64") => "aarch64-apple-darwin",
                ("macos", "x86_64") => "x86_64-apple-darwin",
                _ => {
                    return Err(
                        "Tickloop currently supports macOS Apple Silicon and Intel only.".into(),
                    );
                }
            };
            if !cargo_root.join(".crates2.json").is_file() {
                return Err("Cargo installation metadata is missing; reinstall with cargo install tickloop.".into());
            }
            private_directory(&root)?;
            let _lock = installation_lock(&root)?;
            if !usable_runtime(&root, VERSION) {
                eprintln!("Installing Tickloop {VERSION} for {target}…");
                install_release(&root, &bin.join("tickloop"), target, VERSION, download)?;
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
        fetch: impl Fn(&str, &Path) -> Result<(), String>,
    ) -> Result<(), String> {
        let stage = Stage::create(root)?;
        let archive_name = format!("tickloop-{target}.tar.gz");
        let archive = stage.0.join(&archive_name);
        let checksum_file = stage.0.join("checksum");
        let base = format!("{RELEASES}/v{version}/{archive_name}");
        fetch(&base, &archive)?;
        fetch(&format!("{base}.sha256"), &checksum_file)?;
        let checksum = std::fs::read_to_string(&checksum_file).map_err(display)?;
        let fields = checksum.split_whitespace().collect::<Vec<_>>();
        if fields.len() != 2
            || fields[0].len() != 64
            || !fields[0].bytes().all(|byte| byte.is_ascii_hexdigit())
            || fields[1].trim_start_matches('*') != archive_name
        {
            return Err("Invalid release checksum file.".into());
        }
        let actual = output(
            Command::new("/usr/bin/shasum")
                .args(["-a", "256"])
                .arg(&archive),
        )?;
        if actual
            .split_whitespace()
            .next()
            .is_none_or(|value| !value.eq_ignore_ascii_case(fields[0]))
        {
            return Err(
                "Release SHA-256 mismatch; the existing installation was not replaced.".into(),
            );
        }
        let entries = output(Command::new("/usr/bin/tar").arg("-tzf").arg(&archive))?;
        let mut names = entries.lines().collect::<Vec<_>>();
        names.sort_unstable();
        if names != ["tick-reader", "tickhost", "tickloop"] {
            return Err("The release archive contains unexpected or missing files.".into());
        }
        let modes = output(Command::new("/usr/bin/tar").arg("-tvzf").arg(&archive))?;
        if modes.lines().count() != PROGRAMS.len()
            || modes.lines().any(|line| !line.starts_with('-'))
        {
            return Err("The release archive must contain only regular programs.".into());
        }
        let status = Command::new("/usr/bin/tar")
            .arg("-xzf")
            .arg(&archive)
            .arg("-C")
            .arg(&stage.0)
            .stdin(Stdio::null())
            .status()
            .map_err(display)?;
        if !status.success() {
            return Err("Cannot extract the verified release.".into());
        }
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

    fn download(url: &str, destination: &Path) -> Result<(), String> {
        let status = Command::new("/usr/bin/curl")
            .args([
                "--proto",
                "=https",
                "--proto-redir",
                "=https",
                "--tlsv1.2",
                "-fsSL",
                "--connect-timeout",
                "15",
                "--max-time",
                "600",
                "--max-filesize",
                MAX_DOWNLOAD_BYTES,
            ])
            .arg(url)
            .arg("-o")
            .arg(destination)
            .stdin(Stdio::null())
            .status()
            .map_err(display)?;
        if status.success() {
            Ok(())
        } else {
            Err("Cannot download the official release. Check your connection and try launching again.".into())
        }
    }

    fn output(command: &mut Command) -> Result<String, String> {
        let result = command.stdin(Stdio::null()).output().map_err(display)?;
        if !result.status.success() {
            return Err(format!("Release validation failed: {}", result.status));
        }
        String::from_utf8(result.stdout).map_err(display)
    }

    fn display(error: impl std::fmt::Display) -> String {
        error.to_string()
    }

    struct Stage(PathBuf);
    impl Stage {
        fn create(root: &Path) -> Result<Self, String> {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(display)?
                .as_nanos();
            let directory = root.join(format!(".bootstrap-{}-{nonce}", std::process::id()));
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
        use std::io::Write;

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
            second.try_lock().unwrap();
        }

        #[test]
        fn unverified_download_never_executes_or_replaces_runtime() {
            let parent = Stage::create(&std::env::temp_dir()).unwrap();
            std::fs::write(parent.0.join("keep"), b"existing").unwrap();
            let result = install_release(
                &parent.0,
                Path::new("/unused/launcher"),
                "aarch64-apple-darwin",
                "0.2.4",
                |url, target| {
                    let mut file = File::create(target).map_err(display)?;
                    if url.ends_with(".sha256") {
                        write!(
                            file,
                            "{}  tickloop-aarch64-apple-darwin.tar.gz",
                            "0".repeat(64)
                        )
                        .map_err(display)?;
                    } else {
                        file.write_all(b"corrupt").map_err(display)?;
                    }
                    Ok(())
                },
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
                |url, destination| {
                    assert!(url.starts_with(&format!("{RELEASES}/v1.2.3/")));
                    if url.ends_with(".sha256") {
                        std::fs::write(
                            destination,
                            format!(
                                "{}  tickloop-aarch64-apple-darwin.tar.gz\n",
                                digest.split_whitespace().next().unwrap()
                            ),
                        )
                        .map_err(display)?;
                    } else {
                        std::fs::copy(&archive, destination).map_err(display)?;
                    }
                    Ok(())
                },
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

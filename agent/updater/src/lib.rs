//! Installation and updates belong to the executable, independently of Agent execution.

mod bundle;
mod release;
mod source;
#[cfg(test)]
mod tests;

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::IsTerminal;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tickloop_agent_base::private_fs::{
    create_private_directory, replace_file_atomically, verify_private_directory,
};

use source::{Installation, Source};

pub const PROGRAMS: [&str; 3] = ["tickloop", "tickhost", "tick-reader"];
pub const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const RETRY_INTERVAL: Duration = Duration::from_secs(60 * 60);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const RUNTIME_LOCK_TIMEOUT: Duration = Duration::from_secs(10);
const HOMEBREW_FORMULA: &str = "tickloopai/tap/tickloop";

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Build {
    pub version: String,
    pub identity: String,
}

#[derive(Default, Deserialize, Serialize)]
struct UpdateState {
    attempted_at: u64,
    checked_at: u64,
    message: String,
}

pub(crate) fn data_root() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".local/share/tickloop"))
}

pub(crate) fn private_directory(path: &Path) -> Result<()> {
    create_private_directory(path)?;
    verify_private_directory(path, "Tickloop installation directory")?;
    Ok(())
}

pub(crate) struct Lock(File);

impl Lock {
    pub(crate) fn acquire(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(std::io::Error::last_os_error()).context(
                "Another Tickloop installation or update is in progress; try again later",
            );
        }
        Ok(Self(file))
    }

    fn wait(path: &Path) -> Result<Self> {
        let deadline = std::time::Instant::now() + RUNTIME_LOCK_TIMEOUT;
        loop {
            match Self::acquire(path) {
                Ok(lock) => return Ok(lock),
                Err(error)
                    if error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|error| error.kind() == std::io::ErrorKind::WouldBlock)
                        && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(error) => return Err(error),
            }
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

/// Retains a complete runtime before brew/Cargo can replace or clean their installation.
/// Native versions are already immutable and retained by their installer.
pub fn prepare_launch(build: &Build) -> Result<Option<PathBuf>> {
    let reported_executable = std::env::current_exe()?;
    let executable = reported_executable.canonicalize()?;
    let Some(installation) = Installation::detect(&executable)? else {
        return Ok(None);
    };
    if installation.pinned || matches!(installation.source, Source::Native { .. }) {
        if installation.build.identity != build.identity
            || installation.build.version != build.version
        {
            bail!("This executable does not match its installation record");
        }
        return Ok((reported_executable != executable).then_some(executable));
    }
    let root = installation.state_directory()?.join("runtimes");
    private_directory(&root)?;
    let _lock = Lock::wait(&root.join("install.lock"))?;
    bundle::remove_staging(&root, &[".runtime-"])?;
    let destination = root.join(&build.identity);
    if !destination.exists() {
        let stage = tempfile::Builder::new()
            .prefix(".runtime-")
            .tempdir_in(&root)?;
        bundle::copy_programs(
            executable.parent().context("executable has no parent")?,
            stage.path(),
        )?;
        bundle::verify(stage.path(), build)?;
        let mut record = installation;
        record.pinned = true;
        record.build = build.clone();
        record.write(stage.path())?;
        std::fs::rename(stage.path(), &destination)?;
    } else {
        let record = Installation::read(&destination)?;
        if record.build.identity != build.identity || !record.pinned {
            bail!(
                "The pinned runtime directory has unexpected contents: {}",
                destination.display()
            );
        }
        bundle::verify(&destination, build)?;
    }
    Ok(Some(destination.join("tickloop")))
}

/// Starts a detached, throttled update check only for an ordinary interactive Console.
pub fn start_background(arguments: &[OsString]) -> Result<()> {
    if !std::io::stdin().is_terminal()
        || !std::io::stdout().is_terminal()
        || arguments
            .iter()
            .any(|arg| matches!(arg.to_str(), Some("--help" | "-h" | "--version")))
    {
        return Ok(());
    }
    let executable = std::env::current_exe()?.canonicalize()?;
    let Some(installation) = Installation::detect(&executable)? else {
        return Ok(());
    };
    if matches!(
        installation.source,
        Source::Cargo { .. } | Source::Checkout { .. }
    ) {
        return Ok(());
    }
    let state_path = installation.state_directory()?.join("status.json");
    let state: UpdateState = std::fs::read(&state_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    if !is_due(&state, now()) {
        return Ok(());
    }
    let log = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(state_path.with_file_name("update.log"))?;
    let mut command = std::process::Command::new(executable);
    command
        .args(["update", "--background"])
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    use std::os::unix::process::CommandExt;
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn()?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn is_due(state: &UpdateState, at: u64) -> bool {
    at.saturating_sub(state.checked_at) >= CHECK_INTERVAL.as_secs()
        && at.saturating_sub(state.attempted_at) >= RETRY_INTERVAL.as_secs()
}

pub async fn command(arguments: &[OsString], build: Build) -> Result<()> {
    let args = arguments
        .iter()
        .map(|arg| arg.to_str().context("update arguments must be UTF-8"))
        .collect::<Result<Vec<_>>>()?;
    if args == ["--help"] || args == ["-h"] {
        println!(
            "Usage: tickloop update [--check | --status]\n\nUpdates the complete Tickloop installation through its original installer.\n--check   Check without installing\n--status  Show the most recent automatic update result\n\nUpdates never stop running Agents or Ticks."
        );
        return Ok(());
    }
    if args == ["--install"] {
        return bundle::install_native(build).await;
    }
    let background = args == ["--background"];
    let check = args == ["--check"];
    let status = args == ["--status"];
    if !args.is_empty() && !background && !check && !status {
        bail!("Usage: tickloop update [--check | --status]");
    }
    let executable = std::env::current_exe()?.canonicalize()?;
    let installation = Installation::detect(&executable)?.context(
        "This is an unmanaged/development build. Update its source and rebuild, or install a release through Homebrew, Cargo or the native installer.")?;
    let directory = installation.state_directory()?;
    let state_path = directory.join("status.json");
    let mut state: UpdateState = std::fs::read(&state_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    if status {
        println!(
            "Running: {}\nSource: {}\n{}",
            build.version,
            installation.source.label(),
            if state.message.is_empty() {
                "No update has been attempted."
            } else {
                &state.message
            }
        );
        return Ok(());
    }
    let _lock = Lock::acquire(&directory.join("update.lock"))?;
    state = std::fs::read(&state_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    if background && !is_due(&state, now()) {
        return Ok(());
    }
    state.attempted_at = now();
    replace_file_atomically(&state_path, &serde_json::to_vec(&state)?, 0o600)?;
    let result = update(&installation, &build, check).await;
    match &result {
        Ok(message) => {
            state.checked_at = now();
            state.message = message.clone();
        }
        Err(error) => {
            state.message = format!("Update failed: {error:#}");
        }
    }
    replace_file_atomically(&state_path, &serde_json::to_vec(&state)?, 0o600)?;
    println!("{}", state.message);
    if result.is_ok() && !check {
        println!(
            "Running Agents and Ticks were left running. A new launch may ask which running version to use."
        );
    }
    result.map(|_| ())
}

async fn update(installation: &Installation, build: &Build, check: bool) -> Result<String> {
    match &installation.source {
        Source::Checkout { root } => bail!(
            "This Cargo installation comes from the local checkout {}. Update that checkout explicitly, then run make install there; automatic updates do not modify source worktrees.",
            root.display()
        ),
        Source::Native { root } => {
            private_directory(root)?;
            let _lock = Lock::acquire(&root.join("install.lock"))?;
            bundle::remove_staging(root, &[".install-", ".download-", ".activate-"])?;
            let release = release::latest().await?;
            let current = Installation::read(&root.join("current"))?;
            if !matches!(&current.source, Source::Native { root: current_root } if current_root == root)
            {
                bail!(
                    "The installation source changed; run the current installation's update command instead"
                );
            }
            if release.version <= semver::Version::parse(&current.build.version)? {
                return Ok(format!(
                    "Already installed: {} (running {}).",
                    current.build.version, build.version
                ));
            }
            if check {
                return Ok(format!(
                    "Available: {} → {}",
                    current.build.version, release.version
                ));
            }
            bundle::download_install(root, &release).await?;
            Ok(format!(
                "Installed {}. Available on the next launch.",
                release.version
            ))
        }
        Source::Homebrew { executable } => {
            let mut refresh = tokio::process::Command::new(executable);
            refresh.args(["update", "--quiet"]);
            run_process(&mut refresh).await?;
            let mut command = tokio::process::Command::new(executable);
            if check {
                command.args(["outdated", "--verbose", HOMEBREW_FORMULA]);
            } else {
                command
                    .args(["upgrade", HOMEBREW_FORMULA])
                    .env("HOMEBREW_NO_INSTALL_CLEANUP", "1")
                    .env("HOMEBREW_NO_INSTALLED_DEPENDENTS_CHECK", "1");
            }
            run_process(&mut command).await?;
            Ok(if check {
                "Homebrew update check completed."
            } else {
                "Homebrew upgrade completed; use the installed entry on your next launch."
            }
            .into())
        }
        Source::Cargo { root, repository } => {
            let release = release::latest().await?;
            let (current_repository, installed) = source::cargo_source(root)?
                .context("Cargo no longer records this Tickloop installation")?;
            if &current_repository != repository {
                bail!(
                    "The Cargo repository changed; run the current installation's update command instead"
                );
            }
            if release.version <= installed {
                return Ok(format!(
                    "Already installed: {installed} (running {}).",
                    build.version
                ));
            }
            if check {
                return Ok(format!("Available: {installed} → {}", release.version));
            }
            let stage = tempfile::Builder::new()
                .prefix(".cargo-build-")
                .tempdir_in(installation.state_directory()?)?;
            let target = stage.path().join("target");
            let mut cargo = tokio::process::Command::new("cargo");
            cargo
                .args([
                    "install",
                    "--locked",
                    "--git",
                    repository,
                    "--tag",
                    &format!("v{}", release.version),
                    "--root",
                ])
                .arg(stage.path())
                .arg("--target-dir")
                .arg(&target)
                .args([
                    "tickloop-agent",
                    "tickloop-tickhost",
                    "tickloop-tick-reader",
                ]);
            run_process(&mut cargo).await?;
            let bin = stage.path().join("bin");
            let candidate = bundle::read_build(&bin)?;
            if candidate.version != release.version.to_string() {
                bail!("Cargo built an unexpected version");
            }
            bundle::verify(&bin, &candidate)?;
            let revision = source::cargo_revision(stage.path())?;
            // Cargo owns the final install and its tracking files. The full build above must pass
            // first; running processes have already pinned their complete old runtime.
            let mut cargo = tokio::process::Command::new("cargo");
            cargo
                .args([
                    "install", "--locked", "--git", repository, "--rev", &revision, "--root",
                ])
                .arg(root)
                .arg("--target-dir")
                .arg(&target)
                .args([
                    "tickloop-agent",
                    "tickloop-tickhost",
                    "tickloop-tick-reader",
                ]);
            run_process(&mut cargo).await?;
            bundle::verify(&root.join("bin"), &candidate)?;
            if source::cargo_revision(root)? != revision {
                bail!(
                    "Cargo did not install the verified source revision; inspect the installation before retrying"
                );
            }
            Ok(format!(
                "Cargo installed {}. Available on the next launch.",
                release.version
            ))
        }
    }
}

async fn run_process(command: &mut tokio::process::Command) -> Result<()> {
    command
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .process_group(0);
    let mut child = command.spawn()?;
    let mut group = ProcessGroup {
        process_id: child.id().context("Installer has no process id")?,
        is_armed: true,
    };
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let status = tokio::select! {
        result = tokio::time::timeout(COMMAND_TIMEOUT, child.wait()) => {
            result.context("Update command timed out; inspect the installer before retrying")??
        }
        signal = tokio::signal::ctrl_c() => {
            signal?;
            bail!("Update cancelled; inspect the installer state before retrying");
        }
        _ = terminate.recv() => bail!("Update terminated; inspect the installer state before retrying"),
    };
    if !status.success() {
        bail!("Installer exited with {status}");
    }
    group.is_armed = false;
    Ok(())
}

struct ProcessGroup {
    process_id: u32,
    is_armed: bool,
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        if self.is_armed {
            // This is the dedicated process group we created for this installer, never a user PID.
            unsafe {
                libc::kill(-(self.process_id as i32), libc::SIGKILL);
            }
        }
    }
}

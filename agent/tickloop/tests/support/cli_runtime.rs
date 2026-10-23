use std::{
    cell::RefCell,
    path::{Path, PathBuf},
    time::Duration,
};

use tickloop_agent_client::LocalAgentClient;
use tickloop_agent_protocol::{AgentAccess, AgentCommand, CommandEnvelope};
use uuid::Uuid;

const DAEMON_SOCKET: &str = "daemon.sock";
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(15);

thread_local! {
    static DAEMONS: RefCell<Vec<PrivateDaemon>> = const { RefCell::new(Vec::new()) };
}

/// A CLI fixture's config root and the runtime directories derived from it. Dropping it stops
/// the daemon there gracefully, flushing its Transcripts, then removes the daemon and Tickhost
/// runtime directories, which also ends a daemon that did not stop.
struct PrivateDaemon {
    _config_root: Option<tempfile::TempDir>,
    runtime: PathBuf,
    tickhost: PathBuf,
}

impl PrivateDaemon {
    fn of(config_root: &Path, owned: Option<tempfile::TempDir>) -> Self {
        Self {
            runtime: daemon_directory_of(config_root),
            tickhost: tickhost::paths::HostPaths::of_config_root(Some(config_root)).runtime,
            _config_root: owned,
        }
    }
}

/// A private config root, so the CLI started with it in `TICKLOOP_CONFIG_DIR` gets a daemon and
/// a Tickhost of its own.
pub fn config_root() -> PathBuf {
    let directory = super::runtime_dir();
    let path = directory.path().to_path_buf();
    DAEMONS.with(|daemons| {
        daemons
            .borrow_mut()
            .push(PrivateDaemon::of(&path, Some(directory)))
    });
    path
}

/// Ends the daemon of a config root the test made itself when the test ends, and returns the
/// daemon's runtime directory.
pub fn adopt(config_root: &Path) -> PathBuf {
    let daemon = PrivateDaemon::of(config_root, None);
    let runtime = daemon.runtime.clone();
    DAEMONS.with(|daemons| daemons.borrow_mut().push(daemon));
    runtime
}

/// Runs `executable` with the fixture's config root when an API accepts only an executable path.
pub fn executable_in(config_root: &Path, executable: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let launcher = config_root.join(format!(
        "fixture-{}",
        executable.file_name().unwrap().to_str().unwrap()
    ));
    if !launcher.exists() {
        std::fs::create_dir_all(config_root).unwrap();
        std::fs::write(
            &launcher,
            format!(
                "#!/bin/sh\nexport TICKLOOP_CONFIG_DIR={}\nexec {} \"$@\"\n",
                shell_words::quote(config_root.to_str().unwrap()),
                shell_words::quote(executable.to_str().unwrap()),
            ),
        )
        .unwrap();
        std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    launcher
}

/// The runtime directory of the daemon serving `config_root`.
pub fn daemon_directory_of(config_root: &Path) -> PathBuf {
    tickloop_agent_client::daemon_endpoint::runtime_directory_of(Some(config_root))
}

/// The runtime directory of the daemon a CLI fixture command reaches through its config root.
pub fn command_directory(command: &std::process::Command) -> PathBuf {
    command
        .get_envs()
        .find_map(|(name, value)| (name == "TICKLOOP_CONFIG_DIR").then_some(value).flatten())
        .map(|config_root| daemon_directory_of(Path::new(config_root)))
        .expect("CLI fixture has no private config root")
}

fn verify_owned(directory: &Path) -> Result<(), String> {
    let is_owned = DAEMONS.with(|daemons| {
        daemons
            .borrow()
            .iter()
            .any(|daemon| daemon.runtime == directory)
    });
    if !is_owned {
        return Err("refusing to access an endpoint not owned by this CLI fixture".into());
    }
    Ok(())
}

pub async fn flush_transcripts(directory: &Path) -> Result<(), String> {
    verify_owned(directory)?;
    tokio::time::timeout(CLEANUP_TIMEOUT, async {
        let client = LocalAgentClient::connect(directory.join(DAEMON_SOCKET))
            .await
            .map_err(|error| error.to_string())?;
        flush(&client).await
    })
    .await
    .map_err(|_| "private CLI Transcript flush timed out".to_owned())?
}

pub async fn shutdown_daemon(directory: &Path) -> Result<(), String> {
    verify_owned(directory)?;
    tokio::time::timeout(CLEANUP_TIMEOUT, async {
        let client = LocalAgentClient::connect(directory.join(DAEMON_SOCKET))
            .await
            .map_err(|error| error.to_string())?;
        flush(&client).await?;
        client
            .shutdown_daemon()
            .await
            .map_err(|error| error.to_string())?;
        while directory.join(DAEMON_SOCKET).exists() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        Ok(())
    })
    .await
    .map_err(|_| "private CLI daemon shutdown timed out".to_owned())?
}

async fn flush(client: &LocalAgentClient) -> Result<(), String> {
    client
        .execute(CommandEnvelope {
            daemon_epoch: client.daemon_epoch(),
            command_id: Uuid::new_v4(),
            command: AgentCommand::FlushTranscripts,
        })
        .await
        .map_err(|error| error.to_string())?
        .into_result()
        .map_err(|error| error.to_string())?;
    Ok(())
}

impl Drop for PrivateDaemon {
    fn drop(&mut self) {
        let directory = self.runtime.clone();
        if directory.join(DAEMON_SOCKET).exists() {
            stop_gracefully(&directory);
        }
        let _ = std::fs::remove_dir_all(&self.runtime);
        let _ = std::fs::remove_dir_all(&self.tickhost);
    }
}

fn stop_gracefully(directory: &Path) {
    // Cleanup runs after the test's Tokio runtime may already have been dropped.
    let cleanup_directory = directory.to_path_buf();
    let cleanup = std::thread::Builder::new()
        .name("private-cli-daemon-cleanup".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| error.to_string())?;
            runtime.block_on(async {
                tokio::time::timeout(CLEANUP_TIMEOUT, async {
                    let client = LocalAgentClient::connect(cleanup_directory.join(DAEMON_SOCKET))
                        .await
                        .map_err(|error| error.to_string())?;
                    let flushed = flush(&client).await;
                    client
                        .shutdown_daemon()
                        .await
                        .map_err(|error| error.to_string())?;
                    while cleanup_directory.join(DAEMON_SOCKET).exists() {
                        tokio::time::sleep(Duration::from_millis(25)).await;
                    }
                    flushed
                })
                .await
                .map_err(|_| "private CLI daemon shutdown timed out".to_owned())?
            })
        });
    let result = match cleanup {
        Ok(thread) => thread
            .join()
            .unwrap_or_else(|_| Err("private CLI daemon cleanup thread panicked".into())),
        Err(error) => Err(error.to_string()),
    };
    if let Err(error) = result {
        eprintln!(
            "Private CLI daemon cleanup failed; removing {} ends the daemon: {error}",
            directory.display()
        );
    }
}

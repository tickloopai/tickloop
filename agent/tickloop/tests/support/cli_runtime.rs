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

/// A CLI fixture's runtime directory. Dropping it stops the daemon there gracefully, flushing its
/// Transcripts, then removes the directory, which also ends a daemon that did not stop.
struct PrivateDaemon(tempfile::TempDir);

pub fn directory() -> PathBuf {
    let directory = super::runtime_dir();
    let path = directory.path().to_path_buf();
    DAEMONS.with(|daemons| daemons.borrow_mut().push(PrivateDaemon(directory)));
    path
}

pub fn command_directory(command: &std::process::Command) -> PathBuf {
    command
        .get_envs()
        .find_map(|(name, value)| {
            (name == "TICKLOOP_DAEMON_RUNTIME_DIR")
                .then_some(value)
                .flatten()
        })
        .map(PathBuf::from)
        .expect("CLI fixture has no private Agent runtime directory")
}

fn verify_owned(directory: &Path) -> Result<(), String> {
    let is_owned = DAEMONS.with(|daemons| {
        daemons
            .borrow()
            .iter()
            .any(|daemon| daemon.0.path() == directory)
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
        let directory = self.0.path().to_path_buf();
        if !directory.join(DAEMON_SOCKET).exists() {
            return;
        }
        // Cleanup runs after the test's Tokio runtime may already have been dropped.
        let cleanup_directory = directory.clone();
        let cleanup = std::thread::Builder::new()
            .name("private-cli-daemon-cleanup".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|error| error.to_string())?;
                runtime.block_on(async {
                    tokio::time::timeout(CLEANUP_TIMEOUT, async {
                        let client =
                            LocalAgentClient::connect(cleanup_directory.join(DAEMON_SOCKET))
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
}

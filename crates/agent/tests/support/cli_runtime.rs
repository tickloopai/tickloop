use std::{
    cell::RefCell,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::Duration,
};

use tickloop_agent::agent_access::{AgentAccess, AgentCommand, CommandEnvelope, LocalAgentClient};
use uuid::Uuid;

const OWNER_SOCKET: &str = "owner.sock";
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(15);

thread_local! {
    static OWNERS: RefCell<Vec<PrivateOwner>> = const { RefCell::new(Vec::new()) };
}

struct PrivateOwner(Option<tempfile::TempDir>);

pub fn directory() -> PathBuf {
    let directory = tempfile::Builder::new()
        .prefix("tcli-")
        .tempdir_in("/tmp")
        .unwrap();
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = directory.path().to_path_buf();
    OWNERS.with(|owners| owners.borrow_mut().push(PrivateOwner(Some(directory))));
    path
}

pub fn command_directory(command: &std::process::Command) -> PathBuf {
    command
        .get_envs()
        .find_map(|(name, value)| {
            (name == "TICKLOOP_AGENT_RUNTIME_DIR")
                .then_some(value)
                .flatten()
        })
        .map(PathBuf::from)
        .expect("CLI fixture has no private Agent runtime directory")
}

fn verify_owned(directory: &Path) -> Result<(), String> {
    let is_owned = OWNERS.with(|owners| {
        owners.borrow().iter().any(|owner| {
            owner
                .0
                .as_ref()
                .is_some_and(|owned| owned.path() == directory)
        })
    });
    if !is_owned {
        return Err("refusing to access an endpoint not owned by this CLI fixture".into());
    }
    Ok(())
}

pub async fn flush_transcripts(directory: &Path) -> Result<(), String> {
    verify_owned(directory)?;
    tokio::time::timeout(CLEANUP_TIMEOUT, async {
        let client = LocalAgentClient::connect(directory.join(OWNER_SOCKET))
            .await
            .map_err(|error| error.to_string())?;
        flush(&client).await
    })
    .await
    .map_err(|_| "private CLI Transcript flush timed out".to_owned())?
}

pub async fn shutdown_owner(directory: &Path) -> Result<(), String> {
    verify_owned(directory)?;
    tokio::time::timeout(CLEANUP_TIMEOUT, async {
        let client = LocalAgentClient::connect(directory.join(OWNER_SOCKET))
            .await
            .map_err(|error| error.to_string())?;
        flush(&client).await?;
        client
            .shutdown_owner()
            .await
            .map_err(|error| error.to_string())?;
        while directory.join(OWNER_SOCKET).exists() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        Ok(())
    })
    .await
    .map_err(|_| "private CLI owner shutdown timed out".to_owned())?
}

async fn flush(client: &LocalAgentClient) -> Result<(), String> {
    client
        .execute(CommandEnvelope {
            owner_epoch: client.owner_epoch(),
            command_id: Uuid::new_v4(),
            command: AgentCommand::FlushTranscripts,
        })
        .await
        .map_err(|error| error.to_string())?
        .into_result()
        .map_err(|error| error.to_string())?;
    Ok(())
}

impl Drop for PrivateOwner {
    fn drop(&mut self) {
        let Some(owned) = self.0.take() else {
            return;
        };
        if !owned.path().join(OWNER_SOCKET).exists() {
            return;
        }
        // Cleanup runs after the test's Tokio runtime may already have been dropped.
        let directory = owned.keep();
        let cleanup_directory = directory.clone();
        let cleanup = std::thread::Builder::new()
            .name("private-cli-owner-cleanup".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|error| error.to_string())?;
                runtime.block_on(async {
                    tokio::time::timeout(CLEANUP_TIMEOUT, async {
                        let client =
                            LocalAgentClient::connect(cleanup_directory.join(OWNER_SOCKET))
                                .await
                                .map_err(|error| error.to_string())?;
                        let flushed = flush(&client).await;
                        client
                            .shutdown_owner()
                            .await
                            .map_err(|error| error.to_string())?;
                        while cleanup_directory.join(OWNER_SOCKET).exists() {
                            tokio::time::sleep(Duration::from_millis(25)).await;
                        }
                        flushed?;
                        std::fs::remove_dir_all(&cleanup_directory)
                            .map_err(|error| error.to_string())
                    })
                    .await
                    .map_err(|_| "private CLI owner shutdown timed out".to_owned())?
                })
            });
        let result = match cleanup {
            Ok(thread) => thread
                .join()
                .unwrap_or_else(|_| Err("private CLI owner cleanup thread panicked".into())),
            Err(error) => Err(error.to_string()),
        };
        if let Err(error) = result {
            eprintln!(
                "Private CLI owner cleanup failed; retained {}: {error}",
                directory.display()
            );
        }
    }
}

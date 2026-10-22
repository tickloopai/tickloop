//! Worker-owned ACP claims. A daemon disappearing never frees a still-running Worker's claim.
use super::{Agent, AgentError};
use std::{
    fs::OpenOptions,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
};
use tickloop_agent_protocol::ChatId;

impl Agent {
    pub async fn claim_acp_session(
        &self,
        chat: ChatId,
        cwd: &Path,
        root: &Path,
    ) -> Result<(), AgentError> {
        let cwd = std::fs::canonicalize(cwd).map_err(|e| AgentError::Invalid(e.to_string()))?;
        let root = std::fs::canonicalize(root).map_err(|e| AgentError::Invalid(e.to_string()))?;
        if !cwd.is_dir() {
            return Err(AgentError::Invalid(
                "ACP working directory is not a directory".into(),
            ));
        }
        let mut claims = self.inner.acp_claims.lock().await;
        if claims.contains_key(&chat) {
            return Err(AgentError::Busy);
        }
        let directory = root.join("acp-leases");
        tickloop_agent_base::private_fs::create_private_directory(&directory)
            .map_err(|e| AgentError::Invalid(e.to_string()))?;
        let path = directory.join(format!("{chat}.lock"));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)
            .map_err(|e| AgentError::Invalid(e.to_string()))?;
        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => return Err(AgentError::Busy),
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(AgentError::Storage(error.to_string()));
            }
        }
        let opened = file
            .metadata()
            .map_err(|e| AgentError::Invalid(e.to_string()))?;
        let named =
            std::fs::symlink_metadata(&path).map_err(|e| AgentError::Invalid(e.to_string()))?;
        if !opened.is_file()
            || opened.dev() != named.dev()
            || opened.ino() != named.ino()
            || opened.uid() != unsafe { libc::geteuid() }
        {
            return Err(AgentError::Invalid(
                "ACP claim file identity changed".into(),
            ));
        }
        claims.insert(chat, file);
        Ok(())
    }

    /// Call only after revocation, runtime settlement and session MCP cleanup all succeeded.
    pub async fn release_acp_session_claim(&self, chat: ChatId) {
        // Keep the pathname permanently: deleting a lock file creates an inode replacement race.
        self.inner.acp_claims.lock().await.remove(&chat);
    }
}

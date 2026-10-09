use std::{
    fs::{File, OpenOptions},
    io::{self, Read},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use serde::{Serialize, de::DeserializeOwned};
use tokio_util::sync::CancellationToken;

use crate::ModelError;
use tickloop_agent_base::private_fs::{
    OpenTarget, PRIVATE_DIRECTORY_MODE, PRIVATE_FILE_MODE, open_without_following_links,
    replace_file_atomically, verify_private_directory,
};

const FILE_LIMIT: u64 = 64 * 1024;

pub(super) struct ProviderStore {
    directory: PathBuf,
    name: &'static str,
}

impl ProviderStore {
    pub(super) fn new(root: &Path, name: &'static str) -> Self {
        Self {
            directory: root.join("auth"),
            name,
        }
    }

    fn directory(&self) -> Result<(), ModelError> {
        open_without_following_links(
            &self.directory,
            OpenTarget::Directory {
                create_mode: Some(PRIVATE_DIRECTORY_MODE),
            },
        )
        .map_err(|_| storage_error())?;
        verify_private_directory(&self.directory, "Provider credentials")
            .map_err(|_| storage_error())
    }

    pub(super) fn read<T: DeserializeOwned>(&self) -> Result<Option<T>, ModelError> {
        match open_without_following_links(
            &self.directory,
            OpenTarget::Directory { create_mode: None },
        ) {
            Ok(_) => verify_private_directory(&self.directory, "Provider credentials")
                .map_err(|_| storage_error())?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(storage_error()),
        }
        read_private_json(&self.directory.join(format!("{}.json", self.name)))
    }

    pub(super) fn save<T: Serialize>(&self, value: &T) -> Result<(), ModelError> {
        self.directory()?;
        let _: Option<serde_json::Value> = self.read()?;
        let bytes = serde_json::to_vec(value).map_err(|_| storage_error())?;
        if bytes.len() as u64 > FILE_LIMIT {
            return Err(storage_error());
        }
        replace_file_atomically(
            &self.directory.join(format!("{}.json", self.name)),
            &bytes,
            PRIVATE_FILE_MODE,
        )
        .map_err(|_| storage_error())
    }

    pub(super) async fn lock(&self, cancel: &CancellationToken) -> Result<File, ModelError> {
        self.directory()?;
        let deadline = Instant::now() + Duration::from_secs(35);
        loop {
            if cancel.is_cancelled() {
                return Err(ModelError::Cancelled);
            }
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(PRIVATE_FILE_MODE)
                .custom_flags(libc::O_NOFOLLOW)
                .open(self.directory.join(format!("{}.lock", self.name)))
                .map_err(|_| storage_error())?;
            super::verify_private_file(&file).map_err(|_| storage_error())?;
            match file.try_lock() {
                Ok(()) => return Ok(file),
                Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                    tokio::select! {
                        biased;
                        () = cancel.cancelled() => return Err(ModelError::Cancelled),
                        () = tokio::time::sleep(Duration::from_millis(50)) => {},
                    }
                }
                _ => return Err(storage_error()),
            }
        }
    }
}

pub(super) fn read_private_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, ModelError> {
    let file = match open_without_following_links(path, OpenTarget::Entry) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(storage_error()),
    };
    super::verify_private_file(&file).map_err(|_| storage_error())?;
    let mut bytes = Vec::new();
    file.take(FILE_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| storage_error())?;
    if bytes.len() as u64 > FILE_LIMIT {
        return Err(storage_error());
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|_| storage_error())
}

fn storage_error() -> ModelError {
    ModelError::Configuration("Provider credential file or directory is invalid or unsafe".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::provider_test_support::root;

    #[tokio::test]
    async fn status_reads_create_nothing_and_lock_wait_is_cancellable() {
        let root = root();
        let store = ProviderStore::new(root.path(), "fixture");
        assert!(store.read::<serde_json::Value>().unwrap().is_none());
        assert!(!root.path().join("auth").exists());
        let cancel = CancellationToken::new();
        let held = store.lock(&cancel).await.unwrap();
        let waiting = CancellationToken::new();
        let cancel_waiting = waiting.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            cancel_waiting.cancel();
        });
        assert!(matches!(
            store.lock(&waiting).await,
            Err(ModelError::Cancelled)
        ));
        drop(held);
        assert!(store.lock(&cancel).await.is_ok());
    }
}

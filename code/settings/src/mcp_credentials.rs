//! File adapter for one MCP Server's credential. Locks are shared by CLI logins and Workers.

use async_trait::async_trait;
use std::{
    fs::{File, OpenOptions},
    io::{self, Read},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::Duration,
};
use tickloop_harness::{
    base::private_fs::{PRIVATE_FILE_MODE, PrivateDirectory, current_uid, replace_file_atomically},
    connector::{ConnectorCredential, ConnectorCredentials, CredentialError, CredentialLock},
};

const LOCK_RETRY: Duration = Duration::from_millis(20);
const MAX_CREDENTIAL_BYTES: u64 = 1024 * 1024;

pub struct FileCredentials {
    directory: PrivateDirectory,
    name: String,
}

impl FileCredentials {
    pub fn new(root: &Path, source: &str, name: &str) -> Result<Self, CredentialError> {
        let (relative, name) = if let Some(plugin) = source.strip_prefix("plugin:") {
            tick_packages::validate_package_name(plugin).map_err(|_| CredentialError("located"))?;
            let name = name
                .strip_prefix(&format!("plugin_{plugin}_"))
                .ok_or(CredentialError("located"))?;
            (PathBuf::from("credentials").join(plugin).join("mcp"), name)
        } else {
            (PathBuf::from("credentials/_mcp"), name)
        };
        if name.is_empty()
            || (!source.starts_with("plugin:")
                && !name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')))
        {
            return Err(CredentialError("located"));
        }
        Ok(Self {
            directory: PrivateDirectory::anchored(root.to_owned(), relative),
            name: name
                .bytes()
                .map(|byte| {
                    if byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.') {
                        char::from(byte).to_string()
                    } else {
                        format!("%{byte:02X}")
                    }
                })
                .collect(),
        })
    }

    fn path(&self, extension: &str) -> PathBuf {
        self.directory
            .path()
            .join(format!("{}.{extension}", self.name))
    }

    fn verify_file(file: &File) -> io::Result<()> {
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.uid() != current_uid()
            || metadata.mode() & 0o077 != 0
            || metadata.nlink() != 1
        {
            return Err(io::Error::from(io::ErrorKind::PermissionDenied));
        }
        Ok(())
    }
}

#[async_trait]
impl ConnectorCredentials for FileCredentials {
    async fn lock(&self) -> Result<CredentialLock, CredentialError> {
        self.directory
            .create()
            .map_err(|_| CredentialError("locked"))?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(PRIVATE_FILE_MODE)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(self.path("lock"))
            .map_err(|_| CredentialError("locked"))?;
        Self::verify_file(&file).map_err(|_| CredentialError("locked"))?;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(CredentialLock::new(file)),
                Err(std::fs::TryLockError::WouldBlock) => tokio::time::sleep(LOCK_RETRY).await,
                Err(_) => return Err(CredentialError("locked")),
            }
        }
    }

    async fn load(&self) -> Result<Option<ConnectorCredential>, CredentialError> {
        // A missing login must not create directories merely because an Agent connects.
        if !self
            .directory
            .path()
            .try_exists()
            .map_err(|_| CredentialError("read"))?
        {
            return Ok(None);
        }
        self.directory
            .create()
            .map_err(|_| CredentialError("read"))?;
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(self.path("json"))
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(CredentialError("read")),
        };
        Self::verify_file(&file).map_err(|_| CredentialError("read"))?;
        let mut bytes = Vec::new();
        file.take(MAX_CREDENTIAL_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| CredentialError("read"))?;
        if bytes.len() as u64 > MAX_CREDENTIAL_BYTES {
            return Err(CredentialError("read"));
        }
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|_| CredentialError("read"))
    }

    async fn save(&self, credential: &ConnectorCredential) -> Result<(), CredentialError> {
        self.directory
            .create()
            .map_err(|_| CredentialError("saved"))?;
        let bytes = serde_json::to_vec(credential).map_err(|_| CredentialError("saved"))?;
        replace_file_atomically(&self.path("json"), &bytes, PRIVATE_FILE_MODE)
            .map_err(|_| CredentialError("saved"))
    }
}

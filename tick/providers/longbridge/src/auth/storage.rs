use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use longbridge::oauth::{OAuthResult, StoredToken, TokenStorage};
use serde::{Deserialize, Serialize};

use tick_provider::error::{Error, Result};

pub const TOKEN_FILE_NAME: &str = "auth.json";

#[derive(Clone)]
pub struct AuthStorage {
    pub directory: PathBuf,
}

#[derive(Deserialize, Serialize)]
pub struct SavedToken {
    pub client_id: String,
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: u64,
    pub logged_in_at: u64,
}

impl SavedToken {
    pub fn stored(&self) -> StoredToken {
        StoredToken {
            client_id: self.client_id.clone(),
            access_token: self.access_token.clone(),
            refresh_token: self.refresh_token.clone(),
            expires_at: self.expires_at,
        }
    }
}

impl AuthStorage {
    pub fn in_home(home: &Path) -> Self {
        Self {
            directory: home.join(".tickloop/longbridge"),
        }
    }

    pub fn token(&self) -> Result<Option<SavedToken>> {
        let path = self.directory.join(TOKEN_FILE_NAME);
        match fs::read(path) {
            Ok(bytes) => {
                let token: SavedToken = serde_json::from_slice(&bytes)
                    .map_err(|_| Error::auth("invalid local token file"))?;
                if token.client_id.is_empty() || token.access_token.is_empty() {
                    return Err(Error::auth("invalid local token file"));
                }
                Ok(Some(token))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub fn logout(&self) -> Result<()> {
        match fs::remove_file(self.directory.join(TOKEN_FILE_NAME)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

pub fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    fs::File::open(parent)?.sync_all()
}

impl TokenStorage for AuthStorage {
    fn load(&self, client_id: &str) -> Option<StoredToken> {
        self.token()
            .ok()
            .flatten()
            .filter(|token| token.client_id == client_id)
            .map(|token| token.stored())
    }

    fn save(&self, token: &StoredToken) -> OAuthResult<()> {
        let logged_in_at = self
            .token()
            .ok()
            .flatten()
            .filter(|previous| previous.client_id == token.client_id)
            .map_or_else(now_seconds, |previous| previous.logged_in_at);
        let saved = SavedToken {
            client_id: token.client_id.clone(),
            access_token: token.access_token.clone(),
            refresh_token: token.refresh_token.clone(),
            expires_at: token.expires_at,
            logged_in_at,
        };
        let bytes = serde_json::to_vec_pretty(&saved)
            .map_err(|source| longbridge::oauth::OAuthError::SerializeToken { source })?;
        let path = self.directory.join(TOKEN_FILE_NAME);
        atomic_write(&path, &bytes)
            .map_err(|source| longbridge::oauth::OAuthError::TokenFileWrite { path, source })
    }
}

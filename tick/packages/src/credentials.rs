//! The credentials Tickloop keeps for each package in `<config root>/credentials/<package>/`,
//! and the login state they give. Tickloop picks every path from the package name; a manifest
//! only says which kind of login the package needs.

use std::{
    collections::BTreeMap,
    fmt,
    fs::{self, DirBuilder, File, OpenOptions, Permissions},
    io::{self, Write},
    os::unix::{
        ffi::OsStrExt,
        fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    PRIVATE_DIRECTORY_MODE, PRIVATE_FILE_MODE, PackageAuth, PackageError, validate_package_name,
};

/// The directory under the config root that holds every package's credentials.
const CREDENTIALS_DIRECTORY: &str = "credentials";
/// The token of an `oauth2` package.
const OAUTH_TOKEN_FILE_NAME: &str = "oauth2.json";
/// Held exclusively by whoever changes the token, Tickloop or a program refreshing it.
const OAUTH_LOCK_FILE_NAME: &str = "oauth2.lock";
/// The key of an `api_key` package.
const API_KEY_FILE_NAME: &str = "api_key";
/// Names the token file of an `oauth2` package to a program Tickloop starts.
pub const OAUTH_TOKEN_FILE_VARIABLE: &str = "TICKLOOP_OAUTH_TOKEN_FILE";
/// Moves the config root away from `~/.tickloop` when set and not empty.
const CONFIG_ROOT_VARIABLE: &str = "TICKLOOP_CONFIG_DIR";
const DEFAULT_CONFIG_ROOT: &str = ".tickloop";
/// Bytes of the config root digest that name its runtime directories, short enough for the Unix
/// socket paths beneath them.
const RUNTIME_KEY_BYTES: usize = 8;
/// The `client_id` registered with each issuer, reused by the next login.
const OAUTH_CLIENTS_FILE_NAME: &str = "oauth2-clients.json";
/// Files are replaced through a temporary file with this prefix beside them.
const TEMPORARY_PREFIX: &str = ".replacing-";
/// Far longer than any real key, and short enough to keep out of reach of misuse.
const MAX_API_KEY_LENGTH: usize = 4096;

/// Serialized as `logged_in`, `logged_out`, `expired`, `not_required` or `unknown`, the names the
/// Agent sees.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LoginState {
    LoggedIn,
    LoggedOut,
    /// The access token expired and there is no refresh token to renew it.
    Expired,
    /// The package needs no login.
    NotRequired,
    /// The saved credentials cannot be read.
    Unknown,
}

/// What `oauth2.json` holds: the fields of an RFC 6749 token response, with the server that
/// issued it and when the access token expires. A program refreshes it at `token_endpoint` as
/// `client_id` and saves it back under the lock.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthToken {
    pub access_token: String,
    pub token_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    pub issuer: String,
    pub client_id: String,
    pub token_endpoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revocation_endpoint: Option<String>,
    /// Seconds since the Unix epoch.
    pub expires_at: u64,
}

impl OAuthToken {
    /// The refresh token, when the server gave a usable one.
    pub fn refresh_token(&self) -> Option<&str> {
        self.refresh_token
            .as_deref()
            .filter(|token| !token.is_empty())
    }

    fn login_state(&self) -> LoginState {
        if self.expires_at > now_seconds() || self.refresh_token().is_some() {
            LoginState::LoggedIn
        } else {
            LoginState::Expired
        }
    }
}

/// Shows where the token came from, never the tokens themselves.
impl fmt::Debug for OAuthToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OAuthToken")
            .field("token_type", &self.token_type)
            .field("has_refresh_token", &self.refresh_token().is_some())
            .field("scope", &self.scope)
            .field("issuer", &self.issuer)
            .field("client_id", &self.client_id)
            .field("token_endpoint", &self.token_endpoint)
            .field("revocation_endpoint", &self.revocation_endpoint)
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

/// A key the user entered for an `api_key` package. Its `Debug` never shows it.
#[derive(Clone, PartialEq, Eq)]
pub struct ApiKey(String);

impl ApiKey {
    /// `entered` without surrounding white space, when that is a key: 1 to 4096 bytes without
    /// control characters.
    pub fn new(entered: &str) -> Result<Self, PackageError> {
        let key = entered.trim();
        if key.is_empty() || key.len() > MAX_API_KEY_LENGTH {
            return Err(PackageError::Invalid(format!(
                "a key holds 1 to {MAX_API_KEY_LENGTH} bytes"
            )));
        }
        if key.chars().any(char::is_control) {
            return Err(PackageError::Invalid(
                "a key must not hold control characters".to_owned(),
            ));
        }
        Ok(Self(key.to_owned()))
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ApiKey(..)")
    }
}

/// Where Tickloop keeps one package's credentials: `<config root>/credentials/<package>/`, owner
/// only. Every file in it is replaced at once through a temporary file beside it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageCredentials {
    directory: PathBuf,
    /// Whether the directory is the one Tickloop picks under a config root, which it keeps
    /// owner-only even when it was made before. A directory a program is pointed at is made
    /// owner-only only when it is created here.
    is_chosen_by_tickloop: bool,
}

impl PackageCredentials {
    pub fn in_config_root(config_root: &Path, name: &str) -> Result<Self, PackageError> {
        validate_package_name(name)?;
        Ok(Self {
            directory: config_root.join(CREDENTIALS_DIRECTORY).join(name),
            is_chosen_by_tickloop: true,
        })
    }

    /// The credentials around the OAuth token file at `token_file`, an absolute path to a file
    /// named `oauth2.json`, as a program is pointed at it: the lock and the other
    /// files are beside it.
    pub fn with_token_file(token_file: &Path) -> Result<Self, PackageError> {
        match token_file.parent() {
            Some(directory)
                if token_file.is_absolute()
                    && token_file.file_name() == Some(OAUTH_TOKEN_FILE_NAME.as_ref()) =>
            {
                Ok(Self {
                    directory: directory.to_path_buf(),
                    is_chosen_by_tickloop: false,
                })
            }
            _ => Err(PackageError::Invalid(format!(
                "{OAUTH_TOKEN_FILE_VARIABLE} must be the absolute path of an {OAUTH_TOKEN_FILE_NAME} file"
            ))),
        }
    }

    /// The credentials a program of the `oauth2` package `name` uses: those around the token file
    /// [`OAUTH_TOKEN_FILE_VARIABLE`] names when Tickloop started the program, else the package's
    /// own in the [`default_config_root`], so a program run on its own shares Tickloop's login.
    pub fn for_program(name: &str) -> Result<Self, PackageError> {
        match std::env::var_os(OAUTH_TOKEN_FILE_VARIABLE) {
            Some(token_file) => Self::with_token_file(Path::new(&token_file)),
            None => {
                let config_root = default_config_root().ok_or_else(|| {
                    PackageError::Invalid("cannot find the home directory".to_owned())
                })?;
                Self::in_config_root(&config_root, name)
            }
        }
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// The variables a program of a package that authenticates by `auth` starts with: an
    /// `oauth2` program finds its token file in [`OAUTH_TOKEN_FILE_VARIABLE`], whether or not
    /// the user logged in, and an `api_key` program its key in the variable the manifest names,
    /// once the user entered one. The values must never be shown or logged.
    pub fn launch_environment(&self, auth: &PackageAuth) -> Vec<(String, String)> {
        match auth {
            PackageAuth::None => Vec::new(),
            PackageAuth::ApiKey { env, .. } => match self.api_key() {
                Ok(Some(key)) => vec![(env.clone(), key.expose().to_owned())],
                // A program without a usable key starts all the same and reports the failure.
                Ok(None) | Err(_) => Vec::new(),
            },
            PackageAuth::OAuth2 { .. } => {
                let token_file = self.oauth_token_path();
                let token_file = std::path::absolute(&token_file).unwrap_or(token_file);
                vec![(
                    OAUTH_TOKEN_FILE_VARIABLE.to_owned(),
                    token_file.to_string_lossy().into_owned(),
                )]
            }
        }
    }

    pub fn oauth_token_path(&self) -> PathBuf {
        self.directory.join(OAUTH_TOKEN_FILE_NAME)
    }

    pub fn api_key_path(&self) -> PathBuf {
        self.directory.join(API_KEY_FILE_NAME)
    }

    /// The login state the saved credentials give a package that authenticates by `auth`,
    /// read from the files alone.
    pub fn login_state(&self, auth: &PackageAuth) -> LoginState {
        match auth {
            PackageAuth::None => LoginState::NotRequired,
            PackageAuth::ApiKey { .. } => match self.api_key() {
                Ok(Some(_)) => LoginState::LoggedIn,
                Ok(None) => LoginState::LoggedOut,
                Err(_) => LoginState::Unknown,
            },
            PackageAuth::OAuth2 { .. } => match self.oauth_token() {
                Ok(Some(token)) => token.login_state(),
                Ok(None) => LoginState::LoggedOut,
                Err(_) => LoginState::Unknown,
            },
        }
    }

    /// The saved token; `None` when there is none. A token is replaced at once, so reading it
    /// needs no lock.
    pub fn oauth_token(&self) -> Result<Option<OAuthToken>, PackageError> {
        let path = self.oauth_token_path();
        let Some(bytes) = read_if_present(&path)? else {
            return Ok(None);
        };
        let token: OAuthToken = serde_json::from_slice(&bytes).map_err(|error| {
            PackageError::Invalid(format!("{} is not a valid token: {error}", path.display()))
        })?;
        if token.access_token.is_empty() || token.client_id.is_empty() {
            return Err(PackageError::Invalid(format!(
                "{} holds no access token or client",
                path.display()
            )));
        }
        Ok(Some(token))
    }

    /// Takes the lock every change of the token or the registered clients holds, creating the
    /// directory when needed; it waits while another process holds it.
    pub fn lock_oauth(&self) -> Result<LockedOAuth, PackageError> {
        self.create_directory()?;
        let path = self.directory.join(OAUTH_LOCK_FILE_NAME);
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(PRIVATE_FILE_MODE)
            .open(&path)
            .map_err(PackageError::io(format!("cannot open {}", path.display())))?;
        lock.lock()
            .map_err(PackageError::io(format!("cannot lock {}", path.display())))?;
        Ok(LockedOAuth {
            credentials: self.clone(),
            _lock: lock,
        })
    }

    /// The saved key; `None` when there is none.
    pub fn api_key(&self) -> Result<Option<ApiKey>, PackageError> {
        let path = self.api_key_path();
        let Some(bytes) = read_if_present(&path)? else {
            return Ok(None);
        };
        let invalid = || PackageError::Invalid(format!("{} holds no valid key", path.display()));
        let text = String::from_utf8(bytes).map_err(|_| invalid())?;
        ApiKey::new(&text).map(Some).map_err(|_| invalid())
    }

    pub fn save_api_key(&self, key: &ApiKey) -> Result<(), PackageError> {
        self.create_directory()?;
        replace_file(&self.api_key_path(), key.expose().as_bytes())
    }

    /// Deletes the saved key; returns whether there was one.
    pub fn remove_api_key(&self) -> Result<bool, PackageError> {
        remove_if_present(&self.api_key_path())
    }

    fn create_directory(&self) -> Result<(), PackageError> {
        let failed = || PackageError::io(format!("cannot create {}", self.directory.display()));
        if !self.is_chosen_by_tickloop && self.directory.is_dir() {
            // Someone else's directory, such as the home directory, keeps its permissions.
            return Ok(());
        }
        DirBuilder::new()
            .recursive(true)
            .mode(PRIVATE_DIRECTORY_MODE)
            .create(&self.directory)
            .map_err(failed())?;
        // A directory made before, by hand or under another umask, becomes owner-only too.
        fs::set_permissions(
            &self.directory,
            Permissions::from_mode(PRIVATE_DIRECTORY_MODE),
        )
        .map_err(failed())
    }
}

/// A package's OAuth files while this process holds their lock, which dropping it releases.
#[derive(Debug)]
pub struct LockedOAuth {
    credentials: PackageCredentials,
    _lock: File,
}

/// The client registered with each issuer.
type RegisteredClients = BTreeMap<String, RegisteredClient>;

#[derive(Deserialize, Serialize)]
struct RegisteredClient {
    client_id: String,
}

impl LockedOAuth {
    pub fn token(&self) -> Result<Option<OAuthToken>, PackageError> {
        self.credentials.oauth_token()
    }

    pub fn save_token(&self, token: &OAuthToken) -> Result<(), PackageError> {
        let mut contents = serde_json::to_vec_pretty(token).expect("a token serializes");
        contents.push(b'\n');
        replace_file(&self.credentials.oauth_token_path(), &contents)
    }

    /// Deletes the saved token; returns whether there was one.
    pub fn remove_token(&self) -> Result<bool, PackageError> {
        remove_if_present(&self.credentials.oauth_token_path())
    }

    /// The `client_id` registered with `issuer` before.
    pub fn client_id(&self, issuer: &str) -> Result<Option<String>, PackageError> {
        Ok(self
            .clients()?
            .remove(issuer)
            .map(|client| client.client_id))
    }

    /// Records the `client_id` registered with `issuer`; `None` forgets it, so the next login
    /// registers again.
    pub fn set_client_id(&self, issuer: &str, client_id: Option<&str>) -> Result<(), PackageError> {
        let mut clients = self.clients()?;
        match client_id {
            Some(client_id) => clients.insert(
                issuer.to_owned(),
                RegisteredClient {
                    client_id: client_id.to_owned(),
                },
            ),
            None => clients.remove(issuer),
        };
        let mut contents = serde_json::to_vec_pretty(&clients).expect("the clients serialize");
        contents.push(b'\n');
        replace_file(&self.clients_path(), &contents)
    }

    /// The registered clients; ones that cannot be read are registered again.
    fn clients(&self) -> Result<RegisteredClients, PackageError> {
        Ok(read_if_present(&self.clients_path())?
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default())
    }

    fn clients_path(&self) -> PathBuf {
        self.credentials.directory.join(OAUTH_CLIENTS_FILE_NAME)
    }
}

/// The config root Tickloop uses: `TICKLOOP_CONFIG_DIR` when it is set and not empty, else
/// `.tickloop` in the home directory; `None` when there is neither.
pub fn default_config_root() -> Option<PathBuf> {
    std::env::var_os(CONFIG_ROOT_VARIABLE)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::home_dir()
                .filter(|home| !home.as_os_str().is_empty())
                .map(|home| home.join(DEFAULT_CONFIG_ROOT))
        })
}

/// What sets the runtime of `config_root` apart from the user's others: `None` for
/// `~/.tickloop`, whose daemon and Tickhost keep the per-user runtime directories, and for any
/// other root the start of the SHA-256 of its canonical path. Every runtime directory derives
/// from it, so one config root has exactly one daemon and one Tickhost.
pub fn config_root_runtime_key(config_root: &Path) -> Option<String> {
    let root = resolved_path(config_root);
    let home_root = std::env::home_dir()
        .filter(|home| !home.as_os_str().is_empty())
        .map(|home| resolved_path(&home.join(DEFAULT_CONFIG_ROOT)));
    if home_root.as_ref() == Some(&root) {
        return None;
    }
    let digest = Sha256::digest(root.as_os_str().as_bytes());
    Some(
        digest[..RUNTIME_KEY_BYTES]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
    )
}

/// `path` with every symbolic link resolved, also before it exists: its deepest existing ancestor
/// is resolved and the rest appended, so a config root names the same runtime before and after
/// it is created.
fn resolved_path(path: &Path) -> PathBuf {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    absolute
        .ancestors()
        .find_map(|ancestor| {
            let canonical = ancestor.canonicalize().ok()?;
            let rest = absolute.strip_prefix(ancestor).ok()?;
            // Joining an empty rest would add a trailing separator.
            Some(if rest.as_os_str().is_empty() {
                canonical
            } else {
                canonical.join(rest)
            })
        })
        .unwrap_or(absolute)
}

pub(crate) fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn read_if_present(path: &Path) -> Result<Option<Vec<u8>>, PackageError> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(PackageError::io(format!("cannot read {}", path.display()))(
            error,
        )),
    }
}

fn remove_if_present(path: &Path) -> Result<bool, PackageError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(PackageError::io(format!(
            "cannot delete {}",
            path.display()
        ))(error)),
    }
}

/// Replaces the file at `path` with `contents` at once: an owner-only temporary file beside it
/// is written, flushed and renamed into its place.
fn replace_file(path: &Path, contents: &[u8]) -> Result<(), PackageError> {
    let directory = path
        .parent()
        .expect("a credentials file lives in a directory");
    let failed = || PackageError::io(format!("cannot write {}", path.display()));
    let mut temporary = tempfile::Builder::new()
        .prefix(TEMPORARY_PREFIX)
        .permissions(Permissions::from_mode(PRIVATE_FILE_MODE))
        .tempfile_in(directory)
        .map_err(failed())?;
    temporary.write_all(contents).map_err(failed())?;
    temporary.as_file().sync_all().map_err(failed())?;
    temporary
        .persist(path)
        .map_err(|error| failed()(error.error))?;
    File::open(directory)
        .and_then(|directory| directory.sync_all())
        .map_err(failed())
}

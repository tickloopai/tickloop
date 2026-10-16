use std::{
    fs, io,
    os::unix::fs::PermissionsExt,
    path::{Component, Path, PathBuf},
};

use serde::Deserialize;
use url::{Host, Url};

use crate::{PackageError, printable};

/// The manifest at the root of every Tick package.
pub const MANIFEST_FILE_NAME: &str = "tick-package.json";
/// A package's program is named after the package, so installed programs never share a name.
const EXECUTABLE_PREFIX: &str = "tick-";
const MAX_NAME_LENGTH: usize = 64;
const MAX_VERSION_LENGTH: usize = 64;
const MAX_TEXT_LENGTH: usize = 500;
const ANY_EXECUTE_PERMISSION: u32 = 0o111;
/// Variables a key must not replace, since they change how programs or the system run.
const RESERVED_VARIABLES: [&str; 5] = ["PATH", "HOME", "SHELL", "USER", "TMPDIR"];
const RESERVED_VARIABLE_PREFIXES: [&str; 3] = ["LD_", "DYLD_", "TICKLOOP_"];

/// What a package's `tick-package.json` declares.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageManifest {
    pub name: String,
    pub version: String,
    pub description: String,
    /// The program, relative to the package root and named `tick-<name>`.
    pub executable: PathBuf,
    /// Whether the program speaks the Tick protocol.
    pub protocol: bool,
    /// How the user logs the package in to its data source.
    pub auth: PackageAuth,
}

/// How a package authenticates to its data source. Tickloop logs it in and keeps the
/// credentials; the package's programs get them in their environment when they start.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PackageAuth {
    /// The package needs no login, also when the manifest declares no `auth`.
    #[default]
    None,
    /// A key the user enters, which the programs find in the environment variable `env`.
    ApiKey { env: String, label: String },
    /// OAuth 2.0 device authorization with one of the `issuers`, chosen by the user.
    #[serde(rename = "oauth2")]
    OAuth2 {
        issuers: Vec<OAuthIssuer>,
        /// The scopes to request; none asks for the server's default.
        #[serde(default)]
        scopes: Vec<String>,
    },
}

/// An OAuth 2.0 authorization server a package logs in with.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct OAuthIssuer {
    pub label: String,
    /// The server's issuer identifier (RFC 8414): an https URL without query or fragment, whose
    /// metadata names its endpoints.
    pub issuer: String,
}

#[derive(Deserialize)]
struct ManifestFile {
    name: String,
    version: String,
    description: String,
    executable: String,
    protocol: bool,
    #[serde(default)]
    auth: Option<PackageAuth>,
}

impl PackageManifest {
    /// Parses a manifest and checks its fields, without looking at the files they name.
    pub fn parse(bytes: &[u8]) -> Result<Self, PackageError> {
        let file: ManifestFile = serde_json::from_slice(bytes).map_err(|error| {
            PackageError::Invalid(format!("{MANIFEST_FILE_NAME} is not valid: {error}"))
        })?;
        validate_package_name(&file.name)?;
        if !is_valid_version(&file.version) {
            return Err(PackageError::Invalid(format!(
                "version {:?} in {MANIFEST_FILE_NAME} may hold only letters, digits, '.', '+' and '-'",
                file.version
            )));
        }
        check_text("description", &file.description)?;
        let auth = file.auth.unwrap_or_default();
        check_auth(&auth)?;
        let executable = Path::new(&file.executable);
        let is_inside = !file.executable.is_empty()
            && executable
                .components()
                .all(|component| matches!(component, Component::Normal(_)));
        if !is_inside {
            return Err(PackageError::Invalid(format!(
                "executable {:?} in {MANIFEST_FILE_NAME} must be a relative path inside the package",
                file.executable
            )));
        }
        let expected_file_name = format!("{EXECUTABLE_PREFIX}{}", file.name);
        if executable.file_name() != Some(expected_file_name.as_ref()) {
            return Err(PackageError::Invalid(format!(
                "executable {:?} in {MANIFEST_FILE_NAME} must be named {expected_file_name}",
                file.executable
            )));
        }
        Ok(Self {
            executable: executable.to_path_buf(),
            name: file.name,
            version: file.version,
            description: file.description,
            protocol: file.protocol,
            auth,
        })
    }

    /// Reads the manifest of the package in `root` and checks that its executable is a file
    /// inside the package that can be run.
    pub fn load(root: &Path) -> Result<Self, PackageError> {
        let root = root
            .canonicalize()
            .map_err(PackageError::io(format!("cannot open {}", root.display())))?;
        let path = root.join(MANIFEST_FILE_NAME);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(PackageError::Invalid(format!(
                    "the package has no {MANIFEST_FILE_NAME} at its root"
                )));
            }
            Err(error) => {
                return Err(PackageError::io(format!("cannot read {}", path.display()))(
                    error,
                ));
            }
        };
        let manifest = Self::parse(&bytes)?;
        let shown = printable(&manifest.executable);
        let executable = match root.join(&manifest.executable).canonicalize() {
            Ok(executable) => executable,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(PackageError::Invalid(format!(
                    "executable {shown} does not exist in the package"
                )));
            }
            Err(error) => {
                return Err(PackageError::io(format!("cannot open executable {shown}"))(
                    error,
                ));
            }
        };
        if !executable.starts_with(&root) {
            return Err(PackageError::Invalid(format!(
                "executable {shown} leads outside the package"
            )));
        }
        let metadata = fs::metadata(&executable)
            .map_err(PackageError::io(format!("cannot open executable {shown}")))?;
        if !metadata.is_file() {
            return Err(PackageError::Invalid(format!(
                "executable {shown} is not a file"
            )));
        }
        if metadata.permissions().mode() & ANY_EXECUTE_PERMISSION == 0 {
            return Err(PackageError::Invalid(format!(
                "executable {shown} is not executable"
            )));
        }
        Ok(manifest)
    }
}

fn check_auth(auth: &PackageAuth) -> Result<(), PackageError> {
    match auth {
        PackageAuth::None => Ok(()),
        PackageAuth::ApiKey { env, label } => {
            check_text("auth.label", label)?;
            if is_allowed_variable(env) {
                Ok(())
            } else {
                Err(PackageError::Invalid(format!(
                    "auth.env {env:?} in {MANIFEST_FILE_NAME} must match [A-Z][A-Z0-9_]* and must not be \
                     {}, or start with {}",
                    RESERVED_VARIABLES.join(", "),
                    RESERVED_VARIABLE_PREFIXES.join(", ")
                )))
            }
        }
        PackageAuth::OAuth2 { issuers, scopes } => {
            if issuers.is_empty() {
                return Err(PackageError::Invalid(format!(
                    "auth.issuers in {MANIFEST_FILE_NAME} must list at least one issuer"
                )));
            }
            for (index, OAuthIssuer { label, issuer }) in issuers.iter().enumerate() {
                check_text("each auth.issuers label", label)?;
                check_text("each auth.issuers issuer", issuer)?;
                if let Some(problem) = issuer_problem(issuer) {
                    return Err(PackageError::Invalid(format!(
                        "issuer {issuer} in {MANIFEST_FILE_NAME} {problem}"
                    )));
                }
                let earlier = &issuers[..index];
                if earlier.iter().any(|other| other.label == *label) {
                    return Err(PackageError::Invalid(format!(
                        "auth.issuers in {MANIFEST_FILE_NAME} names the label {label} twice"
                    )));
                }
                if earlier.iter().any(|other| other.issuer == *issuer) {
                    return Err(PackageError::Invalid(format!(
                        "auth.issuers in {MANIFEST_FILE_NAME} lists {issuer} twice"
                    )));
                }
            }
            match scopes.iter().find(|scope| !is_scope_token(scope)) {
                Some(scope) => Err(PackageError::Invalid(format!(
                    "scope {scope:?} in {MANIFEST_FILE_NAME} must be printable ASCII without spaces, '\"' or '\\'"
                ))),
                None => Ok(()),
            }
        }
    }
}

/// Whether a package may name `name` for its key: an upper-case name that cannot change how
/// its program or the system runs.
fn is_allowed_variable(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes.next().is_some_and(|first| first.is_ascii_uppercase())
        && bytes.all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
        && !RESERVED_VARIABLES.contains(&name)
        && !RESERVED_VARIABLE_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix))
}

/// RFC 6749's scope-token: printable ASCII but space, '"' and '\'.
fn is_scope_token(scope: &str) -> bool {
    !scope.is_empty()
        && scope
            .bytes()
            .all(|byte| (0x21..=0x7e).contains(&byte) && byte != b'"' && byte != b'\\')
}

/// Why `issuer` cannot identify an authorization server, if it can: it must be an https URL, or
/// an http one on this machine, with no credentials, query or fragment.
fn issuer_problem(issuer: &str) -> Option<&'static str> {
    let Ok(url) = Url::parse(issuer) else {
        return Some("is not a valid URL");
    };
    let is_on_this_machine = match url.host() {
        Some(Host::Domain(domain)) => domain == "localhost",
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    };
    if !(url.scheme() == "https" || url.scheme() == "http" && is_on_this_machine) {
        Some("must use https, or http on this machine")
    } else if !url.username().is_empty() || url.password().is_some() {
        Some("must not hold credentials")
    } else if url.query().is_some() || url.fragment().is_some() {
        Some("must not have a query or fragment")
    } else {
        None
    }
}

/// A package name holds lowercase letters, digits and `-`, and does not start with `-`.
pub fn validate_package_name(name: &str) -> Result<(), PackageError> {
    let is_valid = !name.is_empty()
        && name.len() <= MAX_NAME_LENGTH
        && !name.starts_with('-')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    if is_valid {
        Ok(())
    } else {
        Err(PackageError::Invalid(format!(
            "{name:?} is not a Tick package name: use lowercase letters, digits and '-', starting with a letter or digit"
        )))
    }
}

/// A version holds letters, digits, '.', '+' and '-', such as `0.2.0-rc.1+build.5`.
pub(crate) fn is_valid_version(version: &str) -> bool {
    !version.is_empty()
        && version.len() <= MAX_VERSION_LENGTH
        && version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b".+-".contains(&byte))
}

fn check_text(field: &str, text: &str) -> Result<(), PackageError> {
    match text_problem(text) {
        Some(problem) => Err(PackageError::Invalid(format!(
            "{field} in {MANIFEST_FILE_NAME} {problem}"
        ))),
        None => Ok(()),
    }
}

/// What keeps `text` from being shown on one line of a terminal, if anything.
pub(crate) fn text_problem(text: &str) -> Option<String> {
    if text.trim().is_empty() || text.chars().count() > MAX_TEXT_LENGTH {
        return Some(format!("must hold 1 to {MAX_TEXT_LENGTH} characters"));
    }
    if text.chars().any(char::is_control) {
        return Some("must not hold control characters".to_owned());
    }
    None
}

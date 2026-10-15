use std::{
    fs, io,
    os::unix::fs::PermissionsExt,
    path::{Component, Path, PathBuf},
};

use serde::Deserialize;

use crate::{PackageError, printable};

/// The manifest at the root of every Tick package.
pub const MANIFEST_FILE_NAME: &str = "tick-package.json";
/// A package's program is named after the package, so installed programs never share a name.
const EXECUTABLE_PREFIX: &str = "tick-";
const MAX_NAME_LENGTH: usize = 64;
const MAX_VERSION_LENGTH: usize = 64;
const MAX_TEXT_LENGTH: usize = 500;
const ANY_EXECUTE_PERMISSION: u32 = 0o111;

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
    /// Where the program keeps its data source credentials, as shown to the user, such as
    /// `~/.tickloop/longbridge/`; uninstalling the package keeps them.
    pub credentials: Option<String>,
}

#[derive(Deserialize)]
struct ManifestFile {
    name: String,
    version: String,
    description: String,
    executable: String,
    protocol: bool,
    #[serde(default)]
    credentials: Option<String>,
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
        if let Some(credentials) = &file.credentials {
            check_text("credentials", credentials)?;
        }
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
            credentials: file.credentials,
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

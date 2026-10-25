use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path};

use crate::private_fs::{
    OpenTarget, PRIVATE_DIRECTORY_MODE, PRIVATE_FILE_MODE, current_uid,
    open_without_following_links, replace_file_atomically, verify_private_directory,
};
use tickloop_harness_protocol::ApiKeyStatus;

const ENV_FILE: &str = ".env";
const LOCK_FILE: &str = ".env.lock";
const MAX_ENV_FILE_BYTES: usize = 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum CredentialError {
    #[error(
        "API key environment variable name must use letters, digits and underscores, starting with a letter or underscore"
    )]
    InvalidName,
    #[error("API key must be nonempty and usable as an HTTP header")]
    InvalidValue,
    #[error("Agent credential directory is unavailable or unsafe")]
    UnsafeDirectory,
    #[error("Agent .env file is unavailable or unsafe")]
    UnsafeFile,
    #[error("Agent .env file is too large or is not UTF-8")]
    InvalidFile,
    #[error("Agent .env entry for {0} is malformed or duplicated")]
    InvalidEntry(String),
    #[error("could not save Agent .env file")]
    Save,
}

/// Reports the active credential source without returning the credential.
/// A saved `.env` value takes precedence over an inherited environment value.
pub fn api_key_status(root: &Path, name: &str) -> Result<ApiKeyStatus, CredentialError> {
    validate_name(name)?;
    if load_api_key(root, name)?.is_some() {
        return Ok(ApiKeyStatus::Saved);
    }
    match std::env::var(name) {
        Ok(value) if validate_value(&value).is_ok() => Ok(ApiKeyStatus::Environment),
        _ => Ok(ApiKeyStatus::Missing),
    }
}

/// Reads only the saved value. Callers can fall back to the inherited environment.
pub fn load_api_key(root: &Path, name: &str) -> Result<Option<String>, CredentialError> {
    validate_name(name)?;
    let root = private_root(root, false)?;
    let Some(root) = root else { return Ok(None) };
    let contents = read_env_file(&root)?;
    find_entry(&contents, name).map(|entry| entry.map(|(_, value)| value))
}

pub fn save_api_key(root: &Path, name: &str, value: &str) -> Result<(), CredentialError> {
    validate_name(name)?;
    validate_value(value)?;
    let root = private_root(root, true)?.ok_or(CredentialError::UnsafeDirectory)?;
    let _lock = lock_env_file(&root)?;

    let contents = read_env_file(&root)?;
    let previous = find_entry(&contents, name)?;
    let assignment = format!("{name}=\"{}\"", escape_value(value));
    let updated = if let Some((line_index, _)) = previous {
        let mut lines: Vec<&str> = contents.split_inclusive('\n').collect();
        let old = lines.remove(line_index);
        let mut updated = String::with_capacity(contents.len() + assignment.len());
        for line in lines.iter().take(line_index) {
            updated.push_str(line);
        }
        updated.push_str(&assignment);
        if old.ends_with('\n') {
            updated.push('\n');
        }
        for line in lines.iter().skip(line_index) {
            updated.push_str(line);
        }
        updated
    } else {
        let mut updated = contents;
        if !updated.is_empty() && !updated.ends_with('\n') {
            updated.push('\n');
        }
        updated.push_str(&assignment);
        updated.push('\n');
        updated
    };
    if updated.len() > MAX_ENV_FILE_BYTES {
        return Err(CredentialError::InvalidFile);
    }
    replace_file_atomically(&root.join(ENV_FILE), updated.as_bytes(), PRIVATE_FILE_MODE)
        .map_err(|_| CredentialError::Save)
}

/// Removes only the saved assignment. An inherited environment value is unchanged.
pub fn remove_api_key(root: &Path, name: &str) -> Result<(), CredentialError> {
    validate_name(name)?;
    let Some(root) = private_root(root, false)? else {
        return Ok(());
    };
    match open_without_following_links(&root.join(ENV_FILE), OpenTarget::Entry) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(CredentialError::UnsafeFile),
    }
    let root = private_root(&root, true)?.ok_or(CredentialError::UnsafeDirectory)?;
    let _lock = lock_env_file(&root)?;
    let contents = read_env_file(&root)?;
    let Some((line_index, _)) = find_entry(&contents, name)? else {
        return Ok(());
    };
    let mut lines: Vec<&str> = contents.split_inclusive('\n').collect();
    lines.remove(line_index);
    let updated = lines.concat();
    replace_file_atomically(&root.join(ENV_FILE), updated.as_bytes(), PRIVATE_FILE_MODE)
        .map_err(|_| CredentialError::Save)
}

fn lock_env_file(root: &Path) -> Result<File, CredentialError> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create(true)
        .mode(PRIVATE_FILE_MODE);
    options.custom_flags(libc::O_NOFOLLOW);
    let lock = options
        .open(root.join(LOCK_FILE))
        .map_err(|_| CredentialError::UnsafeFile)?;
    verify_private_file(&lock)?;
    lock.lock().map_err(|_| CredentialError::UnsafeFile)?;
    Ok(lock)
}

fn validate_name(name: &str) -> Result<(), CredentialError> {
    let mut bytes = name.bytes();
    if !bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        || !bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err(CredentialError::InvalidName);
    }
    Ok(())
}

fn validate_value(value: &str) -> Result<(), CredentialError> {
    if value.trim().is_empty() || reqwest::header::HeaderValue::from_str(value).is_err() {
        return Err(CredentialError::InvalidValue);
    }
    Ok(())
}

fn private_root(root: &Path, create: bool) -> Result<Option<std::path::PathBuf>, CredentialError> {
    if !root.is_absolute()
        || root
            .components()
            .any(|component| !matches!(component, Component::RootDir | Component::Normal(_)))
    {
        return Err(CredentialError::UnsafeDirectory);
    }
    let mode = if create {
        Some(PRIVATE_DIRECTORY_MODE)
    } else {
        None
    };
    match open_without_following_links(root, OpenTarget::Directory { create_mode: mode }) {
        Ok(directory) if create => {
            let metadata = directory
                .metadata()
                .map_err(|_| CredentialError::UnsafeDirectory)?;
            if !metadata.is_dir() || metadata.uid() != current_uid() {
                return Err(CredentialError::UnsafeDirectory);
            }
            if metadata.mode() & 0o077 != 0 {
                directory
                    .set_permissions(std::fs::Permissions::from_mode(PRIVATE_DIRECTORY_MODE))
                    .map_err(|_| CredentialError::UnsafeDirectory)?;
            }
        }
        Ok(_) => {}
        Err(error) if !create && error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(CredentialError::UnsafeDirectory),
    }
    if create {
        verify_private_directory(root, "Agent credential directory")
            .map_err(|_| CredentialError::UnsafeDirectory)?;
    }
    Ok(Some(root.to_path_buf()))
}

fn verify_private_file(file: &File) -> Result<(), CredentialError> {
    let metadata = file.metadata().map_err(|_| CredentialError::UnsafeFile)?;
    if !metadata.is_file() || metadata.uid() != current_uid() || metadata.mode() & 0o077 != 0 {
        return Err(CredentialError::UnsafeFile);
    }
    Ok(())
}

fn read_env_file(root: &Path) -> Result<String, CredentialError> {
    let file = match open_without_following_links(&root.join(ENV_FILE), OpenTarget::Entry) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(String::new()),
        Err(_) => return Err(CredentialError::UnsafeFile),
    };
    verify_private_directory(root, "Agent credential directory")
        .map_err(|_| CredentialError::UnsafeDirectory)?;
    verify_private_file(&file)?;
    let mut bytes = Vec::new();
    file.take(MAX_ENV_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| CredentialError::InvalidFile)?;
    if bytes.len() > MAX_ENV_FILE_BYTES {
        return Err(CredentialError::InvalidFile);
    }
    String::from_utf8(bytes).map_err(|_| CredentialError::InvalidFile)
}

fn find_entry(contents: &str, name: &str) -> Result<Option<(usize, String)>, CredentialError> {
    let mut found = None;
    for (index, line) in contents.lines().enumerate() {
        let Some(raw_value) = assignment_value(line, name) else {
            continue;
        };
        if found.is_some() {
            return Err(CredentialError::InvalidEntry(name.to_owned()));
        }
        let value =
            parse_value(raw_value).ok_or_else(|| CredentialError::InvalidEntry(name.to_owned()))?;
        validate_value(&value).map_err(|_| CredentialError::InvalidEntry(name.to_owned()))?;
        found = Some((index, value));
    }
    Ok(found)
}

fn assignment_value<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let mut line = line.trim_start();
    if let Some(rest) = line.strip_prefix("export ") {
        line = rest.trim_start();
    }
    let rest = line.strip_prefix(name)?;
    if rest.is_empty() {
        return Some("");
    }
    if !matches!(rest.as_bytes()[0], b'=' | b' ' | b'\t') {
        return None;
    }
    Some(rest.trim_start().strip_prefix('=').unwrap_or(""))
}

fn parse_value(raw: &str) -> Option<String> {
    let raw = raw.trim_start();
    if let Some(quoted) = raw.strip_prefix('"') {
        let mut value = String::new();
        let mut chars = quoted.chars();
        while let Some(character) = chars.next() {
            match character {
                '"' => {
                    let rest = chars.as_str().trim();
                    return (rest.is_empty() || rest.starts_with('#')).then_some(value);
                }
                '\\' => match chars.next()? {
                    '\\' => value.push('\\'),
                    '"' => value.push('"'),
                    '$' => value.push('$'),
                    '`' => value.push('`'),
                    'n' => value.push('\n'),
                    'r' => value.push('\r'),
                    't' => value.push('\t'),
                    _ => return None,
                },
                _ => value.push(character),
            }
        }
        None
    } else if let Some(quoted) = raw.strip_prefix('\'') {
        let (value, rest) = quoted.split_once('\'')?;
        (rest.trim().is_empty() || rest.trim_start().starts_with('#')).then(|| value.to_owned())
    } else {
        let value = raw.split_once(" #").map_or(raw, |(value, _)| value).trim();
        Some(value.to_owned())
    }
}

fn escape_value(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('$', "\\$")
        .replace('`', "\\`")
}

#[cfg(test)]
#[path = "credentials_tests.rs"]
mod tests;

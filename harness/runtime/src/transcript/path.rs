use std::io;
use std::path::{Path, PathBuf};

use icu_normalizer::ComposingNormalizerBorrowed;
use uuid::Uuid;

use tickloop_harness_base::private_fs::{PrivateDirectory, current_uid};

/// Transcript project directory and file name suffix; listing looks them up with the same layout.
const PROJECTS_DIRECTORY: &str = "projects";
pub(super) const TRANSCRIPT_FILE_SUFFIX: &str = ".jsonl";
/// Most file systems limit a single path segment to 255 bytes; following Claude Code, keep 200 to
/// leave room for the hash suffix.
const MAX_SANITIZED_LENGTH: usize = 200;
const TEMP_DIRECTORY_ENV: &str = "TICKLOOP_TMPDIR";
const DEFAULT_TEMP_DIRECTORY: &str = "/tmp";
const PRIVATE_TEMP_DIRECTORY_PREFIX: &str = "tickloop-private-";
const FAMILY_TEMP_SUBDIRECTORY: &str = "sessions";

/// `<root>/projects/<project directory name>`. The project directory name follows Claude Code: the
/// canonical path, NFC-normalized, with non-alphanumeric characters replaced; when the working
/// directory cannot be resolved, the original path is used.
pub(crate) fn project_directory(root: &Path, workdir: &Path) -> PathBuf {
    root.join(PROJECTS_DIRECTORY)
        .join(project_directory_name(workdir))
}

/// `<temp dir>/tickloop-private-<uid>/<project directory name>`. The temp dir is `TICKLOOP_TMPDIR`
/// (corresponding to `CLAUDE_CODE_TMPDIR`), or `/tmp` when unset; as with Claude Code's
/// `getClaudeTempDir`, symlinks are resolved only once for this base directory, and the levels
/// below it are literal paths. A separate namespace leaves legacy shared roots untouched; every new
/// layer still requires private ownership and permissions, with no fallback when the new root is
/// unsafe.
pub(crate) fn private_project_temp_directory(workdir: &Path) -> PrivateDirectory {
    private_temp_root().join(project_directory_name(workdir))
}

pub(crate) fn private_family_temp_directory(root_chat_id: Uuid) -> PrivateDirectory {
    private_temp_root()
        .join(FAMILY_TEMP_SUBDIRECTORY)
        .join(root_chat_id.to_string())
}

fn private_temp_root() -> PrivateDirectory {
    let base = std::env::var_os(TEMP_DIRECTORY_ENV)
        .filter(|value| !value.is_empty())
        .map_or_else(|| PathBuf::from(DEFAULT_TEMP_DIRECTORY), PathBuf::from);
    let base = base.canonicalize().unwrap_or(base);
    PrivateDirectory::anchored(
        base,
        format!("{PRIVATE_TEMP_DIRECTORY_PREFIX}{}", current_uid()),
    )
}

fn project_directory_name(workdir: &Path) -> String {
    let resolved = workdir
        .canonicalize()
        .unwrap_or_else(|_| workdir.to_path_buf());
    let normalized = ComposingNormalizerBorrowed::new_nfc()
        .normalize(&resolved.to_string_lossy())
        .into_owned();
    sanitize_path(&normalized)
}

pub(crate) fn transcript_path(project_directory: &Path, chat_id: Uuid) -> PathBuf {
    project_directory.join(format!("{chat_id}{TRANSCRIPT_FILE_SUFFIX}"))
}

/// Finds a Transcript by Chat id across all project directories; resuming does not require the
/// original working directory to still exist.
pub(crate) fn locate(root: &Path, chat_id: Uuid) -> io::Result<Option<PathBuf>> {
    let projects = match std::fs::read_dir(root.join(PROJECTS_DIRECTORY)) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    for project in projects {
        let candidate = transcript_path(&project?.path(), chat_id);
        if candidate.is_file() {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

/// Following Claude Code's `sanitizePath`: replaces non-ASCII-alphanumeric characters with `-` per
/// UTF-16 code unit, and when too long truncates and appends a djb2 hash (Claude Code's fallback
/// implementation under Node).
fn sanitize_path(name: &str) -> String {
    let sanitized = name
        .encode_utf16()
        .map(|unit| match u8::try_from(unit) {
            Ok(byte) if byte.is_ascii_alphanumeric() => char::from(byte),
            _ => '-',
        })
        .collect::<String>();
    if sanitized.len() <= MAX_SANITIZED_LENGTH {
        return sanitized;
    }
    format!(
        "{}-{}",
        &sanitized[..MAX_SANITIZED_LENGTH],
        djb2_base36(name)
    )
}

fn djb2_base36(name: &str) -> String {
    let hash = name.encode_utf16().fold(0_i32, |hash, unit| {
        (hash << 5).wrapping_sub(hash).wrapping_add(i32::from(unit))
    });
    let mut value = i64::from(hash).unsigned_abs();
    if value == 0 {
        return "0".into();
    }
    let mut digits = Vec::new();
    while value > 0 {
        let digit = (value % 36) as u32;
        digits.push(char::from_digit(digit, 36).unwrap());
        value /= 36;
    }
    digits.iter().rev().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_matches_claude_code_for_short_and_long_paths() {
        assert_eq!(
            sanitize_path("/Users/foo/my-project"),
            "-Users-foo-my-project"
        );
        assert_eq!(sanitize_path("plugin:name:server"), "plugin-name-server");
        assert_eq!(sanitize_path("/tmp/研究😀"), "-tmp-----");
        let long = format!("/{}", "a".repeat(250));
        let sanitized = sanitize_path(&long);
        assert!(sanitized.starts_with(&format!("-{}", "a".repeat(199))));
        assert_eq!(sanitized.len(), 201 + djb2_base36(&long).len());
    }

    #[test]
    fn djb2_matches_the_javascript_int32_arithmetic() {
        // node -e 'let h=0;for(const c of "hello")h=((h<<5)-h+c.charCodeAt(0))|0;console.log(Math.abs(h).toString(36))'
        assert_eq!(djb2_base36("hello"), "1n1e4y");
        assert_eq!(djb2_base36(""), "0");
    }
}

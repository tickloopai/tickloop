use std::io;
use std::path::{Path, PathBuf};

use icu_normalizer::ComposingNormalizerBorrowed;
use uuid::Uuid;

use crate::private_fs::{PrivateDirectory, current_uid};

/// Transcript 的项目目录与文件名后缀；过期清理与列表按同一布局查找。
pub(crate) const PROJECTS_DIRECTORY: &str = "projects";
pub(crate) const TRANSCRIPT_FILE_SUFFIX: &str = ".jsonl";
/// 多数文件系统单个路径段上限 255 字节；照 Claude Code 保留 200，给哈希后缀留出空间。
const MAX_SANITIZED_LENGTH: usize = 200;
const TEMP_DIRECTORY_ENV: &str = "TICKLOOP_TMPDIR";
const DEFAULT_TEMP_DIRECTORY: &str = "/tmp";
const PRIVATE_TEMP_DIRECTORY_PREFIX: &str = "tickloop-private-";
const FAMILY_TEMP_SUBDIRECTORY: &str = "sessions";

/// `<root>/projects/<项目目录名>`。项目目录名照 Claude Code：canonical 路径做 NFC 规范化后
/// 替换非字母数字字符；工作目录无法解析时按原路径计算。
pub(crate) fn project_directory(root: &Path, workdir: &Path) -> PathBuf {
    root.join(PROJECTS_DIRECTORY)
        .join(project_directory_name(workdir))
}

/// `<临时目录>/tickloop-private-<uid>/<项目目录名>`。
/// 临时目录取 `TICKLOOP_TMPDIR`（对应 `CLAUDE_CODE_TMPDIR`），未设置时为 `/tmp`；
/// 与 Claude Code `getClaudeTempDir` 相同，只对这个基础目录解析一次符号链接，其下各层是字面路径。
/// A separate namespace leaves legacy shared roots untouched; every new layer still requires
/// private ownership and permissions, with no fallback when the new root is unsafe.
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

/// 按 Chat id 在所有项目目录中查找 Transcript；恢复不要求原工作目录仍然存在。
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

/// 照 Claude Code `sanitizePath`：按 UTF-16 码元把非 ASCII 字母数字替换为 `-`，
/// 超长时截断并追加 djb2 哈希（Claude Code 在 Node 下的回退实现）。
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

use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Component, Path, PathBuf};

use icu_normalizer::ComposingNormalizerBorrowed;
use uuid::Uuid;

use crate::private_fs::{
    PRIVATE_DIRECTORY_MODE, create_private_directory, current_uid, verify_private_directory,
};

/// Transcript 的项目目录与文件名后缀；过期清理与列表按同一布局查找。
pub(crate) const PROJECTS_DIRECTORY: &str = "projects";
pub(crate) const TRANSCRIPT_FILE_SUFFIX: &str = ".jsonl";
const TEMPORARY_DIRECTORY_DESCRIPTION: &str = "temporary directory";
/// 多数文件系统单个路径段上限 255 字节；照 Claude Code 保留 200，给哈希后缀留出空间。
const MAX_SANITIZED_LENGTH: usize = 200;
const TEMP_DIRECTORY_ENV: &str = "TICKLOOP_TMPDIR";
const DEFAULT_TEMP_DIRECTORY: &str = "/tmp";
const PRIVATE_TEMP_DIRECTORY_PREFIX: &str = "tickloop-private-";
const SESSION_TEMP_SUBDIRECTORY: &str = "sessions";

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

pub(crate) fn private_session_temp_directory(session_id: Uuid) -> PrivateDirectory {
    private_temp_root()
        .join(SESSION_TEMP_SUBDIRECTORY)
        .join(session_id.to_string())
}

fn private_temp_root() -> PrivateDirectory {
    let base = std::env::var_os(TEMP_DIRECTORY_ENV)
        .filter(|value| !value.is_empty())
        .map_or_else(|| PathBuf::from(DEFAULT_TEMP_DIRECTORY), PathBuf::from);
    let base = base.canonicalize().unwrap_or(base);
    let path = base.join(format!("{PRIVATE_TEMP_DIRECTORY_PREFIX}{}", current_uid()));
    PrivateDirectory {
        anchor: Some(base),
        path,
    }
}

/// Tick Loop 自己写入的目录。位于用户配置目录下时照常创建；位于系统临时目录下时，
/// 其他用户可以在共享的 `/tmp` 中抢先布置目录或符号链接，因此锚点以下
/// 每一层都必须是当前用户拥有、没有 group/other 权限位的真实目录。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PrivateDirectory {
    /// 可信的基础目录；`None` 表示整条路径可信。
    anchor: Option<PathBuf>,
    path: PathBuf,
}

impl PrivateDirectory {
    pub(crate) fn trusted(path: PathBuf) -> Self {
        Self { anchor: None, path }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn join(&self, name: impl AsRef<Path>) -> Self {
        Self {
            anchor: self.anchor.clone(),
            path: self.path.join(name),
        }
    }

    /// 创建目录（已存在时沿用），失败或校验不通过时返回说明原因的错误。
    pub(crate) fn create(&self) -> io::Result<()> {
        let Some(anchor) = &self.anchor else {
            return create_private_directory(&self.path);
        };
        let relative = self.path.strip_prefix(anchor).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "{} is outside the temporary directory {}",
                    self.path.display(),
                    anchor.display()
                ),
            )
        })?;
        create_private_directory(anchor)?;
        let mut current = anchor.clone();
        for component in relative.components() {
            let Component::Normal(name) = component else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{} is not a normalized path", self.path.display()),
                ));
            };
            current.push(name);
            match std::fs::DirBuilder::new()
                .mode(PRIVATE_DIRECTORY_MODE)
                .create(&current)
            {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
            verify_private_directory(&current, TEMPORARY_DIRECTORY_DESCRIPTION)?;
        }
        Ok(())
    }
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
    use std::os::unix::fs::MetadataExt;

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

    fn anchored(anchor: &Path, relative: &str) -> PrivateDirectory {
        PrivateDirectory {
            anchor: Some(anchor.to_path_buf()),
            path: anchor.join(relative),
        }
    }

    #[test]
    fn private_temp_directories_are_created_owner_only_and_reused() {
        let anchor = tempfile::tempdir().unwrap();
        let directory = anchored(anchor.path(), "tickloop-1/project/chat/tasks");
        directory.create().unwrap();
        directory.create().unwrap();
        for layer in [
            "tickloop-1",
            "tickloop-1/project",
            "tickloop-1/project/chat/tasks",
        ] {
            let metadata = std::fs::symlink_metadata(anchor.path().join(layer)).unwrap();
            assert!(metadata.is_dir());
            assert_eq!(metadata.mode() & 0o777, 0o700, "{layer}");
        }
    }

    #[test]
    fn private_temp_directories_reject_symbolic_links_and_shared_layers() {
        use std::os::unix::fs::PermissionsExt;
        let anchor = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        std::fs::create_dir(anchor.path().join("tickloop-1")).unwrap();
        std::fs::set_permissions(
            anchor.path().join("tickloop-1"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), anchor.path().join("tickloop-1/linked"))
            .unwrap();
        let error = anchored(anchor.path(), "tickloop-1/linked/chat")
            .create()
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("symbolic link"), "{error}");
        assert!(!elsewhere.path().join("chat").exists());

        let shared = anchor.path().join("tickloop-1/shared");
        std::fs::create_dir(&shared).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o750)).unwrap();
        let error = anchored(anchor.path(), "tickloop-1/shared/chat")
            .create()
            .unwrap_err();
        assert!(error.to_string().contains("mode 0750"), "{error}");
        assert!(!shared.join("chat").exists());

        std::fs::write(anchor.path().join("tickloop-1/file"), "").unwrap();
        let error = anchored(anchor.path(), "tickloop-1/file/chat")
            .create()
            .unwrap_err();
        assert!(error.to_string().contains("not a directory"), "{error}");
    }

    #[test]
    fn djb2_matches_the_javascript_int32_arithmetic() {
        // node -e 'let h=0;for(const c of "hello")h=((h<<5)-h+c.charCodeAt(0))|0;console.log(Math.abs(h).toString(36))'
        assert_eq!(djb2_base36("hello"), "1n1e4y");
        assert_eq!(djb2_base36(""), "0");
    }
}

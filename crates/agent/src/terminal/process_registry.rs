//! 进程登记：照 Claude Code `utils/concurrentSessions.ts`，每个 Agent 进程在
//! `<root>/sessions/<pid>.json` 记录自己和当前前台 Chat，退出时删除；统计活动进程时
//! 清理已退出进程留下的文件。所有写入失败只影响登记本身，不影响运行。

use std::{
    fs,
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

use serde::Serialize;
use uuid::Uuid;

use crate::private_fs::{PRIVATE_DIRECTORY_MODE, PRIVATE_FILE_MODE, unix_time_milliseconds};

const SESSIONS_DIRECTORY: &str = "sessions";
const INTERACTIVE_KIND: &str = "interactive";
/// 照 Claude Code `CLAUDE_CODE_ENTRYPOINT`：有终端时为 `cli`，否则为 `sdk-cli`。
const TERMINAL_ENTRYPOINT: &str = "cli";
const PIPED_ENTRYPOINT: &str = "sdk-cli";

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Registration {
    pid: u32,
    chat_id: Option<Uuid>,
    cwd: PathBuf,
    started_at: u64,
    kind: &'static str,
    entrypoint: &'static str,
}

/// 本进程的登记文件；析构时删除。
pub(super) struct ProcessRegistration {
    path: PathBuf,
    record: Registration,
}

impl ProcessRegistration {
    /// 写入登记文件。失败时返回 `None`，本次运行不登记。
    pub(super) fn register(
        root: &Path,
        chat_id: Option<Uuid>,
        cwd: &Path,
        interactive: bool,
    ) -> Option<Self> {
        let directory = root.join(SESSIONS_DIRECTORY);
        fs::create_dir_all(&directory).ok()?;
        fs::set_permissions(
            &directory,
            fs::Permissions::from_mode(PRIVATE_DIRECTORY_MODE),
        )
        .ok()?;
        let pid = std::process::id();
        let registration = Self {
            path: directory.join(format!("{pid}.json")),
            record: Registration {
                pid,
                chat_id,
                cwd: cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf()),
                started_at: unix_time_milliseconds(),
                kind: INTERACTIVE_KIND,
                entrypoint: if interactive {
                    TERMINAL_ENTRYPOINT
                } else {
                    PIPED_ENTRYPOINT
                },
            },
        };
        registration.write().then_some(registration)
    }

    pub(super) fn chat_id(&self) -> Option<Uuid> {
        self.record.chat_id
    }

    /// 前台 Chat 切换后改写登记文件。
    pub(super) fn update_chat(&mut self, chat_id: Option<Uuid>) {
        if self.record.chat_id != chat_id {
            self.record.chat_id = chat_id;
            self.write();
        }
    }

    fn write(&self) -> bool {
        let Ok(encoded) = serde_json::to_vec(&self.record) else {
            return false;
        };
        fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(PRIVATE_FILE_MODE)
            .open(&self.path)
            .and_then(|mut file| file.write_all(&encoded))
            .is_ok()
    }
}

impl Drop for ProcessRegistration {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// 照 Claude Code `countConcurrentSessions`：统计登记目录中仍在运行的进程（含本进程），
/// 并删除已退出进程的文件。只处理严格形如 `<pid>.json` 的文件名。
pub(super) fn count_concurrent_processes(root: &Path) -> usize {
    let directory = root.join(SESSIONS_DIRECTORY);
    let Ok(entries) = fs::read_dir(&directory) else {
        return 0;
    };
    let own_pid = std::process::id();
    let mut count = 0;
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(pid) = file_name
            .to_str()
            .and_then(|name| name.strip_suffix(".json"))
            .filter(|stem| !stem.is_empty() && stem.bytes().all(|byte| byte.is_ascii_digit()))
            .and_then(|stem| stem.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == own_pid || is_process_running(pid) {
            count += 1;
        } else {
            let _ = fs::remove_file(entry.path());
        }
    }
    count
}

/// 照 Claude Code `isProcessRunning`：`kill(pid, 0)` 成功才算存活。
fn is_process_running(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: 信号 0 只做存在与权限检查，不向目标进程发送信号。
    pid > 1 && unsafe { libc::kill(pid, 0) } == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registration_is_written_updated_and_removed_on_drop() {
        let root = tempfile::tempdir().unwrap();
        let first_chat = Uuid::new_v4();
        let mut registration =
            ProcessRegistration::register(root.path(), Some(first_chat), root.path(), false)
                .unwrap();
        let path = root
            .path()
            .join(SESSIONS_DIRECTORY)
            .join(format!("{}.json", std::process::id()));
        let written: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(written["pid"], std::process::id());
        assert_eq!(written["chatId"], first_chat.to_string());
        assert_eq!(written["kind"], "interactive");
        assert_eq!(written["entrypoint"], "sdk-cli");
        assert!(written["startedAt"].as_u64().unwrap() > 0);

        let second_chat = Uuid::new_v4();
        registration.update_chat(Some(second_chat));
        let updated: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(updated["chatId"], second_chat.to_string());
        assert_eq!(updated["startedAt"], written["startedAt"]);
        drop(registration);
        assert!(!path.exists());
    }

    #[test]
    fn counting_processes_removes_only_files_of_exited_processes() {
        let root = tempfile::tempdir().unwrap();
        let _registration =
            ProcessRegistration::register(root.path(), None, root.path(), true).unwrap();
        let directory = root.path().join(SESSIONS_DIRECTORY);
        let mut exited = std::process::Command::new("true").spawn().unwrap();
        let exited_pid = exited.id();
        exited.wait().unwrap();
        let stale = directory.join(format!("{exited_pid}.json"));
        fs::write(&stale, "{}").unwrap();
        let notes = directory.join("2026-03-14_notes.json");
        fs::write(&notes, "keep").unwrap();

        assert_eq!(count_concurrent_processes(root.path()), 1);
        assert!(!stale.exists());
        assert!(notes.exists());
    }
}

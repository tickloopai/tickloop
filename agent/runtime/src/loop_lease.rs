//! Trigger 的跨进程租约与停止状态，参考 Claude Code `utils/cronTasksLock.ts` 与 `cronScheduler.ts`。
//!
//! 同一个 Chat 可以在多个进程中恢复，同一个 Loop 的 Trigger 因此可能被多个进程武装。Loop 将来
//! 承载定时交易，"同一 Loop 同一时刻只由一个进程触发、停止之后不再触发"是安全约束。
//!
//! **租约**：Trigger 触发前获取 `<Chat 目录>/loops/<loopId>.lock`。锁文件以独占创建（写临时文件
//! 后硬链接）建立，内容 `{chatId, pid, nonce, startedAt, host}` 只用于诊断；持有权由对该文件的
//! `flock(LOCK_EX | LOCK_NB)` 决定，[`LoopLease`] 在持有期间保持文件描述符打开。进程退出（包括
//! 被 SIGKILL）时内核释放 flock，不判断 PID 是否存活，因此没有"确认持有者已退出再删除"的
//! 检查与删除之间的窗口，也不受 PID 复用影响。flock 属于打开的文件描述：同一进程内两个 Agent
//! 实例各自打开文件，同样互斥；Rust 打开的文件带 close-on-exec，工具启动的子进程不会继承锁。
//!
//! 归还时先在仍持有 flock 的情况下删除锁文件，再关闭描述符。获取者拿到 flock 后确认锁路径仍指向
//! 自己打开的文件（设备号与 inode 相同）：若它打开的是随后被持有者删除的旧文件，路径已不存在或
//! 换成新文件，放弃这个描述符重新打开。只有持有 flock 的一方删除，所以不会删掉别人持有的锁。
//! 锁内容记录的主机与本机不同时拒绝接管并报告：锁目录可能位于多台机器共享的文件系统上，flock
//! 不一定跨主机生效。
//!
//! **停止状态**：`<loopId>.state` 记录 `{generation, armed, ended, updatedAt, pid}`，以临时文件加
//! rename 原子替换，读改写在 `loops/.state.lock` 的 flock 内完成。结束（含 rewind 移除）、解除
//! 武装、重新武装都递增 generation；结束不可撤销，之后的写入不覆盖它。持锁进程每次触发前读取：
//! 已结束，或已解除武装且 generation 新于本进程所知，就同步本地 Loop（停止或解除武装、取消
//! Trigger 任务、归还租约）且不触发。仅删除锁文件的旧做法会让另一实例接手后继续触发。
//!
//! 与 Claude Code 不同：Claude Code 恢复时只要 sessionId 相同就改写为自己；Tickloop 的两个进程
//! 可以同时继续同一个 Chat，所以以 flock 判断持有者。不持久化的 Agent 的 Chat 不会被其他进程
//! 恢复，不使用租约与停止状态。

use std::collections::HashMap;
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{self, Read, Write};
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::Agent;
use tickloop_agent_base::private_fs::{
    PRIVATE_FILE_MODE, PrivateDirectory, replace_file_atomically,
};
use tickloop_agent_protocol::{ChatId, LoopId, LoopTriggerNotice, LoopTriggerNoticeKind};

/// 照 Claude Code `LOCK_PROBE_INTERVAL_MS`：拿不到租约的进程重试获取的间隔。
pub(super) const LEASE_RETRY_INTERVAL: Duration = Duration::from_secs(5);
const LEASE_DIRECTORY: &str = "loops";
const STATE_WRITE_LOCK_FILE: &str = ".state.lock";
/// 打开的锁文件恰好被持有者删除或替换时重新打开的次数；超过时视为被其他进程持有。
const LOCK_OPEN_ATTEMPTS: usize = 3;
const HELD_ELSEWHERE_MESSAGE: &str =
    "another Tickloop process holds this goal's trigger; the event was not delivered";

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct LeaseHolder {
    chat_id: ChatId,
    pid: u32,
    nonce: Uuid,
    started_at: u64,
    host: String,
}

/// 读取锁内容时只关心主机；旧格式或损坏的内容没有主机，视为本机。
#[derive(Deserialize)]
struct RecordedHost {
    host: Option<String>,
}

/// 本进程持有的一份租约：持有期间保持锁文件的描述符与 flock。析构即归还。
pub(super) struct LoopLease {
    path: PathBuf,
    file: File,
}

impl LoopLease {
    fn is_still_at_path(&self) -> bool {
        refers_to_same_file(&self.file, &self.path).unwrap_or(false)
    }
}

impl Drop for LoopLease {
    fn drop(&mut self) {
        // 仍持有 flock 时删除；随后 `file` 关闭，内核释放 flock。
        if self.is_still_at_path() {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct LoopRunState {
    generation: u64,
    armed: bool,
    ended: bool,
    updated_at: u64,
    pid: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum RunStateChange {
    Armed,
    Disarmed,
    Ended,
}

enum LeaseAttempt {
    Acquired(LoopLease),
    HeldElsewhere,
    ForeignHost(String),
}

/// 持有租约后读取停止状态得到的结论。
enum LeaseCheck {
    Held,
    HeldElsewhere,
    ForeignHost(String),
    EndedElsewhere,
    DisarmedElsewhere(u64),
}

/// 一次触发前的判断结果。
pub(super) enum TriggerPermit {
    Fire,
    /// 暂不能触发（被其他进程持有、锁来自另一台主机、Loop 暂不可触发），稍后重试。
    Wait(String),
    /// 停止状态显示 Loop 已在其他进程结束或解除武装，本地已同步。
    Withdrawn,
    Failed(String),
}

/// 本进程持有的全部租约、已知的停止状态 generation 与待显示的提示。
#[derive(Default)]
pub(super) struct LoopLeases {
    held: Mutex<HashMap<(ChatId, LoopId), LoopLease>>,
    known_generations: Mutex<HashMap<(ChatId, LoopId), u64>>,
    notices: Mutex<HashMap<(ChatId, LoopId), LoopTriggerNotice>>,
}

impl LoopLeases {
    /// 确保持有租约并读取停止状态。持有同一把 `held` 锁完成获取，本进程内的并发触发不会
    /// 因为自己的 flock 而误判为被其他进程持有。
    fn check(
        &self,
        directory: &PrivateDirectory,
        chat_id: ChatId,
        loop_id: LoopId,
    ) -> io::Result<LeaseCheck> {
        let key = (chat_id, loop_id);
        {
            let mut held = self.held.lock().unwrap();
            // 锁文件被外部删除或替换时，别的进程可能已在新文件上持有：放弃旧描述符重新获取。
            if held
                .get(&key)
                .is_some_and(|lease| !lease.is_still_at_path())
            {
                held.remove(&key);
            }
            if let std::collections::hash_map::Entry::Vacant(vacant) = held.entry(key) {
                match try_acquire(directory, chat_id, loop_id)? {
                    LeaseAttempt::Acquired(lease) => {
                        vacant.insert(lease);
                    }
                    LeaseAttempt::HeldElsewhere => return Ok(LeaseCheck::HeldElsewhere),
                    LeaseAttempt::ForeignHost(host) => return Ok(LeaseCheck::ForeignHost(host)),
                }
            }
        }
        let known = self.known_generation(key);
        Ok(match read_run_state(directory, loop_id)? {
            Some(state) if state.ended => LeaseCheck::EndedElsewhere,
            Some(state) if !state.armed && state.generation > known => {
                LeaseCheck::DisarmedElsewhere(state.generation)
            }
            _ => LeaseCheck::Held,
        })
    }

    fn known_generation(&self, key: (ChatId, LoopId)) -> u64 {
        self.known_generations
            .lock()
            .unwrap()
            .get(&key)
            .copied()
            .unwrap_or(0)
    }

    fn remember_generation(&self, key: (ChatId, LoopId), generation: u64) {
        let mut known = self.known_generations.lock().unwrap();
        let entry = known.entry(key).or_insert(0);
        *entry = (*entry).max(generation);
    }

    pub(super) fn set_notice(
        &self,
        chat_id: ChatId,
        loop_id: LoopId,
        kind: LoopTriggerNoticeKind,
        message: String,
    ) {
        self.notices.lock().unwrap().insert(
            (chat_id, loop_id),
            LoopTriggerNotice {
                chat_id,
                loop_id,
                kind,
                message,
            },
        );
    }

    pub(super) fn clear_notice(&self, chat_id: ChatId, loop_id: LoopId) {
        self.notices.lock().unwrap().remove(&(chat_id, loop_id));
    }

    pub(super) fn release(&self, chat_id: ChatId, loop_id: LoopId) {
        let released = self.held.lock().unwrap().remove(&(chat_id, loop_id));
        drop(released);
        self.clear_notice(chat_id, loop_id);
    }

    pub(super) fn release_all(&self) {
        let released = std::mem::take(&mut *self.held.lock().unwrap());
        drop(released);
        self.notices.lock().unwrap().clear();
    }
}

/// 尝试获取租约：打开锁文件（不存在时独占创建）并尝试 flock。
fn try_acquire(
    directory: &PrivateDirectory,
    chat_id: ChatId,
    loop_id: LoopId,
) -> io::Result<LeaseAttempt> {
    directory.create()?;
    let path = directory.path().join(format!("{loop_id}.lock"));
    let holder = LeaseHolder {
        chat_id,
        pid: std::process::id(),
        nonce: Uuid::new_v4(),
        started_at: tickloop_agent_base::private_fs::unix_time_milliseconds(),
        host: local_host_name(),
    };
    let content = serde_json::to_vec(&holder).map_err(io::Error::other)?;
    for _ in 0..LOCK_OPEN_ATTEMPTS {
        let mut file = match OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                create_exclusive(directory, &path, &content)?;
                continue;
            }
            Err(error) => return Err(error),
        };
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Ok(LeaseAttempt::HeldElsewhere),
            Err(TryLockError::Error(error)) => return Err(error),
        }
        if !refers_to_same_file(&file, &path)? {
            continue;
        }
        let mut recorded = Vec::new();
        file.read_to_end(&mut recorded)?;
        if let Some(host) = serde_json::from_slice::<RecordedHost>(&recorded)
            .ok()
            .and_then(|recorded| recorded.host)
            .filter(|host| *host != holder.host)
        {
            return Ok(LeaseAttempt::ForeignHost(host));
        }
        file.set_len(0)?;
        file.write_all_at(&content, 0)?;
        return Ok(LeaseAttempt::Acquired(LoopLease { path, file }));
    }
    Ok(LeaseAttempt::HeldElsewhere)
}

/// 写临时文件再硬链接到锁路径：锁文件出现时诊断内容已经完整。路径已存在时由调用方打开它。
fn create_exclusive(directory: &PrivateDirectory, path: &Path, content: &[u8]) -> io::Result<()> {
    let temporary = unique_temporary_path(directory);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(PRIVATE_FILE_MODE)
        .open(&temporary)?;
    let linked = file
        .write_all(content)
        .and_then(|()| std::fs::hard_link(&temporary, path));
    let _ = std::fs::remove_file(&temporary);
    match linked {
        Err(error) if error.kind() != io::ErrorKind::AlreadyExists => Err(error),
        _ => Ok(()),
    }
}

fn unique_temporary_path(directory: &PrivateDirectory) -> PathBuf {
    directory
        .path()
        .join(format!(".{}.{}.tmp", std::process::id(), Uuid::new_v4()))
}

fn refers_to_same_file(file: &File, path: &Path) -> io::Result<bool> {
    let opened = file.metadata()?;
    match std::fs::symlink_metadata(path) {
        Ok(current) => Ok(current.dev() == opened.dev() && current.ino() == opened.ino()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn local_host_name() -> String {
    let mut buffer = [0u8; 256];
    // SAFETY: 缓冲区长度与传入的长度一致；gethostname 最多写入该长度。
    let result = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) };
    if result != 0 {
        return String::new();
    }
    let length = buffer
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(buffer.len());
    String::from_utf8_lossy(&buffer[..length]).into_owned()
}

fn run_state_path(directory: &PrivateDirectory, loop_id: LoopId) -> PathBuf {
    directory.path().join(format!("{loop_id}.state"))
}

fn read_run_state(
    directory: &PrivateDirectory,
    loop_id: LoopId,
) -> io::Result<Option<LoopRunState>> {
    let path = run_state_path(directory, loop_id);
    match std::fs::read(&path) {
        Ok(content) => serde_json::from_slice(&content).map(Some).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} is unreadable: {error}", path.display()),
            )
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// 在状态写锁内读取、递增并原子替换停止状态，返回写入后文件中的状态。已结束的状态不被覆盖。
fn write_run_state(
    directory: &PrivateDirectory,
    loop_id: LoopId,
    change: RunStateChange,
) -> io::Result<LoopRunState> {
    directory.create()?;
    let guard = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(PRIVATE_FILE_MODE)
        .custom_flags(libc::O_NOFOLLOW)
        .open(directory.path().join(STATE_WRITE_LOCK_FILE))?;
    guard.lock()?;
    let current = read_run_state(directory, loop_id)?;
    if let Some(current) = current.filter(|current| current.ended) {
        return Ok(current);
    }
    let next = LoopRunState {
        generation: current.map_or(0, |current| current.generation) + 1,
        armed: change == RunStateChange::Armed,
        ended: change == RunStateChange::Ended,
        updated_at: tickloop_agent_base::private_fs::unix_time_milliseconds(),
        pid: std::process::id(),
    };
    let contents = serde_json::to_vec(&next).map_err(io::Error::other)?;
    replace_file_atomically(
        &run_state_path(directory, loop_id),
        &contents,
        PRIVATE_FILE_MODE,
    )?;
    Ok(next)
}

impl Agent {
    async fn lease_directory(&self, chat_id: ChatId) -> Option<PrivateDirectory> {
        self.inner
            .chats
            .lock()
            .await
            .get(&chat_id)
            .and_then(|chat| Some(chat.tools.chat_directory()?.join(LEASE_DIRECTORY)))
    }

    /// Trigger 触发前调用：持有租约、读取停止状态，并确认 Loop 仍可触发。停止状态显示已在其他
    /// 进程结束或解除武装时同步本地 Loop 并返回 `Withdrawn`。
    pub(super) async fn acquire_trigger_permit(
        &self,
        chat_id: ChatId,
        loop_id: LoopId,
    ) -> TriggerPermit {
        self.acquire_loop_permit(chat_id, loop_id, false).await
    }

    pub(super) async fn acquire_feedback_replay_permit(
        &self,
        chat_id: ChatId,
        loop_id: LoopId,
    ) -> TriggerPermit {
        self.acquire_loop_permit(chat_id, loop_id, true).await
    }

    async fn acquire_loop_permit(
        &self,
        chat_id: ChatId,
        loop_id: LoopId,
        feedback_replay: bool,
    ) -> TriggerPermit {
        if self.inner.recorder.is_none() {
            return TriggerPermit::Fire;
        }
        let Some(directory) = self.lease_directory(chat_id).await else {
            return TriggerPermit::Wait("chat is no longer loaded".into());
        };
        let agent = self.clone();
        let checked = tokio::task::spawn_blocking(move || {
            agent.inner.loop_leases.check(&directory, chat_id, loop_id)
        })
        .await
        .map_err(|error| error.to_string())
        .and_then(|checked| checked.map_err(|error| error.to_string()));
        let leases = &self.inner.loop_leases;
        if let Ok(LeaseCheck::DisarmedElsewhere(generation)) = &checked
            && feedback_replay
        {
            leases.remember_generation((chat_id, loop_id), *generation);
            if let Err(error) = self
                .record_loop_run_state(chat_id, loop_id, RunStateChange::Armed)
                .await
            {
                return TriggerPermit::Failed(error);
            }
            return Box::pin(self.acquire_loop_permit(chat_id, loop_id, true)).await;
        }
        match checked {
            Err(error) => {
                let message = format!("could not take this goal's trigger lease: {error}");
                leases.set_notice(
                    chat_id,
                    loop_id,
                    LoopTriggerNoticeKind::Warning,
                    message.clone(),
                );
                TriggerPermit::Failed(message)
            }
            Ok(LeaseCheck::HeldElsewhere) => {
                leases.set_notice(
                    chat_id,
                    loop_id,
                    LoopTriggerNoticeKind::HeldByAnotherProcess,
                    "another Tickloop process holds this goal's trigger".into(),
                );
                TriggerPermit::Wait(HELD_ELSEWHERE_MESSAGE.into())
            }
            Ok(LeaseCheck::ForeignHost(host)) => {
                let message = format!(
                    "this goal's trigger lock was written on host {host}; the loops directory may \
                     be shared between machines, so this process will not take it over. Remove \
                     the lock file if that host no longer runs Tickloop"
                );
                leases.set_notice(
                    chat_id,
                    loop_id,
                    LoopTriggerNoticeKind::Warning,
                    message.clone(),
                );
                TriggerPermit::Wait(message)
            }
            Ok(LeaseCheck::EndedElsewhere) => {
                let _ = self.end_goal_ended_elsewhere(chat_id, loop_id).await;
                TriggerPermit::Withdrawn
            }
            Ok(LeaseCheck::DisarmedElsewhere(generation)) => {
                leases.remember_generation((chat_id, loop_id), generation);
                self.disarm_trigger_disarmed_elsewhere(chat_id, loop_id)
                    .await;
                TriggerPermit::Withdrawn
            }
            Ok(LeaseCheck::Held) => {
                let is_still_armed =
                    self.inner
                        .chats
                        .lock()
                        .await
                        .get(&chat_id)
                        .is_some_and(|chat| {
                            chat.snapshot.loops.iter().any(|goal| {
                                goal.id == loop_id
                                    && !matches!(
                                        goal.state,
                                        tickloop_agent_protocol::LoopState::Stopped
                                            | tickloop_agent_protocol::LoopState::Expired
                                    )
                                    && if feedback_replay {
                                        chat.snapshot.policy.mode != crate::ExecutionMode::Plan
                                            && super::feedback_inbox::FeedbackInbox::new(
                                                chat.tools.chat_directory(),
                                                chat_id,
                                            )
                                            .pending()
                                            .is_ok_and(|inputs| {
                                                inputs
                                                    .iter()
                                                    .any(|input| input.identity.loop_id == loop_id)
                                            })
                                    } else {
                                        (chat.trigger_cancellations.contains_key(&loop_id)
                                            || chat.event_cancellations.contains_key(&loop_id))
                                            && (super::triggers::can_trigger(goal)
                                                || (chat
                                                    .event_cancellations
                                                    .contains_key(&loop_id)
                                                    && goal
                                                        .event_sources
                                                        .iter()
                                                        .any(|source| !source.resume_required)))
                                    }
                            })
                        });
                if !is_still_armed {
                    leases.release(chat_id, loop_id);
                    return TriggerPermit::Wait("goal is not eligible for a trigger".into());
                }
                leases.clear_notice(chat_id, loop_id);
                TriggerPermit::Fire
            }
        }
    }

    /// 记录结束、解除武装或重新武装，让其他进程在下一次触发前看到。结束与解除武装应在归还租约
    /// 之前写入，避免接手的进程在写入前读到旧状态。失败时返回说明，由调用方在归还租约后通过
    /// [`Agent::warn_loop_run_state_failure`] 显示，不影响本地操作。
    pub(super) async fn record_loop_run_state(
        &self,
        chat_id: ChatId,
        loop_id: LoopId,
        change: RunStateChange,
    ) -> Result<(), String> {
        if self.inner.recorder.is_none() {
            return Ok(());
        }
        let Some(directory) = self.lease_directory(chat_id).await else {
            return Ok(());
        };
        self.record_loop_run_state_in(chat_id, loop_id, change, directory)
            .await
    }

    pub(super) async fn record_loop_run_state_in(
        &self,
        chat_id: ChatId,
        loop_id: LoopId,
        change: RunStateChange,
        directory: PrivateDirectory,
    ) -> Result<(), String> {
        if self.inner.recorder.is_none() {
            return Ok(());
        }
        let state =
            tokio::task::spawn_blocking(move || write_run_state(&directory, loop_id, change))
                .await
                .map_err(|error| error.to_string())
                .and_then(|written| written.map_err(|error| error.to_string()))?;
        self.inner
            .loop_leases
            .remember_generation((chat_id, loop_id), state.generation);
        Ok(())
    }

    pub(super) fn warn_loop_run_state_failure(
        &self,
        chat_id: ChatId,
        loop_id: LoopId,
        recorded: Result<(), String>,
    ) {
        if let Err(error) = recorded {
            self.inner.loop_leases.set_notice(
                chat_id,
                loop_id,
                LoopTriggerNoticeKind::Warning,
                format!(
                    "could not record this goal's trigger state ({error}); other Tickloop \
                     processes may not see the change"
                ),
            );
        }
    }

    /// 需要让用户看到的 Trigger 租约提示，按 Chat 与 Loop 排列。
    pub fn loop_trigger_notices(&self) -> Vec<LoopTriggerNotice> {
        let mut notices = self
            .inner
            .loop_leases
            .notices
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect::<Vec<_>>();
        notices.sort_by_key(|notice| (notice.chat_id, notice.loop_id));
        notices
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lease_directory() -> (tempfile::TempDir, PrivateDirectory) {
        let root = tempfile::tempdir().unwrap();
        let directory = PrivateDirectory::trusted(root.path().join(LEASE_DIRECTORY));
        (root, directory)
    }

    fn acquired(attempt: LeaseAttempt) -> LoopLease {
        match attempt {
            LeaseAttempt::Acquired(lease) => lease,
            LeaseAttempt::HeldElsewhere => panic!("the lease is held elsewhere"),
            LeaseAttempt::ForeignHost(host) => panic!("the lease belongs to {host}"),
        }
    }

    #[test]
    fn separate_opens_in_one_process_exclude_each_other_until_release() {
        let (_root, directory) = lease_directory();
        let (chat_id, loop_id) = (Uuid::new_v4(), Uuid::new_v4());
        let lease = acquired(try_acquire(&directory, chat_id, loop_id).unwrap());
        assert!(matches!(
            try_acquire(&directory, chat_id, loop_id).unwrap(),
            LeaseAttempt::HeldElsewhere
        ));
        assert!(lease.path.exists());
        let path = lease.path.clone();
        drop(lease);
        assert!(!path.exists());
        let again = acquired(try_acquire(&directory, chat_id, loop_id).unwrap());
        assert!(again.path.exists());
    }

    #[test]
    fn a_descriptor_opened_before_the_holder_deleted_the_lock_is_not_a_lease() {
        let (_root, directory) = lease_directory();
        let (chat_id, loop_id) = (Uuid::new_v4(), Uuid::new_v4());
        let lease = acquired(try_acquire(&directory, chat_id, loop_id).unwrap());
        let stale = File::open(&lease.path).unwrap();
        let path = lease.path.clone();
        drop(lease);
        // 旧描述符上的 flock 可以拿到，但路径已不指向它；新获取者创建新文件。
        stale.try_lock().unwrap();
        assert!(!refers_to_same_file(&stale, &path).unwrap());
        let current = acquired(try_acquire(&directory, chat_id, loop_id).unwrap());
        assert!(refers_to_same_file(&current.file, &path).unwrap());
    }

    #[test]
    fn a_failed_attempt_leaves_the_holders_lock_in_place() {
        let (_root, directory) = lease_directory();
        let (chat_id, loop_id) = (Uuid::new_v4(), Uuid::new_v4());
        let lease = acquired(try_acquire(&directory, chat_id, loop_id).unwrap());
        for _ in 0..10 {
            assert!(matches!(
                try_acquire(&directory, chat_id, loop_id).unwrap(),
                LeaseAttempt::HeldElsewhere
            ));
        }
        assert!(lease.is_still_at_path());
        let content: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&lease.path).unwrap()).unwrap();
        assert_eq!(content["pid"], std::process::id());
        assert_eq!(content["host"], local_host_name());
    }

    #[test]
    fn ended_state_is_not_overwritten_and_generations_increase() {
        let (_root, directory) = lease_directory();
        let loop_id = Uuid::new_v4();
        let armed = write_run_state(&directory, loop_id, RunStateChange::Armed).unwrap();
        let disarmed = write_run_state(&directory, loop_id, RunStateChange::Disarmed).unwrap();
        assert!(armed.armed && !disarmed.armed);
        assert_eq!(disarmed.generation, armed.generation + 1);
        let ended = write_run_state(&directory, loop_id, RunStateChange::Ended).unwrap();
        let rearmed = write_run_state(&directory, loop_id, RunStateChange::Armed).unwrap();
        assert!(rearmed.ended && !rearmed.armed);
        assert_eq!(rearmed.generation, ended.generation);
        assert!(read_run_state(&directory, loop_id).unwrap().unwrap().ended);
    }

    #[test]
    fn a_state_file_disarmed_after_the_known_generation_withdraws_the_holder() {
        let (_root, directory) = lease_directory();
        let leases = LoopLeases::default();
        let (chat_id, loop_id) = (Uuid::new_v4(), Uuid::new_v4());
        let armed = write_run_state(&directory, loop_id, RunStateChange::Armed).unwrap();
        leases.remember_generation((chat_id, loop_id), armed.generation);
        assert!(matches!(
            leases.check(&directory, chat_id, loop_id).unwrap(),
            LeaseCheck::Held
        ));
        write_run_state(&directory, loop_id, RunStateChange::Disarmed).unwrap();
        assert!(matches!(
            leases.check(&directory, chat_id, loop_id).unwrap(),
            LeaseCheck::DisarmedElsewhere(generation) if generation == armed.generation + 1
        ));
        write_run_state(&directory, loop_id, RunStateChange::Ended).unwrap();
        assert!(matches!(
            leases.check(&directory, chat_id, loop_id).unwrap(),
            LeaseCheck::EndedElsewhere
        ));
    }

    #[test]
    fn a_lock_deleted_under_the_holder_is_reacquired_on_the_next_check() {
        let (_root, directory) = lease_directory();
        let leases = LoopLeases::default();
        let (chat_id, loop_id) = (Uuid::new_v4(), Uuid::new_v4());
        assert!(matches!(
            leases.check(&directory, chat_id, loop_id).unwrap(),
            LeaseCheck::Held
        ));
        let path = directory.path().join(format!("{loop_id}.lock"));
        std::fs::remove_file(&path).unwrap();
        let intruder = acquired(try_acquire(&directory, chat_id, loop_id).unwrap());
        assert!(matches!(
            leases.check(&directory, chat_id, loop_id).unwrap(),
            LeaseCheck::HeldElsewhere
        ));
        drop(intruder);
        assert!(matches!(
            leases.check(&directory, chat_id, loop_id).unwrap(),
            LeaseCheck::Held
        ));
    }
}

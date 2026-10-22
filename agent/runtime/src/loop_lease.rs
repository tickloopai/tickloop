//! Trigger 的跨进程租约与接收状态，锁文件参考 Claude Code `utils/cronTasksLock.ts`。
//!
//! 同一个 Chat 可以在多个进程中恢复，它的 Tick 订阅与 Artifact 反馈因此可能同时送到几个进程。
//! "同一 Chat 同一时刻只由一个进程送达、`/stop` 暂停接收之后不再送达"是安全约束：唤醒底座的
//! 唯一入口每次送达前取得租约，取得租约的进程才把输入交给 Loop。
//!
//! **租约**：送达前获取 `<Chat 目录>/trigger/lease.lock`。锁文件以独占创建（写临时文件后硬链接）
//! 建立，内容 `{chatId, pid, nonce, startedAt, host}` 只用于诊断；持有权由对该文件的
//! `flock(LOCK_EX | LOCK_NB)` 决定，[`ChatLease`] 在持有期间保持文件描述符打开。进程退出（包括
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
//! **接收状态**：`trigger/receiving.state` 记录 `{generation, receiving, updatedAt, pid}`，以临时
//! 文件加 rename 原子替换，读改写在 `trigger/.receiving.lock` 的 flock 内完成。暂停与恢复接收都
//! 递增 generation。持锁进程每次送达前读取：已暂停且 generation 新于本进程所知，就同步本地接收
//! 开关（暂停接收、停止 Artifact 监听、归还租约）且不送达。仅删除锁文件的旧做法会让另一实例接手
//! 后继续送达。
//!
//! **接手**：每次新取得租约都先从存储整份重建该 Chat 的 Trigger 状态（账本中的 Trigger 记录、
//! 等待执行的 Trigger 输入、Tick 已送达的序号），完成之前同一 Chat 的所有送达与取用都在同一道
//! 屏障上等待；重建失败时不送达，下一次送达重试。上一个持有者在租约期间送达、执行或结算的内容
//! 因此和一次干净装载看到的一致。用户输入与 Agent 消息不需要租约，属于各自的进程，不随接手改变。
//!
//! 与 Claude Code 不同：Claude Code 恢复时只要 sessionId 相同就改写为自己；Tickloop 的两个进程
//! 可以同时继续同一个 Chat，所以以 flock 判断持有者。不持久化的 Agent 的 Chat 不会被其他进程
//! 恢复，不使用租约与接收状态。

use std::collections::HashMap;
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{self, Read, Write};
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::Agent;
use tickloop_agent_base::private_fs::{
    PRIVATE_FILE_MODE, PrivateDirectory, replace_file_atomically,
};
use tickloop_agent_protocol::{ChatId, LoopTriggerNotice, LoopTriggerNoticeKind};

const LEASE_DIRECTORY: &str = "trigger";
const LEASE_FILE: &str = "lease.lock";
const STATE_FILE: &str = "receiving.state";
const STATE_WRITE_LOCK_FILE: &str = ".receiving.lock";
/// 打开的锁文件恰好被持有者删除或替换时重新打开的次数；超过时视为被其他进程持有。
const LOCK_OPEN_ATTEMPTS: usize = 3;
const HELD_ELSEWHERE_MESSAGE: &str =
    "another Tickloop process holds this Chat's Trigger; the event was not delivered";

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
pub(super) struct ChatLease {
    path: PathBuf,
    file: File,
}

impl ChatLease {
    fn is_still_at_path(&self) -> bool {
        refers_to_same_file(&self.file, &self.path).unwrap_or(false)
    }
}

impl Drop for ChatLease {
    fn drop(&mut self) {
        // 仍持有 flock 时删除；随后 `file` 关闭，内核释放 flock。
        if self.is_still_at_path() {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReceivingRecord {
    generation: u64,
    receiving: bool,
    updated_at: u64,
    pid: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ReceivingChange {
    Resumed,
    Paused,
}

enum LeaseAttempt {
    Acquired(ChatLease),
    HeldElsewhere,
    ForeignHost(String),
}

/// 持有租约后读取接收状态得到的结论。
enum LeaseCheck {
    /// Held as the holding `epoch`; until `synced`, this process has not taken over what another
    /// process delivered to the Chat while it held the lease.
    Held {
        epoch: u64,
        synced: bool,
    },
    HeldElsewhere,
    ForeignHost(String),
    PausedElsewhere(u64),
}

/// 一次送达前的判断结果。
pub(super) enum TriggerPermit {
    /// Deliver under `holding`, which the acceptance or the start checks again under the Chats
    /// lock: a lease released meanwhile no longer stands behind it.
    Fire(LeaseHolding),
    /// 暂不能送达（被其他进程持有、锁来自另一台主机、Loop 未在接收），稍后重试。
    Wait {
        reason: String,
    },
    /// 接收状态显示已在其他进程暂停，本地已同步。
    Withdrawn,
    Failed(String),
}

/// The lease holding a permit was given under; `None` where a host needs no lease, as one that
/// keeps waits durably or saves no Chats.
#[derive(Clone, Copy, Debug)]
pub(super) struct LeaseHolding(Option<u64>);

/// How long a Chat whose Trigger input waits on a failed permit waits before the first retry,
/// doubling with each failure up to the longest.
const PERMIT_RETRY_FIRST: std::time::Duration = std::time::Duration::from_secs(1);
const PERMIT_RETRY_LONGEST: std::time::Duration = std::time::Duration::from_secs(60);

/// The retry of a Chat's waiting Trigger input after failed permits: how many failed in a row,
/// and the scheduled retry, if one is.
#[derive(Default)]
struct PermitRetry {
    failures: u32,
    scheduled: Option<tokio_util::sync::CancellationToken>,
}

/// The Chat's one serialization boundary for Triggers, held while it is entered. Taking or
/// releasing the lease, taking the Chat's Trigger state over, recording the receiving switch,
/// accepting a delivery and starting queued Trigger input, with the start recorded durably,
/// each happen whole within it.
pub(super) type TriggerBoundary = tokio::sync::OwnedMutexGuard<()>;

/// One holding of a Chat's lease. Taken, it is not synced: no Trigger is accepted or started
/// under it until the Chat's Trigger state is taken over from the store.
struct HeldLease {
    lease: ChatLease,
    epoch: u64,
    synced: bool,
}

/// 本进程持有的全部租约、已知的接收状态 generation 与待显示的提示，都按 Chat。
#[derive(Default)]
pub(super) struct ChatLeases {
    held: Mutex<HashMap<ChatId, HeldLease>>,
    epochs: std::sync::atomic::AtomicU64,
    boundaries: Mutex<HashMap<ChatId, Arc<tokio::sync::Mutex<()>>>>,
    known_generations: Mutex<HashMap<ChatId, u64>>,
    notices: Mutex<HashMap<ChatId, LoopTriggerNotice>>,
    retries: Mutex<HashMap<ChatId, PermitRetry>>,
}

impl ChatLeases {
    /// 确保持有租约并读取接收状态。持有同一把 `held` 锁完成获取，本进程内的并发送达不会
    /// 因为自己的 flock 而误判为被其他进程持有。
    fn check(&self, directory: &PrivateDirectory, chat_id: ChatId) -> io::Result<LeaseCheck> {
        let key = chat_id;
        let (epoch, synced) = {
            let mut held = self.held.lock().unwrap();
            // 锁文件被外部删除或替换时，别的进程可能已在新文件上持有：放弃旧描述符重新获取。
            if held
                .get(&key)
                .is_some_and(|holding| !holding.lease.is_still_at_path())
            {
                held.remove(&key);
            }
            let holding = match held.entry(key) {
                std::collections::hash_map::Entry::Occupied(occupied) => occupied.into_mut(),
                std::collections::hash_map::Entry::Vacant(vacant) => {
                    match try_acquire(directory, chat_id)? {
                        LeaseAttempt::Acquired(lease) => vacant.insert(HeldLease {
                            lease,
                            epoch: self
                                .epochs
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                            synced: false,
                        }),
                        LeaseAttempt::HeldElsewhere => return Ok(LeaseCheck::HeldElsewhere),
                        LeaseAttempt::ForeignHost(host) => {
                            return Ok(LeaseCheck::ForeignHost(host));
                        }
                    }
                }
            };
            (holding.epoch, holding.synced)
        };
        let known = self.known_generation(key);
        Ok(match read_receiving(directory)? {
            Some(state) if !state.receiving && state.generation > known => {
                LeaseCheck::PausedElsewhere(state.generation)
            }
            Some(_) | None => LeaseCheck::Held { epoch, synced },
        })
    }

    fn is_synced(&self, chat_id: ChatId, epoch: u64) -> bool {
        self.held
            .lock()
            .unwrap()
            .get(&chat_id)
            .is_some_and(|holding| holding.epoch == epoch && holding.synced)
    }

    /// The epoch of the Chat's current holding, if this process holds its lease.
    fn holding_epoch(&self, chat_id: ChatId) -> Option<u64> {
        self.held
            .lock()
            .unwrap()
            .get(&chat_id)
            .map(|holding| holding.epoch)
    }

    /// The holding `epoch` took the Chat's Trigger state over; a later holding takes it over
    /// again.
    fn mark_synced(&self, chat_id: ChatId, epoch: u64) {
        if let Some(holding) = self
            .held
            .lock()
            .unwrap()
            .get_mut(&chat_id)
            .filter(|holding| holding.epoch == epoch)
        {
            holding.synced = true;
        }
        self.retries.lock().unwrap().remove(&chat_id);
    }

    /// Releases the holding `epoch` only: a pause releases what was held when it was asked for,
    /// never a holding taken after it.
    fn release_holding(&self, chat_id: ChatId, epoch: u64) {
        let released = {
            let mut held = self.held.lock().unwrap();
            match held.get(&chat_id) {
                Some(holding) if holding.epoch == epoch => held.remove(&chat_id),
                Some(_) | None => None,
            }
        };
        if released.is_some() {
            self.cancel_retry(chat_id);
            self.clear_notice(chat_id);
        }
    }

    /// Whether a permit given under `holding` still stands: the lease it was given under is still
    /// held and synced.
    pub(super) fn stands(&self, chat_id: ChatId, holding: LeaseHolding) -> bool {
        match holding.0 {
            None => true,
            Some(epoch) => self.is_synced(chat_id, epoch),
        }
    }

    /// Schedules the retry of the Chat's waiting Trigger input after a failed permit, unless one
    /// is scheduled: its delay and cancellation.
    fn schedule_retry(
        &self,
        chat_id: ChatId,
    ) -> Option<(std::time::Duration, tokio_util::sync::CancellationToken)> {
        let mut retries = self.retries.lock().unwrap();
        let retry = retries.entry(chat_id).or_default();
        if retry.scheduled.is_some() {
            return None;
        }
        let delay = PERMIT_RETRY_FIRST
            .saturating_mul(2u32.saturating_pow(retry.failures))
            .min(PERMIT_RETRY_LONGEST);
        retry.failures = retry.failures.saturating_add(1);
        let cancel = tokio_util::sync::CancellationToken::new();
        retry.scheduled = Some(cancel.clone());
        Some((delay, cancel))
    }

    fn retry_came_due(&self, chat_id: ChatId) {
        if let Some(retry) = self.retries.lock().unwrap().get_mut(&chat_id) {
            retry.scheduled = None;
        }
    }

    fn cancel_retry(&self, chat_id: ChatId) {
        if let Some(retry) = self.retries.lock().unwrap().remove(&chat_id)
            && let Some(scheduled) = retry.scheduled
        {
            scheduled.cancel();
        }
    }

    fn boundary(&self, chat_id: ChatId) -> Arc<tokio::sync::Mutex<()>> {
        self.boundaries
            .lock()
            .unwrap()
            .entry(chat_id)
            .or_default()
            .clone()
    }

    fn known_generation(&self, key: ChatId) -> u64 {
        self.known_generations
            .lock()
            .unwrap()
            .get(&key)
            .copied()
            .unwrap_or(0)
    }

    fn remember_generation(&self, key: ChatId, generation: u64) {
        let mut known = self.known_generations.lock().unwrap();
        let entry = known.entry(key).or_insert(0);
        *entry = (*entry).max(generation);
    }

    pub(super) fn set_notice(&self, chat_id: ChatId, kind: LoopTriggerNoticeKind, message: String) {
        self.notices.lock().unwrap().insert(
            chat_id,
            LoopTriggerNotice {
                chat_id,
                kind,
                message,
            },
        );
    }

    pub(super) fn clear_notice(&self, chat_id: ChatId) {
        self.notices.lock().unwrap().remove(&chat_id);
    }

    pub(super) fn release(&self, chat_id: ChatId) {
        let released = self.held.lock().unwrap().remove(&chat_id);
        drop(released);
        self.cancel_retry(chat_id);
        self.clear_notice(chat_id);
    }

    pub(super) fn release_all(&self) {
        let released = std::mem::take(&mut *self.held.lock().unwrap());
        drop(released);
        for (_, retry) in std::mem::take(&mut *self.retries.lock().unwrap()) {
            if let Some(scheduled) = retry.scheduled {
                scheduled.cancel();
            }
        }
        self.notices.lock().unwrap().clear();
    }
}

/// 尝试获取租约：打开锁文件（不存在时独占创建）并尝试 flock。
fn try_acquire(directory: &PrivateDirectory, chat_id: ChatId) -> io::Result<LeaseAttempt> {
    directory.create()?;
    let path = directory.path().join(LEASE_FILE);
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
        return Ok(LeaseAttempt::Acquired(ChatLease { path, file }));
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

fn receiving_path(directory: &PrivateDirectory) -> PathBuf {
    directory.path().join(STATE_FILE)
}

fn read_receiving(directory: &PrivateDirectory) -> io::Result<Option<ReceivingRecord>> {
    let path = receiving_path(directory);
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

/// 在状态写锁内读取、递增并原子替换接收状态，返回写入后文件中的状态。
fn write_receiving(
    directory: &PrivateDirectory,
    change: ReceivingChange,
) -> io::Result<ReceivingRecord> {
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
    let current = read_receiving(directory)?;
    let next = ReceivingRecord {
        generation: current.map_or(0, |current| current.generation) + 1,
        receiving: match change {
            ReceivingChange::Resumed => true,
            ReceivingChange::Paused => false,
        },
        updated_at: tickloop_agent_base::private_fs::unix_time_milliseconds(),
        pid: std::process::id(),
    };
    let contents = serde_json::to_vec(&next).map_err(io::Error::other)?;
    replace_file_atomically(&receiving_path(directory), &contents, PRIVATE_FILE_MODE)?;
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

    /// Enters the Chat's Trigger boundary, waiting while another holds it.
    pub(super) async fn enter_trigger_boundary(&self, chat_id: ChatId) -> TriggerBoundary {
        self.inner.chat_leases.boundary(chat_id).lock_owned().await
    }

    /// 每次送达前调用，在 Chat 的 Trigger 边界内：持有该 Chat 的租约、读取接收状态，并确认 Loop
    /// 仍在接收。接收状态显示已在其他进程暂停时同步本地接收开关并返回 `Withdrawn`。
    pub(super) async fn acquire_trigger_permit(
        &self,
        chat_id: ChatId,
        _within: &TriggerBoundary,
    ) -> TriggerPermit {
        // A host that keeps waits durably holds the Chat's whole family under its lease: no
        // other process delivers to it.
        if self.inner.recorder.is_none() || self.inner.waits == super::WaitHosting::Durable {
            return TriggerPermit::Fire(LeaseHolding(None));
        }
        let Some(directory) = self.lease_directory(chat_id).await else {
            return TriggerPermit::Wait {
                reason: "chat is no longer loaded".into(),
            };
        };
        let agent = self.clone();
        let checked =
            tokio::task::spawn_blocking(move || agent.inner.chat_leases.check(&directory, chat_id))
                .await
                .map_err(|error| error.to_string())
                .and_then(|checked| checked.map_err(|error| error.to_string()));
        let leases = &self.inner.chat_leases;
        match checked {
            Err(error) => {
                let message = format!("could not take this Chat's Trigger lease: {error}");
                leases.set_notice(chat_id, LoopTriggerNoticeKind::Warning, message.clone());
                TriggerPermit::Failed(message)
            }
            Ok(LeaseCheck::HeldElsewhere) => {
                leases.set_notice(
                    chat_id,
                    LoopTriggerNoticeKind::HeldByAnotherProcess,
                    "another Tickloop process holds this Chat's Trigger".into(),
                );
                TriggerPermit::Wait {
                    reason: HELD_ELSEWHERE_MESSAGE.into(),
                }
            }
            Ok(LeaseCheck::ForeignHost(host)) => {
                let message = format!(
                    "this Chat's Trigger lock was written on host {host}; the trigger directory \
                     may be shared between machines, so this process will not take it over. \
                     Remove the lock file if that host no longer runs Tickloop"
                );
                leases.set_notice(chat_id, LoopTriggerNoticeKind::Warning, message.clone());
                TriggerPermit::Wait { reason: message }
            }
            Ok(LeaseCheck::PausedElsewhere(generation)) => {
                leases.remember_generation(chat_id, generation);
                self.pause_receiving_paused_elsewhere(chat_id).await;
                TriggerPermit::Withdrawn
            }
            Ok(LeaseCheck::Held { epoch, synced }) => {
                if !synced {
                    if let Err(error) = self.reload_trigger_state(chat_id).await {
                        let message = format!(
                            "could not take over this Chat's Trigger state from its \
                             Transcript: {error}; no Trigger is delivered until it can"
                        );
                        leases.set_notice(chat_id, LoopTriggerNoticeKind::Warning, message.clone());
                        return TriggerPermit::Failed(message);
                    }
                    leases.mark_synced(chat_id, epoch);
                }
                let is_receiving = self
                    .inner
                    .chats
                    .lock()
                    .await
                    .get(&chat_id)
                    .is_some_and(|chat| chat.snapshot.chat_loop.receiving.is_receiving());
                if !is_receiving {
                    leases.release(chat_id);
                    return TriggerPermit::Wait {
                        reason: "this Chat's Loop is not receiving".into(),
                    };
                }
                leases.clear_notice(chat_id);
                TriggerPermit::Fire(LeaseHolding(Some(epoch)))
            }
        }
    }

    /// A permit for the Chat's waiting Trigger input failed: it is tried again after a delay
    /// that doubles with each failure in a row, once at a time, until a takeover succeeds, the
    /// lease is released as receiving pauses, or the Agent shuts down.
    pub(super) fn retry_waiting_trigger_input(&self, chat_id: ChatId) {
        let Some((delay, cancel)) = self.inner.chat_leases.schedule_retry(chat_id) else {
            return;
        };
        self.inner
            .tasks
            .spawn(self.clone().retry_after(chat_id, delay, cancel));
    }

    /// The retry's future is named, not inferred: the run it starts asks for a permit, whose
    /// failure schedules this again.
    fn retry_after(
        self,
        chat_id: ChatId,
        delay: std::time::Duration,
        cancel: tokio_util::sync::CancellationToken,
    ) -> futures_util::future::BoxFuture<'static, ()> {
        Box::pin(async move {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = self.inner.shutdown.cancelled() => return,
                _ = tokio::time::sleep(delay) => {}
            }
            self.inner.chat_leases.retry_came_due(chat_id);
            let _ = self.start_next_agent_run(chat_id).await;
        })
    }

    /// 记录接收开关的变化，让其他进程在下一次送达前看到。暂停应在归还租约之前写入，避免接手的
    /// 进程在写入前读到旧状态。失败时在归还租约后显示提示，不影响本地操作。Within the Chat's
    /// Trigger boundary: a pause releases the holding it was asked under, after whatever start
    /// the boundary was recording.
    pub(super) async fn record_receiving(&self, chat_id: ChatId, change: ReceivingChange) {
        let asked_under = self.inner.chat_leases.holding_epoch(chat_id);
        let _within = self.enter_trigger_boundary(chat_id).await;
        let recorded = match self.lease_directory(chat_id).await {
            Some(directory) if self.inner.recorder.is_some() => {
                tokio::task::spawn_blocking(move || write_receiving(&directory, change))
                    .await
                    .map_err(|error| error.to_string())
                    .and_then(|written| written.map_err(|error| error.to_string()))
                    .map(|state| {
                        self.inner
                            .chat_leases
                            .remember_generation(chat_id, state.generation)
                    })
            }
            Some(_) | None => Ok(()),
        };
        #[cfg(any(test, feature = "test-support"))]
        self.pause_point(
            chat_id,
            super::wakeup::trigger_pauses::PausePoint::ReceivingRecorded,
        )
        .await;
        match (change, asked_under) {
            (ReceivingChange::Paused, Some(epoch)) => {
                self.inner.chat_leases.release_holding(chat_id, epoch);
            }
            (ReceivingChange::Paused, None) | (ReceivingChange::Resumed, _) => {}
        }
        if let Err(error) = recorded {
            self.inner.chat_leases.set_notice(
                chat_id,
                LoopTriggerNoticeKind::Warning,
                format!(
                    "could not record this Chat's receiving state ({error}); other Tickloop \
                     processes may not see the change"
                ),
            );
        }
    }

    /// 需要让用户看到的 Trigger 租约提示，按 Chat 排列。
    pub fn loop_trigger_notices(&self) -> Vec<LoopTriggerNotice> {
        let mut notices = self
            .inner
            .chat_leases
            .notices
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect::<Vec<_>>();
        notices.sort_by_key(|notice| notice.chat_id);
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

    fn acquired(attempt: LeaseAttempt) -> ChatLease {
        match attempt {
            LeaseAttempt::Acquired(lease) => lease,
            LeaseAttempt::HeldElsewhere => panic!("the lease is held elsewhere"),
            LeaseAttempt::ForeignHost(host) => panic!("the lease belongs to {host}"),
        }
    }

    #[test]
    fn separate_opens_in_one_process_exclude_each_other_until_release() {
        let (_root, directory) = lease_directory();
        let chat_id = Uuid::new_v4();
        let lease = acquired(try_acquire(&directory, chat_id).unwrap());
        assert!(matches!(
            try_acquire(&directory, chat_id).unwrap(),
            LeaseAttempt::HeldElsewhere
        ));
        assert!(lease.path.exists());
        let path = lease.path.clone();
        drop(lease);
        assert!(!path.exists());
        let again = acquired(try_acquire(&directory, chat_id).unwrap());
        assert!(again.path.exists());
    }

    #[test]
    fn a_descriptor_opened_before_the_holder_deleted_the_lock_is_not_a_lease() {
        let (_root, directory) = lease_directory();
        let chat_id = Uuid::new_v4();
        let lease = acquired(try_acquire(&directory, chat_id).unwrap());
        let stale = File::open(&lease.path).unwrap();
        let path = lease.path.clone();
        drop(lease);
        // 旧描述符上的 flock 可以拿到，但路径已不指向它；新获取者创建新文件。
        stale.try_lock().unwrap();
        assert!(!refers_to_same_file(&stale, &path).unwrap());
        let current = acquired(try_acquire(&directory, chat_id).unwrap());
        assert!(refers_to_same_file(&current.file, &path).unwrap());
    }

    #[test]
    fn a_failed_attempt_leaves_the_holders_lock_in_place() {
        let (_root, directory) = lease_directory();
        let chat_id = Uuid::new_v4();
        let lease = acquired(try_acquire(&directory, chat_id).unwrap());
        for _ in 0..10 {
            assert!(matches!(
                try_acquire(&directory, chat_id).unwrap(),
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
    fn every_change_increases_the_generation() {
        let (_root, directory) = lease_directory();
        let resumed = write_receiving(&directory, ReceivingChange::Resumed).unwrap();
        let paused = write_receiving(&directory, ReceivingChange::Paused).unwrap();
        assert!(resumed.receiving && !paused.receiving);
        assert_eq!(paused.generation, resumed.generation + 1);
        let again = write_receiving(&directory, ReceivingChange::Resumed).unwrap();
        assert!(again.receiving);
        assert_eq!(again.generation, paused.generation + 1);
        assert!(read_receiving(&directory).unwrap().unwrap().receiving);
    }

    #[test]
    fn a_pause_recorded_after_the_known_generation_withdraws_the_holder() {
        let (_root, directory) = lease_directory();
        let leases = ChatLeases::default();
        let chat_id = Uuid::new_v4();
        let resumed = write_receiving(&directory, ReceivingChange::Resumed).unwrap();
        leases.remember_generation(chat_id, resumed.generation);
        assert!(matches!(
            leases.check(&directory, chat_id).unwrap(),
            LeaseCheck::Held { .. }
        ));
        write_receiving(&directory, ReceivingChange::Paused).unwrap();
        assert!(matches!(
            leases.check(&directory, chat_id).unwrap(),
            LeaseCheck::PausedElsewhere(generation) if generation == resumed.generation + 1
        ));
    }

    #[test]
    fn a_lock_deleted_under_the_holder_is_reacquired_on_the_next_check() {
        let (_root, directory) = lease_directory();
        let leases = ChatLeases::default();
        let chat_id = Uuid::new_v4();
        assert!(matches!(
            leases.check(&directory, chat_id).unwrap(),
            LeaseCheck::Held { .. }
        ));
        let path = directory.path().join(LEASE_FILE);
        std::fs::remove_file(&path).unwrap();
        let intruder = acquired(try_acquire(&directory, chat_id).unwrap());
        assert!(matches!(
            leases.check(&directory, chat_id).unwrap(),
            LeaseCheck::HeldElsewhere
        ));
        drop(intruder);
        assert!(matches!(
            leases.check(&directory, chat_id).unwrap(),
            LeaseCheck::Held { .. }
        ));
    }
}

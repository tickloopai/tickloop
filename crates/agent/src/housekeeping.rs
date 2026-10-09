//! 后台维护，照 Claude Code `utils/backgroundHousekeeping.ts` 中的过期文件清理部分。
//!
//! 开始计时 10 分钟后在后台清理一次过期文件；那时用户最近一分钟内有输入就再推迟 10 分钟，
//! 不让慢操作打扰正在输入的用户。清理在独立线程中进行，不占用异步运行时，进程退出时也不等待它。

use std::{
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

use tokio::{sync::oneshot, task::JoinHandle, time::Instant};

use crate::cleanup::{
    ChatDeletionNotice, CleanupOutcome, CleanupResult, cleanup_old_message_files_in_background,
};

/// 照 Claude Code `DELAY_VERY_SLOW_OPERATIONS_THAT_HAPPEN_EVERY_SESSION`：开始计时后与每次推迟的
/// 等待时间。
pub const SLOW_HOUSEKEEPING_DELAY: Duration = Duration::from_secs(10 * 60);
/// 这段时间内有用户输入时推迟清理。
const RECENT_INTERACTION_WINDOW: Duration = Duration::from_secs(60);

/// 最近一次用户输入的时间，对应 Claude Code 的 `lastInteractionTime`。
#[derive(Clone, Debug, Default)]
pub struct UserActivity {
    last_interaction: Arc<Mutex<Option<Instant>>>,
}

impl UserActivity {
    /// 进程内共享的实例：终端在按键与鼠标操作时记录，后台维护读取。
    pub fn process() -> &'static UserActivity {
        static PROCESS_ACTIVITY: OnceLock<UserActivity> = OnceLock::new();
        PROCESS_ACTIVITY.get_or_init(UserActivity::default)
    }

    pub fn record(&self) {
        *self
            .last_interaction
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Instant::now());
    }

    fn is_recent(&self) -> bool {
        self.last_interaction
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_some_and(|last| last.elapsed() < RECENT_INTERACTION_WINDOW)
    }
}

/// 照 Claude Code 的计时起点：交互 REPL 在第一次提交输入（`REPL.tsx` 中 `submitCount === 1`）时启动后台维护，
/// 非交互模式在启动时启动。恢复的旧 Chat 在提交输入前不写 Transcript，从进程启动计时可能在用户开口前就
/// 把它当作过期记录删除。丢弃时中止尚未开始的清理。
pub struct HousekeepingSchedule {
    root: PathBuf,
    activity: UserActivity,
    deletion: Arc<dyn ChatDeletionNotice>,
    task: Option<JoinHandle<CleanupOutcome>>,
    is_started: bool,
}

impl HousekeepingSchedule {
    pub fn new(
        root: PathBuf,
        activity: UserActivity,
        deletion: Arc<dyn ChatDeletionNotice>,
    ) -> Self {
        Self {
            root,
            activity,
            deletion,
            task: None,
            is_started: false,
        }
    }

    /// 开始计时；每个进程只清理一次，之后的调用什么也不做。
    pub fn start(&mut self) {
        if self.is_started {
            return;
        }
        self.is_started = true;
        self.task = Some(start_background_housekeeping(
            self.root.clone(),
            self.activity.clone(),
            self.deletion.clone(),
        ));
    }

    pub fn is_started(&self) -> bool {
        self.is_started
    }

    /// 等待清理结束；尚未开始计时时返回 `None`。
    pub async fn finish(mut self) -> Option<CleanupOutcome> {
        let task = self.task.take()?;
        task.await.ok()
    }
}

impl Drop for HousekeepingSchedule {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

/// 照 Claude Code `startBackgroundHousekeeping`：在当前 Tokio 运行时中启动后台清理，每个进程只清理一次。
/// 返回的任务在清理结束后给出结果；中止任务不会打断已经开始的清理线程。
pub fn start_background_housekeeping(
    root: PathBuf,
    activity: UserActivity,
    deletion: Arc<dyn ChatDeletionNotice>,
) -> JoinHandle<CleanupOutcome> {
    tokio::spawn(async move {
        tokio::time::sleep(SLOW_HOUSEKEEPING_DELAY).await;
        while activity.is_recent() {
            tokio::time::sleep(SLOW_HOUSEKEEPING_DELAY).await;
        }
        let (sender, receiver) = oneshot::channel();
        let spawned = std::thread::Builder::new()
            .name("agent-cleanup".into())
            .spawn(move || {
                let _ = sender.send(cleanup_old_message_files_in_background(
                    &root,
                    deletion.as_ref(),
                ));
            });
        let failed = CleanupOutcome::Completed(CleanupResult {
            messages: 0,
            errors: 1,
        });
        match spawned {
            Ok(_) => receiver.await.unwrap_or(failed),
            Err(_) => failed,
        }
    })
}

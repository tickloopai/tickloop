mod support;

use std::{
    ffi::CString,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use support::{ScriptedModel, config, next_event, text};
use tickloop_agent::{
    cleanup::{ChatDeletionNotice, CleanupOutcome, cleanup_old_message_files_in_background},
    housekeeping::{
        HousekeepingSchedule, SLOW_HOUSEKEEPING_DELAY, UserActivity, start_background_housekeeping,
    },
    runtime::{Agent, AgentError, EventKind},
};
use uuid::Uuid;

const DAY: Duration = Duration::from_secs(24 * 60 * 60);
const EXPIRED: Duration = Duration::from_secs(31 * 24 * 60 * 60);
const RECENT: Duration = DAY;

/// 创建文件（含父目录）并把修改时间设为 `age` 之前。
fn file(path: &Path, age: Duration) -> PathBuf {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, "content").unwrap();
    set_age(path, age);
    path.to_path_buf()
}

/// 用 `lutimes` 设置修改时间：符号链接改的是链接本身，目录要在其内容创建之后再设置。
fn set_age(path: &Path, age: Duration) {
    let seconds = (SystemTime::now() - age)
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as libc::time_t;
    let time = libc::timeval {
        tv_sec: seconds,
        tv_usec: 0,
    };
    let path = CString::new(path.as_os_str().as_bytes()).unwrap();
    // SAFETY: path 是以 NUL 结尾的字符串，times 指向两个有效的 timeval。
    let status = unsafe { libc::lutimes(path.as_ptr(), [time, time].as_ptr()) };
    assert_eq!(status, 0, "{}", std::io::Error::last_os_error());
}

fn symlink(target: &Path, link: &Path, age: Duration) {
    std::fs::create_dir_all(link.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(target, link).unwrap();
    set_age(link, age);
}

/// 与 Tick Loop 写出的文件名形式相同的 Chat ID：连字符分隔的小写 UUID，后接 `suffix`。
fn uuid_name(suffix: &str) -> String {
    format!("{}{suffix}", Uuid::new_v4())
}

fn exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

#[derive(Debug, PartialEq)]
enum Notice {
    Begin {
        chat: Uuid,
        transcript_present: bool,
    },
    Finish {
        chat: Uuid,
        transcript_present: bool,
    },
}

/// Stands in for the Agent daemon: records what it is told and whether the Transcript was still
/// there at that moment; refuses deletions while `is_unavailable`.
#[derive(Default)]
struct DaemonNotices {
    root: PathBuf,
    is_unavailable: bool,
    notices: Mutex<Vec<Notice>>,
}

impl DaemonNotices {
    fn of(root: &Path) -> Self {
        Self {
            root: root.into(),
            ..Self::default()
        }
    }

    fn transcript_present(&self, chat: Uuid) -> bool {
        std::fs::read_dir(self.root.join("projects"))
            .into_iter()
            .flatten()
            .any(|project| exists(&project.unwrap().path().join(format!("{chat}.jsonl"))))
    }

    fn taken(&self) -> Vec<Notice> {
        std::mem::take(&mut self.notices.lock().unwrap())
    }
}

impl ChatDeletionNotice for DaemonNotices {
    fn begin(&self, chat: Uuid) -> Result<Uuid, String> {
        if self.is_unavailable {
            return Err("the Agent daemon did not answer".into());
        }
        let transcript_present = self.transcript_present(chat);
        self.notices.lock().unwrap().push(Notice::Begin {
            chat,
            transcript_present,
        });
        Ok(Uuid::new_v4())
    }

    fn finish(&self, chat: Uuid, _operation: Uuid) {
        let transcript_present = self.transcript_present(chat);
        self.notices.lock().unwrap().push(Notice::Finish {
            chat,
            transcript_present,
        });
    }
}

fn cleanup(root: &Path) -> CleanupOutcome {
    cleanup_old_message_files_in_background(root, &DaemonNotices::of(root))
}

fn completed(outcome: CleanupOutcome) -> tickloop_agent::cleanup::CleanupResult {
    match outcome {
        CleanupOutcome::Completed(result) => result,
        CleanupOutcome::Skipped => panic!("cleanup was skipped"),
    }
}

/// Removing a whole Transcript ends its Chat, so the daemon hears of it while the Transcript is
/// still there and again after the unlink; nothing else a cleanup removes is announced. Without
/// the daemon's answer the Transcript stays.
#[test]
fn the_daemon_hears_of_each_removed_transcript_before_and_after_the_unlink() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let project = root.join("projects/-work");
    let expired_chat = Uuid::new_v4();
    let expired = file(&project.join(format!("{expired_chat}.jsonl")), EXPIRED);
    let expired_watermark = file(
        &project.join(format!(
            "{expired_chat}/tick-watermarks/{}.json",
            Uuid::new_v4()
        )),
        RECENT,
    );
    let recent_chat = Uuid::new_v4();
    let recent = file(&project.join(format!("{recent_chat}.jsonl")), RECENT);
    let recent_watermark = file(
        &project.join(format!(
            "{recent_chat}/tick-watermarks/{}.json",
            Uuid::new_v4()
        )),
        RECENT,
    );
    let tools_only_chat = Uuid::new_v4();
    file(&project.join(format!("{tools_only_chat}.jsonl")), RECENT);
    let old_tool_result = file(
        &project.join(format!("{tools_only_chat}/tool-results/call.txt")),
        EXPIRED,
    );
    let refused_chat = Uuid::new_v4();
    let refused = file(&project.join(format!("{refused_chat}.jsonl")), EXPIRED);
    let unlink_fails_chat = Uuid::new_v4();
    let locked_project = root.join("projects/-locked");
    let unlink_fails = file(
        &locked_project.join(format!("{unlink_fails_chat}.jsonl")),
        EXPIRED,
    );
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&locked_project, std::fs::Permissions::from_mode(0o500)).unwrap();
    std::fs::rename(&refused, root.join("held.jsonl")).unwrap();

    let daemon = DaemonNotices::of(root);
    let result = completed(cleanup_old_message_files_in_background(root, &daemon));
    std::fs::set_permissions(&locked_project, std::fs::Permissions::from_mode(0o700)).unwrap();

    assert!(!exists(&expired) && exists(&recent) && !exists(&old_tool_result));
    assert!(
        !exists(&project.join(expired_chat.to_string())),
        "an ended Chat keeps no Tick watermarks"
    );
    assert!(!exists(&expired_watermark) && exists(&recent_watermark));
    assert!(exists(&unlink_fails));
    assert_eq!(result.errors, 1, "the failed unlink is reported");
    let mut notices = daemon.taken();
    notices.sort_by_key(|notice| format!("{notice:?}"));
    let mut expected = vec![
        Notice::Begin {
            chat: expired_chat,
            transcript_present: true,
        },
        Notice::Finish {
            chat: expired_chat,
            transcript_present: false,
        },
        Notice::Begin {
            chat: unlink_fails_chat,
            transcript_present: true,
        },
        Notice::Finish {
            chat: unlink_fails_chat,
            transcript_present: true,
        },
    ];
    expected.sort_by_key(|notice| format!("{notice:?}"));
    assert_eq!(notices, expected);

    std::fs::rename(root.join("held.jsonl"), &refused).unwrap();
    let unavailable = DaemonNotices {
        is_unavailable: true,
        ..DaemonNotices::of(root)
    };
    let result = completed(cleanup_old_message_files_in_background(root, &unavailable));
    assert!(
        exists(&refused) && exists(&unlink_fails),
        "an unanswered deletion keeps the Transcript"
    );
    assert_eq!(result.errors, 2);
}

/// 照 Claude Code `cleanupOldMessageFilesInBackground`：按修改时间删除过期的 Transcript、Chat 目录中的
/// 工具结果、计划文件、文件回滚备份目录与粘贴缓存，之后删除空目录；其他文件与未过期的文件保留。
#[test]
fn only_expired_files_are_removed_and_emptied_directories_are_deleted() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let projects = root.join("projects");

    let project = projects.join("-work-project");
    let expired_transcript = file(&project.join(uuid_name(".jsonl")), EXPIRED);
    let recent_transcript = file(&project.join(uuid_name(".jsonl")), RECENT);
    let other_file = file(&project.join("notes.txt"), EXPIRED);
    let emptied_session = project.join(uuid_name(""));
    file(&emptied_session.join("tool-results/call.txt"), EXPIRED);
    file(
        &emptied_session.join("tool-results/nested/call.json"),
        EXPIRED,
    );
    let kept_session = project.join(uuid_name(""));
    let expired_result = file(&kept_session.join("tool-results/old.txt"), EXPIRED);
    let recent_result = file(&kept_session.join("tool-results/new.txt"), RECENT);
    let review_session = project.join(uuid_name(""));
    let review = file(&review_session.join("file-changes/review.json"), EXPIRED);
    let empty_session = project.join(uuid_name(""));
    std::fs::create_dir_all(&empty_session).unwrap();

    let emptied_project = projects.join("-work-emptied");
    file(&emptied_project.join(uuid_name(".jsonl")), EXPIRED);

    let plans = root.join("plans");
    let expired_plan = file(&plans.join("calm-river-loop-1.md"), EXPIRED);
    let recent_plan = file(&plans.join("bold-stone-loop-2.md"), RECENT);
    let plan_notes = file(&plans.join("notes.txt"), EXPIRED);

    let file_history = root.join("file-history");
    let expired_backups = file_history.join(uuid_name(""));
    file(&expired_backups.join("0123456789abcdef@v1"), RECENT);
    set_age(&expired_backups, EXPIRED);
    let recent_backups = file_history.join(uuid_name(""));
    file(&recent_backups.join("0123456789abcdef@v1"), EXPIRED);
    set_age(&recent_backups, RECENT);

    let pastes = root.join("paste-cache");
    let expired_paste = file(&pastes.join("aaaaaaaaaaaaaaaa.txt"), EXPIRED);
    let recent_paste = file(&pastes.join("bbbbbbbbbbbbbbbb.txt"), RECENT);

    let history = file(&root.join("history.jsonl"), EXPIRED);
    let registration = file(&root.join("sessions/12345.json"), EXPIRED);
    let settings_lock = file(&root.join("settings.json.lock"), EXPIRED);

    let result = completed(cleanup(root));

    for removed in [
        &expired_transcript,
        &emptied_session,
        &expired_result,
        &empty_session,
        &emptied_project,
        &expired_plan,
        &expired_backups,
        &expired_paste,
    ] {
        assert!(!exists(removed), "{} was not removed", removed.display());
    }
    for kept in [
        &recent_transcript,
        &other_file,
        &recent_result,
        &review,
        &recent_plan,
        &plan_notes,
        &recent_backups.join("0123456789abcdef@v1"),
        &recent_paste,
        &history,
        &registration,
        &settings_lock,
    ] {
        assert!(exists(kept), "{} was removed", kept.display());
    }
    assert_eq!(result.errors, 0);
    assert_eq!(result.messages, 8);

    // 目录清空后删除；粘贴缓存目录照 Claude Code 保留。
    std::fs::remove_file(&recent_plan).unwrap();
    std::fs::remove_file(&plan_notes).unwrap();
    set_age(&recent_backups, EXPIRED);
    std::fs::remove_file(&recent_paste).unwrap();
    completed(cleanup(root));
    assert!(!exists(&plans));
    assert!(!exists(&file_history));
    assert!(exists(&pastes));
}

/// 清理天数从设置文件读取：7 天时，10 天前的文件过期，5 天前的保留。
#[test]
fn the_cleanup_period_comes_from_the_settings_file() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    std::fs::write(root.join("settings.json"), r#"{"cleanupPeriodDays": 7}"#).unwrap();
    let expired = file(
        &root.join("projects/-work").join(uuid_name(".jsonl")),
        10 * DAY,
    );
    let kept = file(
        &root.join("projects/-work").join(uuid_name(".jsonl")),
        5 * DAY,
    );

    completed(cleanup(root));

    assert!(!exists(&expired));
    assert!(exists(&kept));
}

/// 符号链接一律跳过：链接本身和它指向的文件或目录都不删除，也不进入链接指向的目录清理。
#[test]
fn symbolic_links_are_skipped_and_never_followed() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let outside = directory.path().join("outside");
    let outside_transcript = file(&outside.join("project").join(uuid_name(".jsonl")), EXPIRED);
    let outside_result = file(&outside.join("results/call.txt"), EXPIRED);
    let outside_plan = file(&outside.join("plans/plan.md"), EXPIRED);
    let outside_backup = file(&outside.join("backups/0123456789abcdef@v1"), EXPIRED);
    set_age(&outside.join("backups"), EXPIRED);
    let outside_paste = file(&outside.join("paste.txt"), EXPIRED);

    let project = root.join("projects/-work");
    let linked_transcript = project.join(uuid_name(".jsonl"));
    symlink(&outside_transcript, &linked_transcript, EXPIRED);
    let linked_results = project.join(uuid_name("")).join("tool-results");
    symlink(&outside.join("results"), &linked_results, EXPIRED);
    let linked_project = root.join("projects/-linked");
    symlink(&outside.join("project"), &linked_project, EXPIRED);
    let linked_plans = root.join("plans");
    symlink(&outside.join("plans"), &linked_plans, EXPIRED);
    let linked_backups = root.join("file-history").join(uuid_name(""));
    symlink(&outside.join("backups"), &linked_backups, EXPIRED);
    let linked_paste = root.join("paste-cache/cccccccccccccccc.txt");
    symlink(&outside_paste, &linked_paste, EXPIRED);

    completed(cleanup(&root));

    for kept in [
        &outside_transcript,
        &outside_result,
        &outside_plan,
        &outside_backup,
        &outside_paste,
        &linked_transcript,
        &linked_results,
        &linked_project,
        &linked_plans,
        &linked_backups,
        &linked_paste,
    ] {
        assert!(exists(kept), "{} was removed", kept.display());
    }
}

/// 照 Claude Code：设置文件校验失败且显式配置了清理天数时，不回退到默认天数，整次清理跳过。
#[test]
fn invalid_settings_with_an_explicit_cleanup_period_skip_cleanup() {
    for (settings, skipped) in [
        (
            r#"{"cleanupPeriodDays": 90, "contextWindowTokens": "large"}"#,
            true,
        ),
        (r#"{"cleanupPeriodDays": -1}"#, true),
        (r#"{"cleanupPeriodDays": "90"}"#, true),
        (
            r#"{"cleanupPeriodDays": 90, "projects": {"/work": {"permissions": {"rules": [{"tool": ""}]}}}}"#,
            true,
        ),
        ("{\"cleanupPeriodDays\": 90,}", true),
        (r#"{"contextWindowTokens": "large"}"#, false),
        ("{\"model\": 1,}", false),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        std::fs::write(root.join("settings.json"), settings).unwrap();
        let transcript = file(
            &root.join("projects/-work").join(uuid_name(".jsonl")),
            EXPIRED,
        );

        let outcome = cleanup(root);

        assert_eq!(
            matches!(outcome, CleanupOutcome::Skipped),
            skipped,
            "{settings}"
        );
        assert_eq!(exists(&transcript), skipped, "{settings}");
    }
}

/// 清理天数无效时，与其他设置问题一样通过 `user_settings_warnings` 报告。
#[tokio::test]
async fn an_invalid_cleanup_period_is_reported_as_a_settings_warning() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let workdir = directory.path().join("work");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&workdir).unwrap();
    std::fs::write(root.join("settings.json"), r#"{"cleanupPeriodDays": -1}"#).unwrap();
    let agent =
        Agent::open_embedded(root, config(), Arc::new(ScriptedModel::new(Vec::new()))).unwrap();

    let chat = agent.create_chat(workdir).await.unwrap();

    let warnings = agent.user_settings_warnings(chat);
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("cleanupPeriodDays"), "{warnings:?}");
    assert!(agent.saves_chats());
}

/// 照 Claude Code `shouldSkipPersistence`：清理天数为 0 时对话不产生 Transcript，但照常读写用户设置；
/// 之后的清理把修改时间早于现在的记录全部删除。
#[tokio::test]
async fn a_zero_cleanup_period_writes_no_transcript() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let workdir = directory.path().join("work");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&workdir).unwrap();
    std::fs::write(root.join("settings.json"), r#"{"cleanupPeriodDays": 0}"#).unwrap();
    let earlier_transcript = file(
        &root.join("projects/-earlier").join(uuid_name(".jsonl")),
        Duration::from_secs(60),
    );
    let agent = Agent::open_embedded(
        root.clone(),
        config(),
        Arc::new(ScriptedModel::new(vec![text("Not saved.")])),
    )
    .unwrap();
    assert!(!agent.saves_chats());

    let chat = agent.create_chat(workdir.clone()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, None, "Keep this private".into())
        .await
        .unwrap();
    loop {
        match next_event(&mut events).await.kind {
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("turn failed: {error}"),
            _ => {}
        }
    }
    tickloop_agent::settings::UserSettings::in_root(&root)
        .save_thinking_enabled(false)
        .unwrap();
    agent.flush_transcripts().await;
    agent.shutdown().await;

    let project_entries = std::fs::read_dir(root.join("projects"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    assert_eq!(project_entries, vec![std::ffi::OsString::from("-earlier")]);
    let settings: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("settings.json")).unwrap()).unwrap();
    assert_eq!(settings["alwaysThinkingEnabled"], false);
    assert!(settings.get("contextWindowTokens").is_none());
    assert_eq!(settings["cleanupPeriodDays"], 0);
    assert!(agent.saved_chats(&workdir).await.unwrap().is_empty());
    let reopened = Agent::open_embedded(
        root.clone(),
        config(),
        Arc::new(ScriptedModel::new(Vec::new())),
    )
    .unwrap();
    assert!(matches!(
        reopened.resume_chat(chat).await,
        Err(AgentError::NotFound)
    ));

    completed(cleanup(&root));
    assert!(!exists(&earlier_transcript));
    assert!(!exists(&root.join("projects/-earlier")));
}

/// 照 Claude Code `startBackgroundHousekeeping`：启动 10 分钟后清理；那时最近一分钟内有用户输入就再推迟
/// 10 分钟，直到用户一分钟没有输入。
#[tokio::test(start_paused = true)]
async fn recent_user_input_defers_background_cleanup() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().to_path_buf();
    let transcript = file(
        &root.join("projects/-work").join(uuid_name(".jsonl")),
        EXPIRED,
    );
    let activity = UserActivity::default();
    let housekeeping = start_background_housekeeping(
        root.clone(),
        activity.clone(),
        Arc::new(DaemonNotices::of(&root)),
    );
    let delay = SLOW_HOUSEKEEPING_DELAY;
    // 先让后台任务开始计时，再推进时钟。
    settle().await;

    tokio::time::advance(delay - Duration::from_secs(30)).await;
    activity.record();
    settle().await;
    assert!(exists(&transcript), "cleanup ran before the startup delay");

    tokio::time::advance(Duration::from_secs(30)).await;
    settle().await;
    assert!(!housekeeping.is_finished());
    assert!(exists(&transcript), "cleanup ran despite recent input");

    tokio::time::advance(delay - Duration::from_secs(1)).await;
    settle().await;
    assert!(exists(&transcript), "the deferred cleanup ran early");

    // 暂停的时钟会在等待清理线程时自动推进，因此不套超时。
    let outcome = housekeeping.await.unwrap();
    assert!(matches!(outcome, CleanupOutcome::Completed(_)));
    assert!(!exists(&transcript));
}

/// 照 Claude Code `REPL.tsx`（`submitCount === 1` 时 `startBackgroundHousekeeping`）：交互模式在第一次提交输入
/// 时才开始计时，恢复旧 Chat 后长时间不提交也不会清理；之后照常等待 10 分钟。
#[tokio::test(start_paused = true)]
async fn interactive_housekeeping_starts_counting_at_the_first_submitted_prompt() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().to_path_buf();
    let transcript = file(
        &root.join("projects/-work").join(uuid_name(".jsonl")),
        EXPIRED,
    );
    let mut schedule = HousekeepingSchedule::new(
        root.clone(),
        UserActivity::default(),
        Arc::new(DaemonNotices::of(&root)),
    );
    let delay = SLOW_HOUSEKEEPING_DELAY;
    settle().await;

    tokio::time::advance(3 * delay).await;
    settle().await;
    assert!(!schedule.is_started());
    assert!(exists(&transcript), "cleanup ran before the first prompt");

    schedule.start();
    settle().await;
    tokio::time::advance(delay - Duration::from_secs(1)).await;
    schedule.start();
    settle().await;
    assert!(
        exists(&transcript),
        "cleanup ran before the delay after the first prompt"
    );

    tokio::time::advance(Duration::from_secs(1)).await;
    let outcome = schedule.finish().await;
    assert!(matches!(outcome, Some(CleanupOutcome::Completed(_))));
    assert!(!exists(&transcript));
}

/// External removal must not recreate a disconnected Transcript or a metadata-only shell.
#[tokio::test]
async fn externally_removing_the_transcript_of_a_running_chat_stops_saving_it() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let workdir = directory.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    let agent = Agent::open_embedded(
        root.clone(),
        config(),
        Arc::new(ScriptedModel::new(vec![
            text("saved answer"),
            text("unsaved answer"),
        ])),
    )
    .unwrap();
    let chat = agent.create_chat(workdir.clone()).await.unwrap();
    complete(&agent, chat, "saved prompt").await;
    agent.flush_transcripts().await;
    let transcript = support::transcript_file(&root, chat);

    std::fs::remove_file(&transcript).unwrap();
    assert!(!exists(&transcript));

    complete(&agent, chat, "prompt after cleanup").await;
    agent.flush_transcripts().await;
    let error = agent
        .transcript_write_error()
        .expect("the deleted Transcript is reported");
    assert!(error.contains("was deleted"), "{error}");
    agent.shutdown().await;
    assert!(!exists(&transcript));
    let transcripts = std::fs::read_dir(transcript.parent().unwrap())
        .into_iter()
        .flatten()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
        })
        .collect::<Vec<_>>();
    assert!(transcripts.is_empty(), "{transcripts:?}");
}

#[tokio::test]
async fn live_runtime_retains_expired_records_until_all_holders_shut_down() {
    use std::io::Write;

    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let workdir = directory.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    let agent = Agent::open_embedded(
        root.clone(),
        config(),
        Arc::new(ScriptedModel::new(vec![text("A live background task.")])),
    )
    .unwrap();
    let chat = agent.create_chat(workdir).await.unwrap();
    complete(&agent, chat, "Keep this execution available.").await;
    agent.flush_transcripts().await;
    let transcript = support::transcript_file(&root, chat);
    writeln!(
        std::fs::OpenOptions::new()
            .append(true)
            .open(&transcript)
            .unwrap(),
        "{}",
        slug_line("calm-river")
    )
    .unwrap();
    let tool_result = file(
        &transcript
            .parent()
            .unwrap()
            .join(chat.to_string())
            .join("tool-results/old.txt"),
        EXPIRED,
    );
    let image = file(&root.join(format!("image-cache/{chat}/old.png")), EXPIRED);
    let backup = file(&root.join(format!("file-history/{chat}/old@v1")), EXPIRED);
    let plan = file(
        &root.join(format!("plans/calm-river-loop-{}.md", Uuid::new_v4())),
        EXPIRED,
    );
    set_age(image.parent().unwrap(), EXPIRED);
    set_age(backup.parent().unwrap(), EXPIRED);
    set_age(&transcript, EXPIRED);
    let kept = [&transcript, &tool_result, &image, &backup, &plan];
    completed(cleanup(&root));
    for path in kept {
        assert!(exists(path), "live record removed: {}", path.display());
    }

    let second =
        Agent::open_embedded(root.clone(), config(), Arc::new(ScriptedModel::new(vec![]))).unwrap();
    second.resume_chat(chat).await.unwrap();
    agent.shutdown().await;
    set_age(&transcript, EXPIRED);
    completed(cleanup(&root));
    for path in kept {
        assert!(exists(path), "independent holder lost: {}", path.display());
    }
    second.shutdown().await;
    set_age(&transcript, EXPIRED);
    completed(cleanup(&root));
    for path in kept {
        assert!(
            !exists(path),
            "expired record retained after release: {}",
            path.display()
        );
    }
}

#[test]
fn an_unusable_retention_lock_preserves_records_without_following_links() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let chat = Uuid::new_v4();
    let transcript = file(&root.join(format!("projects/-work/{chat}.jsonl")), EXPIRED);
    let image = file(&root.join(format!("image-cache/{chat}/old.png")), EXPIRED);
    set_age(image.parent().unwrap(), EXPIRED);
    let outside = file(&directory.path().join("outside.txt"), EXPIRED);
    let lock = root.join(format!("chat-retention/{chat}.lock"));
    symlink(&outside, &lock, EXPIRED);
    std::fs::set_permissions(
        lock.parent().unwrap(),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    completed(cleanup(&root));
    for path in [&transcript, &image, &lock, &outside] {
        assert!(
            exists(path),
            "unconfirmed retention removed {}",
            path.display()
        );
    }
    assert_eq!(std::fs::read_to_string(outside).unwrap(), "content");
}

#[tokio::test]
async fn exiting_a_worker_releases_retention_without_a_pid_liveness_check() {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};

    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let agent = Agent::open_embedded(
        root.clone(),
        config(),
        Arc::new(ScriptedModel::new(vec![text("A recorded task.")])),
    )
    .unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    complete(&agent, chat, "Record this task.").await;
    agent.shutdown().await;
    let transcript = support::transcript_file(&root, chat);
    let mut worker = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "retention_worker_fixture",
            "--ignored",
            "--nocapture",
        ])
        .env("TICKLOOP_RETENTION_TEST_ROOT", &root)
        .env("TICKLOOP_RETENTION_TEST_CHAT", chat.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut output = BufReader::new(worker.stdout.take().unwrap());
    let mut line = String::new();
    loop {
        line.clear();
        assert!(
            output.read_line(&mut line).unwrap() > 0,
            "worker did not retain the Chat"
        );
        if line.trim() == "RETENTION_READY" {
            break;
        }
    }
    set_age(&transcript, EXPIRED);
    completed(cleanup(&root));
    let retained = exists(&transcript);
    drop(worker.stdin.take());
    assert!(worker.wait().unwrap().success());
    assert!(retained);
    completed(cleanup(&root));
    assert!(!exists(&transcript));
}

#[tokio::test]
#[ignore = "subprocess fixture invoked by the retention lifecycle test"]
async fn retention_worker_fixture() {
    use std::io::Write;
    let root = PathBuf::from(std::env::var_os("TICKLOOP_RETENTION_TEST_ROOT").unwrap());
    let chat = Uuid::parse_str(&std::env::var("TICKLOOP_RETENTION_TEST_CHAT").unwrap()).unwrap();
    let agent = Agent::open_embedded(root, config(), Arc::new(ScriptedModel::new(vec![]))).unwrap();
    agent.resume_chat(chat).await.unwrap();
    agent.flush_transcripts().await;
    println!("RETENTION_READY");
    std::io::stdout().flush().unwrap();
    let mut input = String::new();
    assert_eq!(std::io::stdin().read_line(&mut input).unwrap(), 0);
    std::process::exit(0);
}

/// 文件回滚备份目录与计划文件不随对话更新修改时间：Transcript 仍未过期的 Chat 保留其备份目录，
/// 计划文件名的 slug 出现在未过期 Transcript 中时保留；没有未过期 Transcript 的过期项照常删除。
#[test]
fn backups_and_plans_of_chats_with_recent_transcripts_are_kept() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let project = root.join("projects/-work");
    let padding = format!(
        r#"{{"type":"user","message":"{}"}}"#,
        "x".repeat(100 * 1024)
    );

    let tail_slug_chat = Uuid::new_v4();
    file_with(
        &project.join(format!("{tail_slug_chat}.jsonl")),
        &format!("{padding}\n{}\n", slug_line("calm-river")),
        RECENT,
    );
    let middle_slug_chat = Uuid::new_v4();
    file_with(
        &project.join(format!("{middle_slug_chat}.jsonl")),
        &format!("{padding}\n{}\n{padding}\n", slug_line("bold-stone")),
        RECENT,
    );
    let expired_chat = Uuid::new_v4();
    file_with(
        &project.join(format!("{expired_chat}.jsonl")),
        &format!("{}\n", slug_line("old-leaf")),
        EXPIRED,
    );
    let chat_without_transcript = Uuid::new_v4();

    let backups = |chat: Uuid| {
        let backups = root.join("file-history").join(chat.to_string());
        file(&backups.join("0123456789abcdef@v1"), EXPIRED);
        set_age(&backups, EXPIRED);
        backups
    };
    let plan = |slug: &str| {
        file(
            &root
                .join("plans")
                .join(format!("{slug}-loop-{}.md", Uuid::new_v4())),
            EXPIRED,
        )
    };
    let kept = [
        backups(tail_slug_chat),
        backups(middle_slug_chat),
        plan("calm-river"),
        plan("bold-stone"),
    ];
    let removed = [
        backups(expired_chat),
        backups(chat_without_transcript),
        plan("old-leaf"),
        plan("lone-cloud"),
    ];

    let result = completed(cleanup(root));

    for path in &kept {
        assert!(exists(path), "{} was removed", path.display());
    }
    for path in &removed {
        assert!(!exists(path), "{} was not removed", path.display());
    }
    assert_eq!(result.errors, 0);
}

/// 图片缓存照文件回滚备份清理：按目录本身的修改时间删除整个 `<root>/image-cache/<chatId>/`，
/// Transcript 仍未过期的 Chat 保留，不是 UUID 命名的目录不动，清空后删除 `image-cache/`。
/// 不按进程删除（偏离 Claude Code），其他进程中运行的 Chat 仍有未过期的 Transcript。
#[test]
fn image_caches_expire_with_the_transcripts_of_their_chats() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let project = root.join("projects/-work");
    let chat_with_recent_transcript = Uuid::new_v4();
    file(
        &project.join(format!("{chat_with_recent_transcript}.jsonl")),
        RECENT,
    );
    let chat_with_expired_transcript = Uuid::new_v4();
    file(
        &project.join(format!("{chat_with_expired_transcript}.jsonl")),
        EXPIRED,
    );
    let image_cache = root.join("image-cache");
    let cached_images = |name: String, age: Duration| {
        let chat_directory = image_cache.join(name);
        file(&chat_directory.join("1.png"), EXPIRED);
        set_age(&chat_directory, age);
        chat_directory
    };
    let kept = [
        cached_images(chat_with_recent_transcript.to_string(), EXPIRED),
        cached_images(uuid_name(""), RECENT),
        cached_images("Pictures".into(), EXPIRED),
    ];
    let removed = [
        cached_images(chat_with_expired_transcript.to_string(), EXPIRED),
        cached_images(uuid_name(""), EXPIRED),
    ];

    let result = completed(cleanup(root));

    for path in &kept {
        assert!(
            exists(&path.join("1.png")),
            "{} was removed",
            path.display()
        );
    }
    for path in &removed {
        assert!(!exists(path), "{} was not removed", path.display());
    }
    assert_eq!(result.errors, 0);
    assert_eq!(result.messages, 3);

    for path in &kept {
        std::fs::remove_dir_all(path).unwrap();
    }
    completed(cleanup(root));
    assert!(!exists(&image_cache));
}

/// 只清理 Tick Loop 自己写出的名称：`<uuid>.jsonl` 的 Transcript、UUID 命名的Chat 目录与文件回滚备份目录。
/// 配置目录被误设为主目录这类情况下，同名目录中的用户文件不被删除。
#[test]
fn only_uuid_named_transcripts_session_directories_and_backups_are_cleaned() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let project = root.join("projects/-work");
    let chat = Uuid::new_v4();
    let transcript = file(&project.join(format!("{chat}.jsonl")), EXPIRED);
    let session_result = file(
        &project.join(format!("{chat}/tool-results/call.txt")),
        EXPIRED,
    );
    let backups = root.join("file-history").join(chat.to_string());
    file(&backups.join("0123456789abcdef@v1"), EXPIRED);
    set_age(&backups, EXPIRED);

    let user_files = [
        file(&project.join("notes.jsonl"), EXPIRED),
        file(&project.join(format!("{}.jsonl", chat.simple())), EXPIRED),
        file(&project.join(format!("{}.jsonl", chat.braced())), EXPIRED),
        file(&project.join("Documents/tool-results/report.txt"), EXPIRED),
        {
            let pictures = root.join("file-history/Pictures");
            let photo = file(&pictures.join("photo.jpg"), EXPIRED);
            set_age(&pictures, EXPIRED);
            photo
        },
    ];

    completed(cleanup(root));

    for path in [&transcript, &session_result, &backups] {
        assert!(!exists(path), "{} was not removed", path.display());
    }
    for path in &user_files {
        assert!(exists(path), "{} was removed", path.display());
    }
}

/// 链上条目带 `slug` 字段的一行 Transcript。
fn slug_line(slug: &str) -> String {
    format!(
        r#"{{"type":"assistant","uuid":"{}","slug":"{slug}"}}"#,
        Uuid::new_v4()
    )
}

fn file_with(path: &Path, content: &str, age: Duration) -> PathBuf {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
    set_age(path, age);
    path.to_path_buf()
}

async fn complete(agent: &Agent, chat: tickloop_agent::runtime::ChatId, prompt: &str) {
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, None, prompt.into()).await.unwrap();
    loop {
        let event = next_event(&mut events).await;
        if event.run_id != receipt.run_id {
            continue;
        }
        match event.kind {
            EventKind::Completed { .. } => return,
            EventKind::Failed(error) => panic!("turn failed: {error}"),
            _ => {}
        }
    }
}

/// 让已到期的定时器与后台线程有机会运行，再检查文件。
async fn settle() {
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    std::thread::sleep(Duration::from_millis(100));
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
}

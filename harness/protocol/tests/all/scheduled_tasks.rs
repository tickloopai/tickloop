use serde_json::json;
use tickloop_harness_protocol::{
    ScheduledTask, ScheduledTaskConfirmation, ScheduledTaskCreator, ScheduledTaskKind,
    ScheduledTaskNotice, ScheduledTaskRemovalReason, ScheduledTaskRevocation,
    legacy_scheduled_task_id,
};
use uuid::Uuid;

const TASK: Uuid = Uuid::from_u128(0x0198_0a1b_2c3d_7000_8000_0000_0a1b_2c3d);
const CHAT: Uuid = Uuid::from_u128(0xc4a7);

fn task(kind: ScheduledTaskKind, confirmation: Option<ScheduledTaskConfirmation>) -> ScheduledTask {
    ScheduledTask {
        paused_at_ms: None,
        id: TASK,
        kind,
        title: None,
        prompt: "check the deploy".into(),
        creator: ScheduledTaskCreator::User,
        time_zone: "Asia/Shanghai".into(),
        confirmation,
        created_at_ms: 1_000,
        expires_at_ms: Some(605_801_000),
        ends_at_ms: None,
        scheduled_for_ms: Some(301_000),
        next_fire_at_ms: Some(286_000),
        jitter_ms: -15_000,
        last_fired_at_ms: None,
        last_scheduled_for_ms: None,
        started_by_run: None,
        skipped: 0,
    }
}

#[test]
fn scheduled_tasks_serialize_with_tagged_kinds() {
    let recurring = task(
        ScheduledTaskKind::Recurring {
            cron: "*/5 * * * *".into(),
        },
        None,
    );
    let value = serde_json::to_value(&recurring).unwrap();
    assert_eq!(
        value["kind"],
        json!({"type": "recurring", "cron": "*/5 * * * *"})
    );
    assert_eq!(value["creator"], "user");
    assert_eq!(value["confirmation"], json!(null));
    assert_eq!(
        serde_json::from_value::<ScheduledTask>(value).unwrap(),
        recurring
    );

    let self_paced = task(
        ScheduledTaskKind::SelfPaced {
            delay_seconds: Some(270),
            reason: Some("build still running".into()),
            fallback: false,
        },
        Some(ScheduledTaskConfirmation::Confirmed),
    );
    let value = serde_json::to_value(&self_paced).unwrap();
    assert_eq!(value["kind"]["type"], "self_paced");
    assert_eq!(value["confirmation"], "confirmed");
    assert_eq!(
        serde_json::from_value::<ScheduledTask>(value).unwrap(),
        self_paced
    );
}

#[test]
fn scheduled_tasks_time_only_when_confirmed_and_scheduled() {
    let one_shot = ScheduledTaskKind::OneShot {
        cron: "0 15 * * *".into(),
    };
    assert!(task(one_shot.clone(), None).is_timing());
    assert!(task(one_shot.clone(), Some(ScheduledTaskConfirmation::Confirmed)).is_timing());
    assert!(!task(one_shot.clone(), Some(ScheduledTaskConfirmation::Pending)).is_timing());
    let mut paused = task(one_shot.clone(), None);
    paused.paused_at_ms = Some(2_000);
    assert!(!paused.is_timing());
    let waiting = ScheduledTask {
        scheduled_for_ms: None,
        next_fire_at_ms: None,
        ..task(one_shot, None)
    };
    assert!(!waiting.is_timing());
}

#[test]
fn titles_and_ends_are_written_only_when_set() {
    let plain = task(
        ScheduledTaskKind::Recurring {
            cron: "0 9 * * *".into(),
        },
        None,
    );
    let value = serde_json::to_value(&plain).unwrap();
    assert_eq!(value["id"], json!(TASK));
    assert!(value.get("title").is_none() && value.get("ends_at_ms").is_none());

    let named = ScheduledTask {
        title: Some("Daily digest".into()),
        ends_at_ms: Some(900_000),
        ..plain
    };
    let value = serde_json::to_value(&named).unwrap();
    assert_eq!(value["title"], "Daily digest");
    assert_eq!(value["ends_at_ms"], 900_000);
    assert_eq!(
        serde_json::from_value::<ScheduledTask>(value).unwrap(),
        named
    );
}

/// A task recorded before tasks had UUIDs, titles and ends reads with the UUIDv5 of its Chat,
/// its eight-character id and its creation time, the same on every read; a task recorded with
/// a UUID keeps it.
#[test]
fn tasks_recorded_with_an_eight_character_id_read_with_a_stable_uuid() {
    let legacy = json!({
        "id": "0a1b2c3d", "kind": {"type": "recurring", "cron": "*/5 * * * *"},
        "prompt": "check the deploy", "creator": "user", "time_zone": "Asia/Shanghai",
        "confirmation": null, "created_at_ms": 1_000, "expires_at_ms": 605_801_000,
        "scheduled_for_ms": 301_000, "next_fire_at_ms": 286_000, "jitter_ms": -15_000,
        "last_fired_at_ms": null, "skipped": 0
    });
    let read = ScheduledTask::from_record(CHAT, legacy.clone()).unwrap();
    assert_eq!(read.id, legacy_scheduled_task_id(CHAT, "0a1b2c3d", 1_000));
    assert_eq!(read.id.get_version_num(), 5);
    assert_eq!(read.title, None);
    assert_eq!(read.ends_at_ms, None);
    assert_eq!(read.prompt, "check the deploy");
    assert_eq!(
        ScheduledTask::from_record(CHAT, legacy.clone()).unwrap(),
        read
    );
    assert_ne!(
        ScheduledTask::from_record(Uuid::from_u128(0xd00d), legacy.clone())
            .unwrap()
            .id,
        read.id,
        "the same short id in another Chat is another task"
    );
    let mut later = legacy;
    later["created_at_ms"] = json!(2_000);
    assert_ne!(ScheduledTask::from_record(CHAT, later).unwrap().id, read.id);

    let current = task(
        ScheduledTaskKind::OneShot {
            cron: "0 15 * * *".into(),
        },
        None,
    );
    assert_eq!(
        ScheduledTask::from_record(CHAT, serde_json::to_value(&current).unwrap()).unwrap(),
        current
    );
    assert!(serde_json::from_value::<ScheduledTask>(json!({"id": "0a1b2c3d"})).is_err());
}

#[test]
fn removals_name_the_task_and_why_it_left() {
    let removed = ScheduledTaskNotice::Removed {
        task_id: TASK,
        reason: ScheduledTaskRemovalReason::EndsAtReached,
    };
    let value = serde_json::to_value(&removed).unwrap();
    assert_eq!(
        value,
        json!({"kind": "removed", "task_id": TASK, "reason": "ends_at_reached"})
    );
    assert_eq!(
        serde_json::from_value::<ScheduledTaskNotice>(value).unwrap(),
        removed
    );
    for (reason, wire) in [
        (ScheduledTaskRemovalReason::UserDeleted, "user_deleted"),
        (ScheduledTaskRemovalReason::AgentDeleted, "agent_deleted"),
        (ScheduledTaskRemovalReason::Expired, "expired"),
        (ScheduledTaskRemovalReason::Rejected, "rejected"),
        (
            ScheduledTaskRemovalReason::OneShotCompleted,
            "one_shot_completed",
        ),
        (
            ScheduledTaskRemovalReason::SelfPacedEnded,
            "self_paced_ended",
        ),
    ] {
        assert_eq!(serde_json::to_value(reason).unwrap(), wire);
    }
}

#[test]
fn cancellations_recorded_with_an_eight_character_id_still_read() {
    let legacy =
        |revoked_at_ms: u64| json!({"task_id": "0a1b2c3d", "revoked_at_ms": revoked_at_ms});
    let first: ScheduledTaskRevocation = serde_json::from_value(legacy(1)).unwrap();
    let second: ScheduledTaskRevocation = serde_json::from_value(legacy(2)).unwrap();
    assert_eq!(first.task_id, second.task_id);
    let current = ScheduledTaskRevocation {
        task_id: TASK,
        revoked_at_ms: 3,
    };
    assert_eq!(
        serde_json::from_value::<ScheduledTaskRevocation>(serde_json::to_value(&current).unwrap())
            .unwrap(),
        current
    );
}

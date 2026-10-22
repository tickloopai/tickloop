use serde_json::json;
use tickloop_agent_protocol::{
    ScheduledTask, ScheduledTaskConfirmation, ScheduledTaskCreator, ScheduledTaskKind,
};

fn task(kind: ScheduledTaskKind, confirmation: Option<ScheduledTaskConfirmation>) -> ScheduledTask {
    ScheduledTask {
        id: "0a1b2c3d".into(),
        kind,
        prompt: "check the deploy".into(),
        creator: ScheduledTaskCreator::User,
        confirmation,
        created_at_ms: 1_000,
        expires_at_ms: Some(605_801_000),
        scheduled_for_ms: Some(301_000),
        next_fire_at_ms: Some(286_000),
        jitter_ms: -15_000,
        last_fired_at_ms: None,
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
    let waiting = ScheduledTask {
        scheduled_for_ms: None,
        next_fire_at_ms: None,
        ..task(one_shot, None)
    };
    assert!(!waiting.is_timing());
}

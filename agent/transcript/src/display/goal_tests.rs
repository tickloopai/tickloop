use super::*;
use crate::{GoalEntry, MetadataKind, RetainedSnapshotEntry, Retention};
use tickloop_agent_protocol::{GoalBlock, GoalNotice, GoalStatus};

fn snapshot(objective: &str, status: GoalStatus) -> GoalSnapshot {
    GoalSnapshot {
        id: Uuid::nil(),
        objective: objective.into(),
        objective_revision: 1,
        status,
        token_budget: None,
        tokens_used: 0,
        time_used_ms: 0,
        last_check: None,
        quiet_turns: 0,
        created_at_ms: 1,
        updated_at_ms: 1,
    }
}

fn goal_line(goal: Option<GoalSnapshot>) -> Line {
    Line::Goal(GoalEntry {
        chat_id: Uuid::nil(),
        cwd: None,
        goal,
    })
}

fn event_line(notice: GoalNotice) -> Line {
    Line::GoalEvent(GoalEventEntry {
        chat_id: Uuid::nil(),
        goal_id: Uuid::nil(),
        objective: "Ship it".into(),
        notice,
        timestamp: "2026-10-23T00:00:00Z".into(),
    })
}

#[test]
fn the_latest_goal_record_wins_and_a_cleared_goal_is_none() {
    let active = snapshot("Ship it", GoalStatus::Active);
    let complete = snapshot("Ship it", GoalStatus::Complete);
    assert_eq!(
        goal(&[goal_line(Some(active)), goal_line(Some(complete.clone()))]),
        Some(complete.clone())
    );
    assert_eq!(goal(&[goal_line(Some(complete)), goal_line(None)]), None);
    assert_eq!(goal(&[]), None);
}

#[test]
fn goal_events_keep_every_line_through_retained_snapshots() {
    let blocked = GoalNotice::Blocked {
        by: GoalBlock::NoProgress,
    };
    let lines = [
        Line::RetainedSnapshot(RetainedSnapshotEntry {
            chat_id: Uuid::nil(),
            lines: vec![
                event_line(GoalNotice::Set),
                goal_line(Some(snapshot("Ship it", GoalStatus::Active))),
            ],
        }),
        event_line(blocked.clone()),
    ];
    let notices = goal_events(&lines)
        .into_iter()
        .map(|event| event.notice)
        .collect::<Vec<_>>();
    assert_eq!(notices, [GoalNotice::Set, blocked]);
    assert!(goal(&lines).is_some());
}

#[test]
fn goal_records_are_kept_as_the_latest_and_goal_events_accumulate() {
    assert_eq!(MetadataKind::Goal.retention(), Retention::ChatLatest);
    assert_eq!(MetadataKind::GoalEvent.retention(), Retention::Accumulating);
    let line = event_line(GoalNotice::Cleared);
    let value = serde_json::to_value(&line).unwrap();
    assert_eq!(value["type"], "goal-event");
    assert_eq!(value["notice"], serde_json::json!({"kind": "cleared"}));
    assert!(matches!(
        serde_json::from_value::<Line>(value).unwrap(),
        Line::GoalEvent(_)
    ));
}

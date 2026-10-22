//! Wire shapes of a Chat's goal and the user's `/goal` actions.

use serde_json::json;
use tickloop_agent_protocol::{
    GoalAction, GoalBlock, GoalCheck, GoalPause, GoalSnapshot, GoalStatus, GoalVerdict,
};
use uuid::Uuid;

fn goal(status: GoalStatus) -> GoalSnapshot {
    GoalSnapshot {
        id: Uuid::nil(),
        objective: "Ship the export command".into(),
        objective_revision: 1,
        status,
        token_budget: None,
        tokens_used: 1_200,
        time_used_ms: 30_000,
        last_check: None,
        quiet_turns: 0,
        created_at_ms: 1,
        updated_at_ms: 2,
    }
}

#[test]
fn a_goal_snapshot_round_trips_and_omits_what_it_lacks() {
    let mut snapshot = goal(GoalStatus::Active);
    let value = serde_json::to_value(&snapshot).unwrap();
    assert_eq!(
        value,
        json!({
            "id": Uuid::nil(),
            "objective": "Ship the export command",
            "objective_revision": 1,
            "status": {"state": "active"},
            "tokens_used": 1_200,
            "time_used_ms": 30_000,
            "quiet_turns": 0,
            "created_at_ms": 1,
            "updated_at_ms": 2,
        })
    );
    assert_eq!(
        serde_json::from_value::<GoalSnapshot>(value).unwrap(),
        snapshot
    );

    snapshot.token_budget = Some(5_000);
    snapshot.last_check = Some(GoalCheck {
        verdict: GoalVerdict::NotMet {
            reason: "no test output".into(),
        },
        checked_at_ms: 3,
    });
    let value = serde_json::to_value(&snapshot).unwrap();
    assert_eq!(value["token_budget"], 5_000);
    assert_eq!(
        value["last_check"],
        json!({"verdict": {"kind": "not_met", "reason": "no test output"}, "checked_at_ms": 3})
    );
    assert_eq!(
        serde_json::from_value::<GoalSnapshot>(value).unwrap(),
        snapshot
    );
}

#[test]
fn statuses_carry_why_a_goal_paused_or_blocked() {
    for (status, wire) in [
        (GoalStatus::Active, json!({"state": "active"})),
        (
            GoalStatus::Paused {
                by: GoalPause::HistoryLoaded,
            },
            json!({"state": "paused", "by": "history_loaded"}),
        ),
        (
            GoalStatus::Blocked {
                by: GoalBlock::Reported {
                    reason: "no credentials".into(),
                },
            },
            json!({"state": "blocked", "by": {"kind": "reported", "reason": "no credentials"}}),
        ),
        (
            GoalStatus::Blocked {
                by: GoalBlock::NoProgress,
            },
            json!({"state": "blocked", "by": {"kind": "no_progress"}}),
        ),
        (
            GoalStatus::Blocked {
                by: GoalBlock::ExecutionFailed {
                    error: "provider unavailable".into(),
                },
            },
            json!({"state": "blocked", "by": {"kind": "execution_failed", "error": "provider unavailable"}}),
        ),
        (
            GoalStatus::Blocked {
                by: GoalBlock::CheckImpossible {
                    reason: "API removed".into(),
                },
            },
            json!({"state": "blocked", "by": {"kind": "check_impossible", "reason": "API removed"}}),
        ),
        (
            GoalStatus::BudgetLimited,
            json!({"state": "budget_limited"}),
        ),
        (GoalStatus::Complete, json!({"state": "complete"})),
    ] {
        assert_eq!(serde_json::to_value(&status).unwrap(), wire);
        assert_eq!(serde_json::from_value::<GoalStatus>(wire).unwrap(), status);
    }
    for (by, wire) in [
        (GoalPause::User, "user"),
        (GoalPause::Interrupted, "interrupted"),
        (GoalPause::Stopped, "stopped"),
    ] {
        assert_eq!(serde_json::to_value(by).unwrap(), wire);
    }
}

#[test]
fn only_a_complete_goal_is_finished() {
    assert!(GoalStatus::Complete.is_finished());
    for status in [
        GoalStatus::Active,
        GoalStatus::Paused {
            by: GoalPause::User,
        },
        GoalStatus::Blocked {
            by: GoalBlock::NoProgress,
        },
        GoalStatus::BudgetLimited,
    ] {
        assert!(!status.is_finished());
    }
}

#[test]
fn the_budget_is_exhausted_once_usage_reaches_it() {
    let mut snapshot = goal(GoalStatus::Active);
    assert!(!snapshot.budget_exhausted());
    snapshot.token_budget = Some(1_201);
    assert!(!snapshot.budget_exhausted());
    snapshot.token_budget = Some(1_200);
    assert!(snapshot.budget_exhausted());
}

#[test]
fn goal_actions_use_their_kind_and_default_optional_fields() {
    assert_eq!(
        serde_json::from_value::<GoalAction>(json!({"kind": "set", "objective": "Ship it"}))
            .unwrap(),
        GoalAction::Set {
            objective: "Ship it".into(),
            token_budget: None,
            replace: false,
        }
    );
    for (action, wire) in [
        (
            GoalAction::Set {
                objective: "Ship it".into(),
                token_budget: Some(9_000),
                replace: true,
            },
            json!({"kind": "set", "objective": "Ship it", "token_budget": 9_000, "replace": true}),
        ),
        (
            GoalAction::Edit {
                objective: "Ship it".into(),
            },
            json!({"kind": "edit", "objective": "Ship it"}),
        ),
        (GoalAction::Pause, json!({"kind": "pause"})),
        (GoalAction::Resume, json!({"kind": "resume"})),
        (GoalAction::Clear, json!({"kind": "clear"})),
        (
            GoalAction::SetBudget { token_budget: None },
            json!({"kind": "set_budget", "token_budget": null}),
        ),
    ] {
        assert_eq!(serde_json::to_value(&action).unwrap(), wire);
        assert_eq!(serde_json::from_value::<GoalAction>(wire).unwrap(), action);
    }
}

#[test]
fn unknown_fields_are_ignored_in_snapshots_and_rejected_in_actions_with_fields() {
    let snapshot = goal(GoalStatus::Paused {
        by: GoalPause::User,
    });
    let mut value = serde_json::to_value(&snapshot).unwrap();
    value["future_field"] = json!(true);
    value["status"]["since_ms"] = json!(7);
    assert_eq!(
        serde_json::from_value::<GoalSnapshot>(value).unwrap(),
        snapshot
    );

    for action in [
        json!({"kind": "edit", "objective": "Ship it", "goal_id": Uuid::nil()}),
        json!({"kind": "set_budget", "token_budget": 5, "replace": true}),
    ] {
        assert!(serde_json::from_value::<GoalAction>(action).is_err());
    }
    // Like LoopAction, a variant without fields is internally tagged and tolerates extra ones.
    assert_eq!(
        serde_json::from_value::<GoalAction>(json!({"kind": "pause", "goal_id": Uuid::nil()}))
            .unwrap(),
        GoalAction::Pause
    );
}

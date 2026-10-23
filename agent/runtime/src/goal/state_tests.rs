use super::*;
use tickloop_agent_protocol::GoalAuthor;

const SET_AT: u64 = 1_000;
const NOW: u64 = 2_000;

fn set_event(token_budget: Option<u64>) -> GoalEvent {
    GoalEvent::Set {
        id: Uuid::new_v4(),
        objective: "Ship the export command".into(),
        token_budget,
        replace: false,
        set_by: GoalAuthor::User,
    }
}

fn goal_with(token_budget: Option<u64>) -> GoalSnapshot {
    apply(None, set_event(token_budget), SET_AT)
        .unwrap()
        .goal
        .unwrap()
}

fn with_status(status: GoalStatus) -> GoalSnapshot {
    GoalSnapshot {
        status,
        ..goal_with(None)
    }
}

fn step(goal: &GoalSnapshot, event: GoalEvent) -> GoalTransition {
    apply(Some(goal), event, NOW).unwrap()
}

fn next(goal: &GoalSnapshot, event: GoalEvent) -> GoalSnapshot {
    step(goal, event).goal.unwrap()
}

fn continuation(called_tools: bool, replied_substantively: bool) -> GoalEvent {
    GoalEvent::TurnEnded(TurnSummary {
        continuation: true,
        called_tools,
        replied_substantively,
        failure: None,
    })
}

fn user_turn() -> GoalEvent {
    GoalEvent::TurnEnded(TurnSummary {
        continuation: false,
        called_tools: false,
        replied_substantively: false,
        failure: None,
    })
}

fn left_active(notice: GoalNotice) -> Vec<GoalEffect> {
    vec![GoalEffect::WithdrawContinuation, GoalEffect::Record(notice)]
}

fn blocked() -> GoalStatus {
    GoalStatus::Blocked {
        by: GoalBlock::NoProgress,
    }
}

fn paused(by: GoalPause) -> GoalStatus {
    GoalStatus::Paused { by }
}

/// The completion check's verdict for `goal` as it was when its completion was reported.
fn checked(goal: &GoalSnapshot, verdict: GoalVerdict) -> GoalEvent {
    GoalEvent::Checked {
        goal_id: goal.id,
        objective_revision: goal.objective_revision,
        verdict,
    }
}

#[test]
fn setting_a_goal_starts_it_active_with_fresh_usage() {
    let id = Uuid::new_v4();
    let transition = apply(
        None,
        GoalEvent::Set {
            id,
            objective: "  Ship the export command  ".into(),
            token_budget: Some(5_000),
            replace: false,
            set_by: GoalAuthor::User,
        },
        SET_AT,
    )
    .unwrap();

    assert_eq!(
        transition.goal,
        Some(GoalSnapshot {
            id,
            objective: "Ship the export command".into(),
            objective_revision: 1,
            set_by: GoalAuthor::User,
            status: GoalStatus::Active,
            token_budget: Some(5_000),
            tokens_used: 0,
            time_used_ms: 0,
            last_check: None,
            quiet_turns: 0,
            last_continued_at_ms: None,
            created_at_ms: SET_AT,
            updated_at_ms: SET_AT,
        })
    );
    assert_eq!(transition.effects, [GoalEffect::Record(GoalNotice::Set)]);
}

#[test]
fn setting_over_an_unfinished_goal_needs_an_explicit_replacement() {
    let mut current = goal_with(Some(10_000));
    current.tokens_used = 4_000;
    current.time_used_ms = 9_000;
    for status in [
        GoalStatus::Active,
        paused(GoalPause::User),
        blocked(),
        GoalStatus::BudgetLimited,
    ] {
        current.status = status;
        assert_eq!(
            apply(Some(&current), set_event(None), NOW),
            Err(GoalError::UnfinishedGoal)
        );

        let id = Uuid::new_v4();
        let replaced = step(
            &current,
            GoalEvent::Set {
                id,
                objective: "Write the migration guide".into(),
                token_budget: None,
                replace: true,
                set_by: GoalAuthor::User,
            },
        );
        let goal = replaced.goal.unwrap();
        assert_eq!(goal.id, id);
        assert_eq!(goal.status, GoalStatus::Active);
        assert_eq!((goal.tokens_used, goal.time_used_ms), (0, 0));
        assert_eq!(goal.created_at_ms, NOW);
        assert_eq!(
            replaced.effects,
            [
                GoalEffect::WithdrawContinuation,
                GoalEffect::Record(GoalNotice::Set)
            ]
        );
    }
}

#[test]
fn a_new_goal_follows_a_complete_one_without_confirmation() {
    let complete = with_status(GoalStatus::Complete);
    let transition = step(&complete, set_event(None));
    assert_eq!(transition.goal.unwrap().status, GoalStatus::Active);
    assert_eq!(transition.effects, [GoalEffect::Record(GoalNotice::Set)]);
}

#[test]
fn objectives_and_budgets_are_validated() {
    let too_long = "字".repeat(MAX_OBJECTIVE_CHARACTERS + 1);
    for objective in ["  ".to_owned(), too_long] {
        assert!(matches!(
            apply(
                None,
                GoalEvent::Set {
                    id: Uuid::new_v4(),
                    objective: objective.clone(),
                    token_budget: None,
                    replace: false,
                    set_by: GoalAuthor::User,
                },
                NOW,
            ),
            Err(GoalError::InvalidObjective(_))
        ));
        assert!(matches!(
            apply(Some(&goal_with(None)), GoalEvent::Edit { objective }, NOW),
            Err(GoalError::InvalidObjective(_))
        ));
    }
    assert_eq!(
        apply(None, set_event(Some(0)), NOW),
        Err(GoalError::InvalidBudget)
    );
    assert_eq!(
        apply(
            Some(&goal_with(None)),
            GoalEvent::SetBudget {
                token_budget: Some(0)
            },
            NOW
        ),
        Err(GoalError::InvalidBudget)
    );
}

#[test]
fn user_actions_and_reports_need_a_goal() {
    for event in [
        GoalEvent::Edit {
            objective: "x".into(),
        },
        GoalEvent::Pause,
        GoalEvent::Resume,
        GoalEvent::Clear,
        GoalEvent::SetBudget { token_budget: None },
        GoalEvent::ReportedBlocked { reason: "x".into() },
    ] {
        assert_eq!(apply(None, event, NOW), Err(GoalError::NoGoal));
    }
}

#[test]
fn host_events_without_a_goal_change_nothing() {
    for event in [
        user_turn(),
        GoalEvent::TokensUsed(10),
        GoalEvent::TimeElapsed { ms: 10 },
        GoalEvent::Interrupted,
        GoalEvent::Stopped,
        GoalEvent::HistoryLoaded,
    ] {
        assert_eq!(
            apply(None, event, NOW),
            Ok(GoalTransition {
                goal: None,
                effects: Vec::new(),
            })
        );
    }
}

#[test]
fn editing_replaces_the_objective_and_forgets_the_last_check() {
    let mut current = goal_with(None);
    current.quiet_turns = 2;
    current.last_check = Some(GoalCheck {
        verdict: GoalVerdict::NotMet {
            reason: "no tests".into(),
        },
        checked_at_ms: SET_AT,
    });
    let transition = step(
        &current,
        GoalEvent::Edit {
            objective: "Ship the import command".into(),
        },
    );
    let goal = transition.goal.unwrap();
    assert_eq!(goal.objective, "Ship the import command");
    assert_eq!(goal.last_check, None);
    assert_eq!(goal.quiet_turns, 0);
    assert_eq!(goal.id, current.id);
    assert_eq!(goal.objective_revision, current.objective_revision + 1);
    assert_eq!(
        transition.effects,
        [GoalEffect::InsertObjectiveUpdatedPrompt]
    );

    assert_eq!(
        apply(
            Some(&with_status(GoalStatus::Complete)),
            GoalEvent::Edit {
                objective: "x".into()
            },
            NOW
        ),
        Err(GoalError::Inapplicable {
            action: "edit",
            status: "complete"
        })
    );
}

#[test]
fn editing_a_paused_goal_still_tells_the_running_turn() {
    // `/goal pause` leaves the current turn running on the old objective.
    let running = goal_with(None);
    let paused_goal = next(&running, GoalEvent::Pause);
    let transition = step(
        &paused_goal,
        GoalEvent::Edit {
            objective: "Ship the import command".into(),
        },
    );
    assert_eq!(transition.goal.unwrap().status, paused(GoalPause::User));
    assert_eq!(
        transition.effects,
        [GoalEffect::InsertObjectiveUpdatedPrompt]
    );
}

#[test]
fn pausing_applies_only_to_an_active_goal() {
    let transition = step(&goal_with(None), GoalEvent::Pause);
    assert_eq!(transition.goal.unwrap().status, paused(GoalPause::User));
    assert_eq!(
        transition.effects,
        left_active(GoalNotice::Paused {
            by: GoalPause::User
        })
    );

    for (status, name) in [
        (paused(GoalPause::Interrupted), "paused"),
        (blocked(), "blocked"),
        (GoalStatus::BudgetLimited, "budget_limited"),
        (GoalStatus::Complete, "complete"),
    ] {
        assert_eq!(
            apply(Some(&with_status(status)), GoalEvent::Pause, NOW),
            Err(GoalError::Inapplicable {
                action: "pause",
                status: name
            })
        );
    }
}

#[test]
fn resuming_reactivates_and_restarts_the_quiet_count() {
    for status in [
        paused(GoalPause::Stopped),
        blocked(),
        GoalStatus::BudgetLimited,
    ] {
        let mut current = with_status(status);
        current.quiet_turns = 2;
        let transition = step(&current, GoalEvent::Resume);
        let goal = transition.goal.unwrap();
        assert_eq!(goal.status, GoalStatus::Active);
        assert_eq!(goal.quiet_turns, 0);
        assert!(transition.effects.is_empty());
    }
    for (status, name) in [
        (GoalStatus::Active, "active"),
        (GoalStatus::Complete, "complete"),
    ] {
        assert_eq!(
            apply(Some(&with_status(status)), GoalEvent::Resume, NOW),
            Err(GoalError::Inapplicable {
                action: "resume",
                status: name
            })
        );
    }
}

#[test]
fn clearing_removes_any_goal() {
    for status in [GoalStatus::Active, GoalStatus::Complete, blocked()] {
        assert_eq!(
            step(&with_status(status), GoalEvent::Clear),
            GoalTransition {
                goal: None,
                effects: vec![
                    GoalEffect::WithdrawContinuation,
                    GoalEffect::Record(GoalNotice::Cleared)
                ],
            }
        );
    }
}

#[test]
fn usage_counts_only_while_active_and_survives_pause_and_resume() {
    let goal = goal_with(None);
    assert_eq!(goal.tokens_used, 0);
    let goal = next(&goal, GoalEvent::TokensUsed(300));
    let goal = next(&goal, GoalEvent::TimeElapsed { ms: 2_000 });
    let goal = next(&goal, GoalEvent::Pause);
    let goal = next(&goal, GoalEvent::TokensUsed(200));
    let goal = next(&goal, GoalEvent::TimeElapsed { ms: 5_000 });
    let goal = next(&goal, GoalEvent::Resume);
    let goal = next(&goal, GoalEvent::TokensUsed(100));
    assert_eq!(goal.tokens_used, 400);
    assert_eq!(goal.time_used_ms, 2_000);

    for status in [
        paused(GoalPause::Interrupted),
        blocked(),
        GoalStatus::BudgetLimited,
        GoalStatus::Complete,
    ] {
        let goal = with_status(status);
        let transition = step(&goal, GoalEvent::TokensUsed(100));
        assert_eq!(transition.goal, Some(goal.clone()));
        assert!(transition.effects.is_empty());
        assert_eq!(next(&goal, GoalEvent::TimeElapsed { ms: 100 }), goal);
    }
}

#[test]
fn exhausting_the_budget_limits_the_goal_and_wraps_up_the_turn() {
    let goal = next(&goal_with(Some(1_000)), GoalEvent::TokensUsed(999));
    assert_eq!(goal.status, GoalStatus::Active);

    let transition = step(&goal, GoalEvent::TokensUsed(1));
    assert_eq!(transition.goal.unwrap().status, GoalStatus::BudgetLimited);
    assert_eq!(
        transition.effects,
        [
            GoalEffect::InsertBudgetLimitPrompt,
            GoalEffect::WithdrawContinuation,
            GoalEffect::Record(GoalNotice::BudgetLimited)
        ]
    );
}

#[test]
fn a_budget_limited_goal_resumes_only_after_the_budget_is_raised_or_removed() {
    let limited = next(&goal_with(Some(1_000)), GoalEvent::TokensUsed(1_500));
    assert_eq!(limited.status, GoalStatus::BudgetLimited);
    assert_eq!(
        apply(Some(&limited), GoalEvent::Resume, NOW),
        Err(GoalError::BudgetExhausted)
    );

    let too_small = next(
        &limited,
        GoalEvent::SetBudget {
            token_budget: Some(1_200),
        },
    );
    assert_eq!(
        apply(Some(&too_small), GoalEvent::Resume, NOW),
        Err(GoalError::BudgetExhausted)
    );

    for token_budget in [Some(2_000), None] {
        let raised = next(&limited, GoalEvent::SetBudget { token_budget });
        assert_eq!(raised.status, GoalStatus::BudgetLimited);
        assert_eq!(next(&raised, GoalEvent::Resume).status, GoalStatus::Active);
    }

    let paused_over_budget = next(
        &next(
            &next(&goal_with(Some(1_000)), GoalEvent::TokensUsed(600)),
            GoalEvent::Pause,
        ),
        GoalEvent::SetBudget {
            token_budget: Some(500),
        },
    );
    assert_eq!(paused_over_budget.status, paused(GoalPause::User));
    assert_eq!(
        apply(Some(&paused_over_budget), GoalEvent::Resume, NOW),
        Err(GoalError::BudgetExhausted)
    );
}

#[test]
fn lowering_an_active_goals_budget_below_its_usage_limits_it() {
    let goal = next(&goal_with(None), GoalEvent::TokensUsed(500));
    let transition = step(
        &goal,
        GoalEvent::SetBudget {
            token_budget: Some(400),
        },
    );
    assert_eq!(transition.goal.unwrap().status, GoalStatus::BudgetLimited);
    assert!(
        transition
            .effects
            .contains(&GoalEffect::InsertBudgetLimitPrompt)
    );
    assert!(matches!(
        apply(
            Some(&with_status(GoalStatus::Complete)),
            GoalEvent::SetBudget { token_budget: None },
            NOW
        ),
        Err(GoalError::Inapplicable { .. })
    ));
}

#[test]
fn three_quiet_continuations_block_the_goal() {
    let mut goal = goal_with(None);
    for expected in 1..MAX_QUIET_CONTINUATIONS {
        goal = next(&goal, continuation(false, false));
        assert_eq!(goal.quiet_turns, expected);
        assert_eq!(goal.status, GoalStatus::Active);
    }
    let transition = step(&goal, continuation(false, false));
    assert_eq!(
        transition.goal.unwrap().status,
        GoalStatus::Blocked {
            by: GoalBlock::NoProgress
        }
    );
    assert_eq!(
        transition.effects,
        left_active(GoalNotice::Blocked {
            by: GoalBlock::NoProgress
        })
    );
}

#[test]
fn user_turns_and_progress_break_a_run_of_quiet_continuations() {
    let goal = next(&goal_with(None), continuation(false, false));
    let goal = next(&goal, continuation(false, false));
    let after_user_turn = next(&goal, user_turn());
    assert_eq!(after_user_turn.quiet_turns, 0);
    assert_eq!(after_user_turn.status, GoalStatus::Active);

    let goal_after_user_turn = next(&after_user_turn, continuation(false, false));
    let goal_after_user_turn = next(&goal_after_user_turn, continuation(false, false));
    assert_eq!(goal_after_user_turn.quiet_turns, 2);
    assert_eq!(goal_after_user_turn.status, GoalStatus::Active);

    assert_eq!(next(&goal, continuation(true, false)).quiet_turns, 0);
    assert_eq!(next(&goal, continuation(false, true)).quiet_turns, 0);
}

#[test]
fn an_execution_failure_blocks_but_a_failed_tool_call_does_not() {
    for continuation in [true, false] {
        let transition = step(
            &goal_with(None),
            GoalEvent::TurnEnded(TurnSummary {
                continuation,
                called_tools: true,
                replied_substantively: false,
                failure: Some("provider unavailable".into()),
            }),
        );
        let by = GoalBlock::ExecutionFailed {
            error: "provider unavailable".into(),
        };
        assert_eq!(
            transition.goal.unwrap().status,
            GoalStatus::Blocked { by: by.clone() }
        );
        assert_eq!(transition.effects, left_active(GoalNotice::Blocked { by }));
    }

    let after_failed_tool = next(&goal_with(None), continuation(true, false));
    assert_eq!(after_failed_tool.status, GoalStatus::Active);
}

#[test]
fn turns_ending_while_not_active_change_nothing() {
    for status in [
        paused(GoalPause::User),
        blocked(),
        GoalStatus::BudgetLimited,
        GoalStatus::Complete,
    ] {
        let goal = with_status(status);
        let transition = step(
            &goal,
            GoalEvent::TurnEnded(TurnSummary {
                continuation: true,
                called_tools: false,
                replied_substantively: false,
                failure: Some("error".into()),
            }),
        );
        assert_eq!(transition.goal, Some(goal));
        assert!(transition.effects.is_empty());
    }
}

#[test]
fn check_verdicts_complete_keep_or_block_the_goal() {
    let met = {
        let goal = goal_with(None);
        step(&goal, checked(&goal, GoalVerdict::Met))
    };
    let goal = met.goal.unwrap();
    assert_eq!(goal.status, GoalStatus::Complete);
    assert_eq!(
        goal.last_check,
        Some(GoalCheck {
            verdict: GoalVerdict::Met,
            checked_at_ms: NOW
        })
    );
    assert_eq!(met.effects, left_active(GoalNotice::Completed));

    let not_met = GoalVerdict::NotMet {
        reason: "no test output".into(),
    };
    let transition = {
        let goal = goal_with(None);
        step(&goal, checked(&goal, not_met.clone()))
    };
    let goal = transition.goal.unwrap();
    assert_eq!(goal.status, GoalStatus::Active);
    assert_eq!(goal.last_check.unwrap().verdict, not_met);
    assert!(transition.effects.is_empty());

    let goal = goal_with(None);
    let impossible = step(
        &goal,
        checked(
            &goal,
            GoalVerdict::Impossible {
                reason: "the API was removed".into(),
            },
        ),
    );
    let by = GoalBlock::CheckImpossible {
        reason: "the API was removed".into(),
    };
    assert_eq!(
        impossible.goal.unwrap().status,
        GoalStatus::Blocked { by: by.clone() }
    );
    assert_eq!(impossible.effects, left_active(GoalNotice::Blocked { by }));
}

#[test]
fn a_completion_verified_after_pausing_or_running_out_of_budget_still_counts() {
    for status in [paused(GoalPause::User), GoalStatus::BudgetLimited] {
        let goal = with_status(status);
        let goal = next(&goal, checked(&goal, GoalVerdict::Met));
        assert_eq!(goal.status, GoalStatus::Complete);
    }
    let complete = with_status(GoalStatus::Complete);
    assert_eq!(
        apply(Some(&complete), checked(&complete, GoalVerdict::Met), NOW),
        Err(GoalError::Inapplicable {
            action: "check",
            status: "complete"
        })
    );
}

#[test]
fn the_agent_reports_a_blocker_only_while_the_goal_is_pursued() {
    let by = GoalBlock::Reported {
        reason: "no credentials".into(),
    };
    for status in [GoalStatus::Active, paused(GoalPause::User)] {
        let transition = step(
            &with_status(status),
            GoalEvent::ReportedBlocked {
                reason: "no credentials".into(),
            },
        );
        assert_eq!(
            transition.goal.unwrap().status,
            GoalStatus::Blocked { by: by.clone() }
        );
        assert_eq!(
            transition.effects,
            left_active(GoalNotice::Blocked { by: by.clone() })
        );
    }
    for status in [blocked(), GoalStatus::BudgetLimited, GoalStatus::Complete] {
        assert!(matches!(
            apply(
                Some(&with_status(status)),
                GoalEvent::ReportedBlocked { reason: "x".into() },
                NOW
            ),
            Err(GoalError::Inapplicable {
                action: "block",
                ..
            })
        ));
    }
}

#[test]
fn esc_stop_and_history_load_pause_only_an_active_goal() {
    for (event, by) in [
        (GoalEvent::Interrupted, GoalPause::Interrupted),
        (GoalEvent::Stopped, GoalPause::Stopped),
        (GoalEvent::HistoryLoaded, GoalPause::HistoryLoaded),
    ] {
        let transition = step(&goal_with(None), event.clone());
        assert_eq!(transition.goal.unwrap().status, paused(by));
        assert_eq!(transition.effects, left_active(GoalNotice::Paused { by }));

        for status in [
            paused(GoalPause::User),
            blocked(),
            GoalStatus::BudgetLimited,
            GoalStatus::Complete,
        ] {
            let goal = with_status(status);
            let transition = step(&goal, event.clone());
            assert_eq!(transition.goal, Some(goal));
            assert!(transition.effects.is_empty());
        }
    }
}

#[test]
fn only_changes_move_the_update_time() {
    let goal = goal_with(None);
    assert_eq!(next(&goal, GoalEvent::TokensUsed(1)).updated_at_ms, NOW);
    assert_eq!(next(&goal, GoalEvent::TokensUsed(0)).updated_at_ms, SET_AT);
    let complete = with_status(GoalStatus::Complete);
    assert_eq!(
        next(&complete, GoalEvent::Interrupted).updated_at_ms,
        SET_AT
    );
}

fn ready() -> ContinuationConditions {
    ContinuationConditions {
        receiving: true,
        turn_ended: true,
        input_queue_empty: true,
        background_agents_running: false,
        background_shells_running: false,
        plan_mode: false,
    }
}

#[test]
fn an_active_goal_continues_once_the_loop_is_quiet() {
    assert_eq!(
        continuation_eligibility(Some(&goal_with(None)), ready()),
        Ok(())
    );
}

#[test]
fn each_condition_alone_prevents_a_continuation() {
    let goal = goal_with(None);
    for (conditions, reason) in [
        (
            ContinuationConditions {
                receiving: false,
                ..ready()
            },
            NoContinuation::NotReceiving,
        ),
        (
            ContinuationConditions {
                turn_ended: false,
                ..ready()
            },
            NoContinuation::TurnRunning,
        ),
        (
            ContinuationConditions {
                input_queue_empty: false,
                ..ready()
            },
            NoContinuation::InputQueued,
        ),
        (
            ContinuationConditions {
                background_agents_running: true,
                ..ready()
            },
            NoContinuation::BackgroundAgentsRunning,
        ),
        (
            ContinuationConditions {
                background_shells_running: true,
                ..ready()
            },
            NoContinuation::BackgroundShellsRunning,
        ),
        (
            ContinuationConditions {
                plan_mode: true,
                ..ready()
            },
            NoContinuation::PlanMode,
        ),
    ] {
        assert_eq!(
            continuation_eligibility(Some(&goal), conditions),
            Err(reason)
        );
    }
    assert_eq!(
        continuation_eligibility(None, ready()),
        Err(NoContinuation::NoGoal)
    );
    for status in [
        paused(GoalPause::User),
        blocked(),
        GoalStatus::BudgetLimited,
        GoalStatus::Complete,
    ] {
        assert_eq!(
            continuation_eligibility(Some(&with_status(status)), ready()),
            Err(NoContinuation::NotActive)
        );
    }
}

#[test]
fn a_check_for_an_edited_objective_is_discarded() {
    let reported = goal_with(None);
    let edited = next(
        &reported,
        GoalEvent::Edit {
            objective: "Ship the import command".into(),
        },
    );
    for verdict in [
        GoalVerdict::Met,
        GoalVerdict::Impossible { reason: "x".into() },
    ] {
        let transition = step(&edited, checked(&reported, verdict));
        assert_eq!(transition.goal, Some(edited.clone()));
        assert_eq!(
            transition.effects,
            [GoalEffect::StaleCheckDiscarded {
                goal_id: reported.id,
                objective_revision: reported.objective_revision,
            }]
        );
    }
}

#[test]
fn a_check_for_a_replaced_or_cleared_goal_is_discarded() {
    let reported = goal_with(None);
    let replacement = next(
        &reported,
        GoalEvent::Set {
            id: Uuid::new_v4(),
            objective: "Write the migration guide".into(),
            token_budget: None,
            replace: true,
            set_by: GoalAuthor::User,
        },
    );
    assert!(replacement.objective_revision > reported.objective_revision);
    let stale = GoalEffect::StaleCheckDiscarded {
        goal_id: reported.id,
        objective_revision: reported.objective_revision,
    };

    let transition = step(&replacement, checked(&reported, GoalVerdict::Met));
    assert_eq!(transition.goal, Some(replacement.clone()));
    assert_eq!(transition.effects, std::slice::from_ref(&stale));

    // A replacement that reuses the id is still told apart by its revision.
    let same_id = GoalSnapshot {
        id: reported.id,
        ..replacement
    };
    let transition = step(&same_id, checked(&reported, GoalVerdict::Met));
    assert_eq!(transition.goal.unwrap().status, GoalStatus::Active);
    assert_eq!(transition.effects, std::slice::from_ref(&stale));

    assert_eq!(
        apply(None, checked(&reported, GoalVerdict::Met), NOW),
        Ok(GoalTransition {
            goal: None,
            effects: vec![stale],
        })
    );
}

#[test]
fn usage_saturates_instead_of_overflowing() {
    let mut goal = goal_with(None);
    goal.tokens_used = u64::MAX - 1;
    goal.time_used_ms = u64::MAX - 1;
    let goal = next(&goal, GoalEvent::TokensUsed(10));
    let goal = next(&goal, GoalEvent::TimeElapsed { ms: 10 });
    assert_eq!((goal.tokens_used, goal.time_used_ms), (u64::MAX, u64::MAX));
    assert_eq!(goal.status, GoalStatus::Active);

    let mut budgeted = goal_with(Some(u64::MAX));
    budgeted.tokens_used = u64::MAX - 1;
    let transition = step(&budgeted, GoalEvent::TokensUsed(u64::MAX));
    let limited = transition.goal.unwrap();
    assert_eq!(limited.tokens_used, u64::MAX);
    assert_eq!(limited.status, GoalStatus::BudgetLimited);

    let mut revised = goal_with(None);
    revised.objective_revision = u64::MAX;
    let edited = next(
        &revised,
        GoalEvent::Edit {
            objective: "Ship the import command".into(),
        },
    );
    assert_eq!(edited.objective_revision, u64::MAX);
}

#[test]
fn a_failed_check_blocks_its_goal_and_a_stale_one_is_discarded() {
    let goal = goal_with(None);
    let failed = |goal: &GoalSnapshot| GoalEvent::CheckFailed {
        goal_id: goal.id,
        objective_revision: goal.objective_revision,
        error: "provider unavailable".into(),
    };
    let by = GoalBlock::ExecutionFailed {
        error: "provider unavailable".into(),
    };
    let transition = step(&goal, failed(&goal));
    assert_eq!(
        transition.goal.unwrap().status,
        GoalStatus::Blocked { by: by.clone() }
    );
    assert_eq!(transition.effects, left_active(GoalNotice::Blocked { by }));

    let edited = next(
        &goal,
        GoalEvent::Edit {
            objective: "Ship the import command".into(),
        },
    );
    let transition = step(&edited, failed(&goal));
    assert_eq!(transition.goal, Some(edited));
    assert_eq!(
        transition.effects,
        [GoalEffect::StaleCheckDiscarded {
            goal_id: goal.id,
            objective_revision: goal.objective_revision,
        }]
    );
}

#[test]
fn an_edit_makes_the_objective_the_users() {
    let agent_set = apply(
        None,
        GoalEvent::Set {
            id: Uuid::new_v4(),
            objective: "Ship the export command".into(),
            token_budget: None,
            replace: false,
            set_by: GoalAuthor::Agent,
        },
        SET_AT,
    )
    .unwrap()
    .goal
    .unwrap();
    assert_eq!(agent_set.set_by, GoalAuthor::Agent);
    let edited = next(
        &agent_set,
        GoalEvent::Edit {
            objective: "Ship the import command".into(),
        },
    );
    assert_eq!(edited.set_by, GoalAuthor::User);
}

use super::*;
use std::sync::Arc;
use tickloop_agent_protocol::{GoalPause, Receiving};

fn goal(status: GoalStatus) -> GoalSnapshot {
    GoalSnapshot {
        id: Uuid::new_v4(),
        objective: "Verify the release".into(),
        objective_revision: 1,
        set_by: tickloop_agent_protocol::GoalAuthor::User,
        status,
        token_budget: Some(50_000),
        tokens_used: 12_500,
        time_used_ms: 90_000,
        last_check: None,
        quiet_turns: 0,
        last_continued_at_ms: None,
        created_at_ms: 1,
        updated_at_ms: 1,
    }
}

#[test]
fn goal_command_parser_preserves_conditions_and_limits_unicode_characters() {
    assert!(matches!(parse("/goal").unwrap(), GoalCommand::Show));
    assert!(matches!(parse("/goal edit").unwrap(), GoalCommand::Edit));
    for (command, expected) in [
        ("pause", GoalAction::Pause),
        ("resume", GoalAction::Resume),
        ("clear", GoalAction::Clear),
    ] {
        assert!(
            matches!(parse(&format!("/goal {command}")).unwrap(), GoalCommand::Apply(action) if action == expected)
        );
    }
    assert!(
        matches!(parse("/goal ship\nand verify").unwrap(), GoalCommand::Apply(GoalAction::Set { objective, replace: false, token_budget: None }) if objective == "ship\nand verify")
    );
    assert!(parse(&format!("/goal {}", "中".repeat(4000))).is_ok());
    let error = parse(&format!("/goal {}", "中".repeat(4001)))
        .err()
        .unwrap();
    assert!(error.contains("file or Artifact"));
    assert!(validate_objective("  ").is_err());
    for value in ["0", "-1", "5k", "18446744073709551616"] {
        assert!(budget(value).is_err());
    }
    assert_eq!(budget("50000").unwrap(), Some(50_000));
    assert_eq!(budget("none").unwrap(), None);
}

async fn fixture() -> (Agent, Uuid, Console, Menus) {
    let core = tickloop_agent_runtime::Agent::new_embedded(
        tickloop_agent_test_support::config(),
        Arc::new(tickloop_agent_test_support::ScriptedModel::new(vec![])),
    );
    core.hold_goal_continuations_for_tests();
    let agent = Agent::from_embedded(core, tickloop_agent_test_support::config());
    let chat = agent
        .create_chat(std::env::current_dir().unwrap())
        .await
        .unwrap();
    (
        agent,
        chat,
        Console::test_interactive_for_chat(chat),
        Menus::default(),
    )
}

#[tokio::test]
async fn goal_commands_confirm_replacement_and_edit_pause_resume_budget_clear() {
    let (agent, chat, console, mut menus) = fixture().await;
    menus
        .goal_command(&agent, &console, chat, "/goal first condition")
        .await
        .unwrap();
    let first = agent.snapshot(chat).await.unwrap().goal.unwrap();
    menus
        .goal_command(&agent, &console, chat, "/goal replacement")
        .await
        .unwrap();
    let id = menus.id().unwrap();
    assert_eq!(
        agent.snapshot(chat).await.unwrap().goal.unwrap().id,
        first.id
    );
    menus
        .choose(&agent, &console, id, None, None)
        .await
        .unwrap();
    assert_eq!(
        agent.snapshot(chat).await.unwrap().goal.unwrap().objective,
        "first condition"
    );
    menus
        .goal_command(&agent, &console, chat, "/goal replacement")
        .await
        .unwrap();
    menus
        .choose(&agent, &console, menus.id().unwrap(), Some(0), None)
        .await
        .unwrap();
    assert_eq!(
        agent.snapshot(chat).await.unwrap().goal.unwrap().objective,
        "replacement"
    );
    menus
        .goal_command(&agent, &console, chat, "/goal edit")
        .await
        .unwrap();
    menus
        .choose(
            &agent,
            &console,
            menus.id().unwrap(),
            Some(0),
            Some("edited\ncondition".into()),
        )
        .await
        .unwrap();
    assert_eq!(
        agent.snapshot(chat).await.unwrap().goal.unwrap().objective,
        "edited\ncondition"
    );
    menus
        .goal_command(&agent, &console, chat, "/goal pause")
        .await
        .unwrap();
    assert!(matches!(
        agent.snapshot(chat).await.unwrap().goal.unwrap().status,
        GoalStatus::Paused { .. }
    ));
    menus
        .goal_command(&agent, &console, chat, "/goal resume")
        .await
        .unwrap();
    assert_eq!(
        agent.snapshot(chat).await.unwrap().goal.unwrap().status,
        GoalStatus::Active
    );
    menus
        .goal_command(&agent, &console, chat, "/goal")
        .await
        .unwrap();
    menus
        .choose(&agent, &console, menus.id().unwrap(), Some(3), None)
        .await
        .unwrap();
    menus
        .choose(
            &agent,
            &console,
            menus.id().unwrap(),
            Some(0),
            Some("75000".into()),
        )
        .await
        .unwrap();
    assert_eq!(
        agent
            .snapshot(chat)
            .await
            .unwrap()
            .goal
            .unwrap()
            .token_budget,
        Some(75_000)
    );
    menus
        .goal_command(&agent, &console, chat, "/goal clear")
        .await
        .unwrap();
    assert!(agent.snapshot(chat).await.unwrap().goal.is_none());
    agent.shutdown().await;
}

#[tokio::test]
async fn continue_prompt_keeps_or_resumes_receiving_and_goal_independently() {
    for resume in [false, true] {
        let (agent, chat, console, mut menus) = fixture().await;
        menus
            .goal_command(&agent, &console, chat, "/goal verify")
            .await
            .unwrap();
        agent.stop(chat).await.unwrap();
        let status = agent.status(chat).await.unwrap();
        menus.offer_continue(&console, &status).unwrap();
        let id = menus.id().unwrap();
        let menu = console.menu_snapshot(id).unwrap().unwrap();
        assert_eq!(
            menu.options,
            ["Continue receiving and resume Goal", "Keep paused"]
        );
        let outcome = menus
            .choose(&agent, &console, id, Some(usize::from(!resume)), None)
            .await
            .unwrap();
        let snapshot = agent.snapshot(chat).await.unwrap();
        assert_eq!(snapshot.chat_loop.receiving.is_receiving(), resume);
        assert_eq!(snapshot.goal.unwrap().status == GoalStatus::Active, resume);
        if !resume {
            assert!(outcome.status.unwrap().contains("/goal resume"));
        }
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn paused_or_blocked_goal_prompt_does_not_require_receiving_to_be_paused() {
    let (agent, chat, console, mut menus) = fixture().await;
    for state in [
        GoalStatus::Paused {
            by: GoalPause::HistoryLoaded,
        },
        GoalStatus::Blocked {
            by: tickloop_agent_protocol::GoalBlock::NoProgress,
        },
    ] {
        let mut status = agent.status(chat).await.unwrap();
        status.goal = Some(goal(state));
        status.chat_loop.receiving = Receiving::Receiving;
        menus.reoffer_continue(chat);
        menus.offer_continue(&console, &status).unwrap();
        let id = menus.id().unwrap();
        assert_eq!(
            console.menu_snapshot(id).unwrap().unwrap().options,
            ["Resume Goal", "Keep paused"]
        );
        menus
            .choose(&agent, &console, id, None, None)
            .await
            .unwrap();
    }
    agent.shutdown().await;
}

#[tokio::test]
async fn goal_event_history_survives_reopening_and_uses_notice_styles() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(tickloop_agent_test_support::ScriptedModel::new(vec![]));
    let core = tickloop_agent_runtime::Agent::open_embedded(
        directory.path().into(),
        tickloop_agent_test_support::config(),
        model.clone(),
    )
    .unwrap();
    core.hold_goal_continuations_for_tests();
    let chat = core.create_chat(directory.path().into()).await.unwrap();
    core.manage_goal(
        chat,
        GoalAction::Set {
            objective: "Save the evidence".into(),
            token_budget: None,
            replace: false,
        },
    )
    .await
    .unwrap();
    core.manage_goal(chat, GoalAction::Pause).await.unwrap();
    let before = core.snapshot(chat).await.unwrap().goal_history;
    assert_eq!(before.len(), 2);
    core.shutdown().await;
    let path = tickloop_agent_test_support::transcript_file(directory.path(), chat);
    let mut transcript = std::fs::read_to_string(&path).unwrap();
    let notices = transcript
        .lines()
        .filter_map(|line| {
            let value: serde_json::Value = serde_json::from_str(line).unwrap();
            (value["type"] == "goal-event").then_some(value)
        })
        .collect::<Vec<_>>();
    transcript.push_str(
        &serde_json::json!({"type": "retained-snapshot", "chatId": chat, "lines": notices})
            .to_string(),
    );
    transcript.push('\n');
    std::fs::write(path, transcript).unwrap();
    let reopened = Agent::open_embedded(
        directory.path().into(),
        tickloop_agent_test_support::config(),
        model,
    )
    .unwrap();
    let snapshot = reopened.resume_chat(chat).await.unwrap();
    assert_eq!(snapshot.goal_history, before);
    apply(
        &reopened,
        chat,
        GoalAction::SetBudget {
            token_budget: Some(100_000),
        },
    )
    .await
    .unwrap();
    assert_eq!(reopened.snapshot(chat).await.unwrap().goal_history, before);
    let history = super::super::super::workflows::public_history(
        &snapshot,
        super::super::super::display::DisplayProfile::default(),
    );
    assert!(history.contains("Goal set"), "{history}");
    assert!(history.contains("Goal paused"), "{history}");
    reopened.shutdown().await;
}

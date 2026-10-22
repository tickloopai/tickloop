use std::sync::Arc;

use tickloop_agent_protocol::{ExecutionMode, GoalBlock};
use tickloop_agent_test_support::{ScriptedModel, config, text};

use super::*;

const WAIT: Duration = Duration::from_secs(10);
const SETTLE: Duration = Duration::from_millis(400);

async fn goal_status(agent: &Agent, chat: ChatId) -> Option<GoalStatus> {
    agent
        .snapshot(chat)
        .await
        .unwrap()
        .goal
        .map(|goal| goal.status)
}

/// A continuation named by a turn that already had one, as a crash replay or another process
/// would hand it again, finds its ledger record and runs nothing again.
#[tokio::test]
async fn a_continuation_repeated_for_the_same_turn_runs_once() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![text("progress")]));
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    {
        let mut chats = agent.inner.chats.lock().await;
        let chat = chats.get_mut(&chat).unwrap();
        agent
            .change_goal(
                chat,
                GoalEvent::Set {
                    id: Uuid::new_v4(),
                    objective: "Ship the export command".into(),
                    token_budget: None,
                    replace: false,
                },
            )
            .unwrap();
    }
    let ended_turn = Uuid::new_v4();
    agent.continue_goal(chat, ended_turn).await;
    // The next continuation finds the script exhausted, which fails its turn and blocks the goal.
    tokio::time::timeout(WAIT, async {
        while !matches!(
            goal_status(&agent, chat).await,
            Some(GoalStatus::Blocked {
                by: GoalBlock::ExecutionFailed { .. }
            })
        ) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the goal is blocked");
    let requests = || model.requests.lock().unwrap().len();
    let ran = requests();

    {
        let mut chats = agent.inner.chats.lock().await;
        let chat = chats.get_mut(&chat).unwrap();
        chat.snapshot.goal.as_mut().unwrap().status = GoalStatus::Active;
    }
    agent.continue_goal(chat, ended_turn).await;
    tokio::time::sleep(SETTLE).await;
    assert_eq!(requests(), ran, "the repeated continuation ran nothing");

    agent.continue_goal(chat, Uuid::new_v4()).await;
    tokio::time::timeout(WAIT, async {
        while requests() == ran {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("a continuation for another turn runs");
    agent.shutdown().await;
}

#[test]
fn a_report_of_the_goal_adds_its_remaining_budget() {
    let goal = GoalSnapshot {
        id: Uuid::nil(),
        objective: "Ship it".into(),
        objective_revision: 1,
        status: GoalStatus::Active,
        token_budget: Some(1_000),
        tokens_used: 1_200,
        time_used_ms: 0,
        last_check: None,
        quiet_turns: 0,
        created_at_ms: 0,
        updated_at_ms: 0,
    };
    assert_eq!(goal_report(&goal)["tokens_remaining"], 0);
    let unbudgeted = GoalSnapshot {
        token_budget: None,
        ..goal
    };
    assert!(goal_report(&unbudgeted).get("tokens_remaining").is_none());
}

#[test]
fn only_fresh_input_and_output_are_charged() {
    assert_eq!(
        charged_tokens(&Usage {
            input_tokens: Some(100),
            output_tokens: Some(20),
            cache_read_input_tokens: Some(5_000),
            cache_write_input_tokens: Some(300),
        }),
        120
    );
}

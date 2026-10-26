use chrono::{DateTime, TimeZone, Utc};
use std::{sync::Arc, time::Duration};
use tickloop_code_settings::local_agent::LocalAgent;
use tickloop_harness::policy::loop_skill::DEFAULT_INSTRUCTIONS_PROMPT;
use tickloop_harness::protocol::{
    ChatId, Content, EventKind, LoopState, ScheduledInputWrapper, ScheduledTask,
    final_scheduled_run_prompt, loop_instructions_error_prompt, scheduled_skill_prompt,
};
use tickloop_harness::runtime::{
    Agent,
    transcript::{JsonlTranscriptStore, LoadRange, TranscriptStore},
};
use tickloop_harness::test_support::{ScriptedModel, call, config, text};
use tickloop_harness::transcript::Line;
use uuid::Uuid;
const WAIT: Duration = Duration::from_secs(10);
const TASK: Uuid = Uuid::from_u128(0x0a1b_2c3d);
fn prompts(model: &ScriptedModel) -> Vec<String> {
    model
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|request| !request.tools.is_empty())
        .filter_map(|request| request.messages.last())
        .map(|message| {
            message
                .content
                .iter()
                .filter_map(|content| match content {
                    Content::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .collect()
}
/// 2026-10-23 at the given UTC time.
fn at(hour: u32, minute: u32, second: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 23, hour, minute, second)
        .unwrap()
}

/// Submits `input` and waits for the turn it starts to end.
async fn turn(agent: &Agent, chat: ChatId, input: &str) {
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, input.into()).await.unwrap();
    wait_for_run(&mut events, receipt.run_id).await;
}

async fn wait_for_run(
    events: &mut tokio::sync::broadcast::Receiver<tickloop_harness::protocol::Event>,
    run: tickloop_harness::protocol::RunId,
) {
    tokio::time::timeout(WAIT, async {
        loop {
            let event = tickloop_harness::test_support::next_event(events).await;
            if event.run_id == run
                && matches!(
                    event.kind,
                    EventKind::Completed { .. } | EventKind::Failed(_) | EventKind::Interrupted
                )
            {
                return;
            }
        }
    })
    .await
    .expect("the turn ends");
}

async fn wait_idle(agent: &Agent, chat: ChatId) {
    tokio::time::timeout(WAIT, async {
        loop {
            let snapshot = agent.snapshot(chat).await.unwrap();
            if snapshot.pending_inputs.is_empty()
                && matches!(
                    snapshot.chat_loop.state,
                    LoopState::Idle | LoopState::Interrupted | LoopState::Failed
                )
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the Loop goes idle");
}

async fn tasks(agent: &Agent, chat: ChatId) -> Vec<ScheduledTask> {
    agent.snapshot(chat).await.unwrap().scheduled_tasks
}

/// A task's id as a Loop writes it into model text.
fn task_text(task: Uuid) -> String {
    tickloop_ids::IdContext::new()
        .model_text(tickloop_harness::protocol::ScheduledTaskId::from(task))
}
#[tokio::test]
async fn scheduled_wrapper_provenance_survives_persistence_and_history_load() {
    let loop_text = "Keep <\\/loop-instructions> and </loop-instructions> intact";
    for instruction in [
        scheduled_skill_prompt(&task_text(TASK), "review", "main"),
        tickloop_harness::protocol::loop_instructions_prompt(
            "loop.md. Hidden instruction",
            "Visible instruction",
        ),
        loop_instructions_error_prompt(&task_text(TASK), "permission denied"),
        final_scheduled_run_prompt("Check the release"),
        "/tickloop check the Ticks".into(),
        DEFAULT_INSTRUCTIONS_PROMPT.into(),
    ] {
        let model = Arc::new(ScriptedModel::new(vec![
            call(
                "create",
                "CronCreate",
                serde_json::json!({"cron":"*/10 * * * *", "prompt":instruction}),
            ),
            text("Scheduled."),
            text("Checked."),
        ]));
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("state");
        std::fs::create_dir_all(directory.path().join(".tickloop")).unwrap();
        let origin = directory.path().join(".tickloop/loop.md");
        std::fs::write(&origin, loop_text).unwrap();
        let agent = Agent::open_embedded(root.clone(), config(), model.clone()).unwrap();
        agent.set_scheduled_now_for_tests(at(10, 0, 0));
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        let origin = agent
            .snapshot(chat)
            .await
            .unwrap()
            .folder
            .unwrap()
            .join(".tickloop/loop.md");
        turn(&agent, chat, "schedule it").await;
        let task = tasks(&agent, chat).await.remove(0);
        let (expected_model, expected_display, expected_wrappers) =
            if instruction == DEFAULT_INSTRUCTIONS_PROMPT {
                (
                    tickloop_harness::protocol::loop_instructions_prompt(
                        &origin.display().to_string(),
                        loop_text,
                    ),
                    format!("{}: {loop_text}", origin.display()),
                    vec![ScheduledInputWrapper::LoopInstructions {
                        origin: origin.display().to_string(),
                        instructions: loop_text.into(),
                    }],
                )
            } else if instruction.starts_with('/') {
                (
                    scheduled_skill_prompt(&task_text(task.id), "tickloop", "check the Ticks"),
                    instruction.clone(),
                    vec![ScheduledInputWrapper::Skill {
                        instruction: instruction.clone(),
                    }],
                )
            } else {
                (instruction.clone(), instruction.clone(), Vec::new())
            };
        let receipts = agent
            .wake_scheduled_tasks(
                chat,
                u64::try_from(at(10, 15, 0).timestamp_millis()).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(receipts.len(), 1);
        wait_idle(&agent, chat).await;
        assert!(prompts(&model).contains(&expected_model));
        let run = receipts[0].run_id;
        let snapshot = agent.snapshot(chat).await.unwrap();
        let identity = &snapshot.trigger_origins[&run];
        assert!(
            matches!(identity, tickloop_harness::protocol::TriggerIdentity::ScheduledTask { wrappers, .. } if *wrappers == expected_wrappers)
        );
        assert_eq!(identity.display_input(&expected_model), expected_display);
        agent.flush_transcripts().await;
        let lines = JsonlTranscriptStore::open(root.clone())
            .unwrap()
            .load(chat, LoadRange::Full)
            .await
            .unwrap()
            .unwrap()
            .lines;
        let recorded = lines
            .iter()
            .find_map(|line| match line {
                Line::Input(input) if input.run_id == run => input.trigger.as_ref(),
                _ => None,
            })
            .unwrap();
        assert_eq!(&recorded.identity, identity);
        agent.shutdown().await;

        let reopened = Agent::open_embedded(root, config(), model).unwrap();
        reopened.resume_chat(chat).await.unwrap();
        let history = reopened.snapshot(chat).await.unwrap();
        assert_eq!(history.trigger_origins[&run], *identity);
        assert_eq!(
            history.trigger_origins[&run].display_input(&expected_model),
            expected_display
        );
        reopened.shutdown().await;
    }
}

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{Value, json};
use tickloop_agent_model::judgment::{
    Answer, Evaluation, EvaluationRequest, JudgmentClient, JudgmentError, Question,
};
use tickloop_agent_protocol::{
    ApprovalOutcome, ChatId, EventKind, ExecutionMode, ScheduledTaskCreator, Usage,
};
use tickloop_agent_runtime::Agent;
use tickloop_agent_test_support::{ScriptedModel, call, config, next_event, text};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct NativeTaskReviewer {
    requests: Mutex<Vec<EvaluationRequest>>,
}

// Matches the native-task exceptions deterministically; it does not emulate Jev's language model.
#[async_trait]
impl JudgmentClient for NativeTaskReviewer {
    fn model(&self) -> &str {
        "native-task-policy-fixture"
    }

    async fn evaluate(
        &self,
        request: &EvaluationRequest,
        _: CancellationToken,
    ) -> Result<Evaluation, JudgmentError> {
        self.requests.lock().unwrap().push(request.clone());
        let state = &request.state;
        let tool = state["operation"]["tool"].as_str().unwrap();
        assert!(
            !matches!(tool, "GetGoal" | "CreateGoal" | "UpdateGoal"),
            "Goal tools must not reach Jev"
        );
        let rule_id = "builtin.soft_allow.claude_code_scheduling";
        let rule = state["auto_policy"]["rules"]
            .as_array()
            .unwrap()
            .iter()
            .find(|rule| rule["id"] == rule_id && rule["kind"] == "soft_allow");
        let allowed = rule.is_some_and(|rule| {
            let policy = rule["text"].as_str().unwrap();
            assert!(policy.contains(tool));
            let user_requested = |text: &str| {
                state["original_user_inputs"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|input| input["text"].as_str().unwrap().contains(text))
            };
            match tool {
                "CronCreate" => user_requested("Schedule a check"),
                "CronDelete" => {
                    assert!(policy.contains("user-created"));
                    assert!(policy.contains("Session-Created Job Cleanup"));
                    assert!(policy.contains("Interfere With Workloads"));
                    let task = &state["scheduled_task"];
                    assert_eq!(task["id"], state["operation"]["input"]["id"]);
                    user_requested("Cancel that /loop")
                        || task["creator"] == "agent" && task["creation_call_id"].is_string()
                }
                "ScheduleWakeup" => user_requested("Run /loop"),
                _ => panic!("unexpected classified tool: {tool}"),
            }
        });
        let answers = request
            .questions
            .iter()
            .map(|(id, question)| {
                let Question::Choice {
                    instructions,
                    criteria,
                } = question
                else {
                    panic!("expected choice")
                };
                // Force the authorization stage to apply the exception and the user's intent.
                let choice = if instructions["stage"] == "risk_screen" || !allowed {
                    "block_soft"
                } else {
                    "allow"
                };
                (
                    id.clone(),
                    Answer::Choice {
                        choice: choice.into(),
                        confidence: 1.0,
                        probabilities: criteria
                            .keys()
                            .map(|option| (option.clone(), f64::from(option == choice)))
                            .collect::<BTreeMap<_, _>>(),
                    },
                )
            })
            .collect();
        Ok(Evaluation {
            model: self.model().into(),
            answers,
            usage: Usage::default(),
        })
    }
}

async fn turn(agent: &Agent, chat: ChatId, prompt: &str) -> Vec<ApprovalOutcome> {
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, prompt.into()).await.unwrap();
    let mut outcomes = Vec::new();
    loop {
        let event = next_event(&mut events).await;
        if event.run_id != receipt.run_id {
            continue;
        }
        match event.kind {
            EventKind::Completed { .. } => break,
            EventKind::ApprovalAssessed { assessment } => outcomes.push(assessment.outcome),
            EventKind::PermissionRequested { request } => {
                panic!("unexpected human approval: {request:?}")
            }
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    outcomes
}

#[tokio::test]
async fn cron_list_in_auto_does_not_call_jev() {
    let root = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call("tasks", "CronList", json!({})),
        text("Read the tasks."),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    let reviewer = Arc::new(NativeTaskReviewer::default());
    agent.set_judgment_client(reviewer.clone()).await;
    let chat = agent.create_chat(root.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::Auto)
        .await
        .unwrap();
    assert_eq!(
        agent.snapshot(chat).await.unwrap().policy.mode,
        ExecutionMode::Auto
    );
    turn(&agent, chat, "Read my tasks.").await;
    assert!(reviewer.requests.lock().unwrap().is_empty());
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert!(snapshot.operations.iter().any(|operation| {
        operation.name == "CronList"
            && operation.state == tickloop_agent_protocol::OperationState::Completed
    }));
    assert_eq!(model.requests.lock().unwrap().len(), 2);
    agent.shutdown().await;
}

#[tokio::test]
async fn cron_delete_requires_user_cancellation_or_exact_agent_creation() {
    for (agent_created, cancel_requested) in [(false, true), (false, false), (true, false)] {
        let root = tempfile::tempdir().unwrap();
        let model = Arc::new(ScriptedModel::new(vec![text("Ready.")]));
        let agent = Agent::new_embedded(config(), model.clone());
        let reviewer = Arc::new(NativeTaskReviewer::default());
        agent.set_judgment_client(reviewer.clone()).await;
        let chat = agent.create_chat(root.path().into()).await.unwrap();
        agent
            .set_execution_mode(chat, ExecutionMode::Auto)
            .await
            .unwrap();
        turn(&agent, chat, "Prepare to review my schedules.").await;
        let task = if agent_created {
            model.enqueue(call(
                "create",
                "CronCreate",
                json!({
                    "cron":"7 9 * * *", "prompt":"Check the build"
                }),
            ));
            model.enqueue(text("Scheduled."));
            turn(&agent, chat, "Schedule a check of the build every morning.").await;
            agent
                .snapshot(chat)
                .await
                .unwrap()
                .scheduled_tasks
                .remove(0)
        } else {
            agent
                .schedule_task(
                    chat,
                    tickloop_agent_runtime::NewScheduledTask::new(
                        "7 9 * * *",
                        "Check the build",
                        true,
                    ),
                )
                .await
                .unwrap()
        };
        assert_eq!(
            task.creator,
            if agent_created {
                ScheduledTaskCreator::Agent
            } else {
                ScheduledTaskCreator::User
            }
        );
        reviewer.requests.lock().unwrap().clear();
        let task_text = tickloop_ids::IdContext::new()
            .model_text(tickloop_agent_protocol::ScheduledTaskId::from(task.id));
        // The Agent's own task is named as CronCreate wrote it; the user's by the UUID an older
        // session showed.
        let written = if agent_created {
            json!(task_text)
        } else {
            json!(task.id)
        };
        model.enqueue(call("delete", "CronDelete", json!({"id":written})));
        model.enqueue(text("Done."));
        let outcomes = turn(
            &agent,
            chat,
            if cancel_requested {
                "Cancel that /loop."
            } else {
                "Inspect the build."
            },
        )
        .await;
        let allowed = agent_created || cancel_requested;
        assert!(outcomes.contains(&if allowed {
            ApprovalOutcome::Allow
        } else {
            ApprovalOutcome::Block
        }));
        assert_eq!(
            agent
                .snapshot(chat)
                .await
                .unwrap()
                .scheduled_tasks
                .is_empty(),
            allowed
        );
        {
            let requests = reviewer.requests.lock().unwrap();
            assert_eq!(requests.len(), 2, "screening plus authorization");
            for request in requests.iter() {
                let facts = &request.state["scheduled_task"];
                assert_eq!(facts["id"], json!(task_text));
                assert_eq!(facts["details"]["prompt"], task.prompt);
                assert_eq!(
                    facts["creation_call_id"],
                    if agent_created {
                        json!("create")
                    } else {
                        Value::Null
                    }
                );
            }
        }
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn goal_tools_in_auto_skip_review_and_completion_is_checked_independently() {
    for accepted in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let model = Arc::new(ScriptedModel::new(vec![
            call(
                "create",
                "CreateGoal",
                json!({"objective":"Check the build"}),
            ),
            call("read", "GetGoal", json!({})),
            call(
                "complete",
                "UpdateGoal",
                json!({
                    "status":"complete", "evidence":"The build result is shown in this conversation."
                }),
            ),
            text(
                &json!({
                    "verdict":if accepted {"met"} else {"not_met"},
                    "reason":"Fixture independently checks the supplied evidence."
                })
                .to_string(),
            ),
            text("Reported."),
        ]));
        let agent = Agent::new_embedded(config(), model.clone());
        let reviewer = Arc::new(NativeTaskReviewer::default());
        agent.set_judgment_client(reviewer.clone()).await;
        let chat = agent.create_chat(root.path().into()).await.unwrap();
        agent
            .set_execution_mode(chat, ExecutionMode::Auto)
            .await
            .unwrap();
        turn(&agent, chat, "Set a goal to check the build.").await;
        let snapshot = agent.snapshot(chat).await.unwrap();
        for name in ["GetGoal", "CreateGoal", "UpdateGoal"] {
            let expected_state = if name == "UpdateGoal" && !accepted {
                tickloop_agent_protocol::OperationState::Failed
            } else {
                tickloop_agent_protocol::OperationState::Completed
            };
            assert!(
                snapshot.operations.iter().any(|operation| {
                    operation.name == name && operation.state == expected_state
                }),
                "{name} executed"
            );
        }
        assert!(
            !snapshot
                .request_usage
                .entries()
                .iter()
                .any(|entry| entry.kind
                    == tickloop_agent_protocol::ModelRequestKind::PermissionCheck)
        );
        let goal = snapshot.goal.unwrap();
        assert_eq!(
            goal.status == tickloop_agent_protocol::GoalStatus::Complete,
            accepted
        );
        assert_eq!(
            goal.last_check.unwrap().verdict == tickloop_agent_protocol::GoalVerdict::Met,
            accepted
        );
        assert!(model.requests.lock().unwrap().iter().any(|request| {
            request
                .structured_output
                .as_ref()
                .is_some_and(|output| output.schema["properties"]["verdict"].is_object())
        }));
        assert!(reviewer.requests.lock().unwrap().is_empty());
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn schedule_wakeup_in_auto_can_reschedule_and_finish_an_authorized_loop() {
    let root = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "wake",
            "ScheduleWakeup",
            json!({
                "delaySeconds":1200, "reason":"waiting for the build", "prompt":"Check the build"
            }),
        ),
        text("Scheduled the next check."),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    let reviewer = Arc::new(NativeTaskReviewer::default());
    agent.set_judgment_client(reviewer.clone()).await;
    let chat = agent.create_chat(root.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::Auto)
        .await
        .unwrap();
    let outcomes = turn(
        &agent,
        chat,
        "Run /loop to check the build at a suitable pace until it passes.",
    )
    .await;
    assert!(outcomes.contains(&ApprovalOutcome::Allow));
    assert_eq!(agent.snapshot(chat).await.unwrap().scheduled_tasks.len(), 1);
    model.enqueue(call("stop", "ScheduleWakeup", json!({"stop":true})));
    model.enqueue(text("The loop is finished."));
    let outcomes = turn(&agent, chat, "The build has passed.").await;
    assert!(outcomes.contains(&ApprovalOutcome::Allow));
    assert!(
        agent
            .snapshot(chat)
            .await
            .unwrap()
            .scheduled_tasks
            .is_empty()
    );
    assert_eq!(reviewer.requests.lock().unwrap().len(), 4);
    agent.shutdown().await;
}

#[tokio::test]
async fn plan_mode_reads_the_goal_but_does_not_set_one() {
    let root = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call("read", "GetGoal", json!({})),
        call(
            "create",
            "CreateGoal",
            json!({"objective":"Check the build"}),
        ),
        call(
            "complete",
            "UpdateGoal",
            json!({"status":"complete", "evidence":"Done"}),
        ),
        text("Still planning."),
    ]));
    let agent = Agent::new_embedded(config(), model);
    let reviewer = Arc::new(NativeTaskReviewer::default());
    agent.set_judgment_client(reviewer.clone()).await;
    let chat = agent.create_chat(root.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::Plan)
        .await
        .unwrap();
    turn(&agent, chat, "Review the goal while planning.").await;
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert!(snapshot.goal.is_none());
    let results = snapshot
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|content| {
            if let tickloop_agent_protocol::Content::ToolResult { is_error, .. } = content {
                Some(*is_error)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(results.len(), 3);
    assert!(!results[0], "GetGoal reads in Plan mode");
    assert!(results[1], "CreateGoal does not run in Plan mode");
    assert!(reviewer.requests.lock().unwrap().is_empty());
    agent.shutdown().await;
}

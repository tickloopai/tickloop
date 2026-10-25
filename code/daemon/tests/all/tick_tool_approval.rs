//! How ordinary Tick modifications go through approval in each execution mode. A controlled
//! Tick tool provider stands in for the Tickhost: it declares TickManage as the hosted tools do,
//! with the same reviewable permission and approval facts, and counts what reaches it.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{Value, json};
use tick_protocol::InstanceRef;
use tickloop_harness_model::judgment::{
    Answer, Evaluation, EvaluationRequest, JudgmentClient, JudgmentError, Question,
};
use tickloop_harness_runtime::{
    Agent,
    host_tools::{HostCallResult, HostToolPermission, HostToolProvider, PreparedHostCall},
};
use tokio_util::sync::CancellationToken;

use tickloop_harness_protocol::{
    ApprovalPath, ChatId, Decision, EventKind, ExecutionMode, LoopState, PermissionRule,
    PreparedApprovalFacts, ToolDefinition, ToolResult, Usage,
};
use tickloop_harness_test_support::{ScriptedModel, call, config, text};

/// TickManage as a Tick tool provider declares it, recording each call that was dispatched.
#[derive(Default)]
struct ControlledTicks {
    sent: Mutex<Vec<Value>>,
}

#[async_trait]
impl HostToolProvider for ControlledTicks {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "TickManage".into(),
            description: "Modify a Tick this Chat opened.".into(),
            input_schema: json!({"type":"object","properties":{"action":{"type":"string","enum":["send_input","close"]},"tick_instance_id":{"type":"string"},"text":{"type":"string"}},"required":["action","tick_instance_id"],"additionalProperties":false}),
        }]
    }

    fn permission(&self, _: ChatId, _: &str, _: &Value) -> HostToolPermission {
        HostToolPermission::Reviewable
    }

    async fn prepare_call(
        &self,
        _: ChatId,
        name: &str,
        input: &Value,
        _: CancellationToken,
    ) -> Result<PreparedHostCall, String> {
        let instance: InstanceRef = input["tick_instance_id"]
            .as_str()
            .ok_or("tick_instance_id is required")?
            .parse()
            .map_err(|error: &str| error.to_owned())?;
        Ok(PreparedHostCall::new(
            name.into(),
            input.clone(),
            input.clone(),
            HostToolPermission::Reviewable,
            format!("{} hosted Tick {instance}", input["action"]),
        )
        .with_approval_facts(PreparedApprovalFacts {
            normalized_input: input.clone(),
            closes_tick: input["action"] == "close",
            tick_instance: Some(instance),
            ..Default::default()
        }))
    }

    async fn execute_prepared(
        &self,
        _: ChatId,
        call: PreparedHostCall,
        _: CancellationToken,
    ) -> HostCallResult {
        self.sent.lock().unwrap().push(call.input().clone());
        HostCallResult::Completed(ToolResult {
            content: json!({"status":"completed"}).to_string(),
            is_error: false,
            output_ref: None,
        })
    }

    async fn execute(&self, _: ChatId, _: &str, _: Value, _: CancellationToken) -> ToolResult {
        ToolResult {
            content: "Tick tool calls require preparation".into(),
            is_error: true,
            output_ref: None,
        }
    }
}

fn target_instance() -> InstanceRef {
    InstanceRef::hosted(uuid::Uuid::from_u128(7))
}

struct ControlledJudgment {
    block: bool,
}

#[async_trait]
impl JudgmentClient for ControlledJudgment {
    fn model(&self) -> &str {
        "tick-test-jev"
    }

    async fn evaluate(
        &self,
        request: &EvaluationRequest,
        _: CancellationToken,
    ) -> Result<Evaluation, JudgmentError> {
        let answers = request
            .questions
            .iter()
            .map(|(id, question)| {
                let Question::Choice { criteria, .. } = question else {
                    panic!("expected typed approval choices")
                };
                let selected = if self.block { "block_soft" } else { "allow" };
                (
                    id.clone(),
                    Answer::Choice {
                        choice: selected.into(),
                        confidence: 1.0,
                        probabilities: criteria
                            .keys()
                            .map(|option| (option.clone(), f64::from(option == selected)))
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

#[tokio::test]
async fn tick_modification_mode_rules_and_human_recovery_apply_to_new_messages_for_both_reviewers()
{
    for use_jev in [false, true] {
        let host = Arc::new(ControlledTicks::default());
        let mut outputs = Vec::new();
        for step in 0..12 {
            outputs.push(call(&format!("input-{step}"), "TickManage", json!({"action":"send_input","tick_instance_id":target_instance(),"text":format!("input-{step}")})));
            outputs.push(text("Step completed."));
        }
        let agent = Agent::new_embedded(config(), Arc::new(ScriptedModel::new(outputs)));
        agent.install_host_tools(host.clone()).unwrap();
        if use_jev {
            agent
                .set_judgment_client(Arc::new(ControlledJudgment { block: true }))
                .await;
        } else {
            agent
                .set_auto_reviewer(Arc::new(ScriptedModel::new(vec![
                    text(
                        &tickloop_harness_test_support::auto_classification(true).to_string()
                    );
                    14
                ])))
                .await;
        }
        let directory = tempfile::tempdir().unwrap();
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        let mut events = agent.subscribe();
        let cases: [(ExecutionMode, &[Decision], bool, usize); 12] = [
            (ExecutionMode::Auto, &[], false, 0),
            (ExecutionMode::Auto, &[], false, 0),
            (ExecutionMode::FullAccess, &[], false, 1),
            (ExecutionMode::Auto, &[], false, 1),
            (ExecutionMode::Auto, &[Decision::Allow], false, 2),
            (
                ExecutionMode::FullAccess,
                &[Decision::Allow, Decision::Ask],
                true,
                3,
            ),
            (
                ExecutionMode::FullAccess,
                &[Decision::Allow, Decision::Ask, Decision::Deny],
                false,
                3,
            ),
            (ExecutionMode::AcceptEdits, &[], true, 4),
            (ExecutionMode::Auto, &[], false, 4),
            (ExecutionMode::Auto, &[], false, 4),
            (ExecutionMode::Auto, &[], true, 5),
            (ExecutionMode::Auto, &[], false, 5),
        ];
        for (step, (mode, decisions, should_prompt, expected_dispatches)) in
            cases.into_iter().enumerate()
        {
            agent.set_execution_mode(chat, mode).await.unwrap();
            agent
                .set_permission_rules(
                    chat,
                    decisions
                        .iter()
                        .map(|decision| PermissionRule {
                            id: uuid::Uuid::new_v4(),
                            decision: *decision,
                            tool: "TickManage".into(),
                            specifier: None,
                            input: None,
                            loop_id: None,
                            persistent: false,
                        })
                        .collect(),
                )
                .await
                .unwrap();
            agent
                .submit(chat, format!("Send input for step {step}."))
                .await
                .unwrap();
            let mut prompted = false;
            loop {
                match tickloop_harness_test_support::next_event(&mut events)
                    .await
                    .kind
                {
                    EventKind::PermissionRequested { request } => {
                        assert!(
                            should_prompt,
                            "unexpected human request at step {step}: {:?}",
                            request.approval_reason
                        );
                        assert!(!prompted);
                        prompted = true;
                        agent
                            .answer_permission(chat, request.id, true)
                            .await
                            .unwrap();
                    }
                    EventKind::Completed { .. } => break,
                    EventKind::Failed(error) => panic!("step {step} failed: {error}"),
                    _ => {}
                }
            }
            assert_eq!(prompted, should_prompt);
            assert_eq!(
                host.sent.lock().unwrap().len(),
                expected_dispatches,
                "step {step}"
            );
        }
        let history = agent.approval_history_view(chat).await.unwrap();
        let auto_path = if use_jev {
            ApprovalPath::AutoJev
        } else {
            ApprovalPath::AutoLlm
        };
        assert_eq!(
            history
                .records
                .iter()
                .filter(|record| record.path == auto_path)
                .count(),
            6
        );
        assert_eq!(history.records.len(), 12);
        agent.shutdown().await;
    }
}

async fn assert_mode_admits_tick_modification_without_human(mode: ExecutionMode, use_jev: bool) {
    let host = Arc::new(ControlledTicks::default());
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "close",
            "TickManage",
            json!({"action":"close","tick_instance_id":target_instance()}),
        ),
        text("Closed."),
    ]));
    let reviewer = Arc::new(ScriptedModel::new(vec![text(
        &tickloop_harness_test_support::auto_classification(false).to_string(),
    )]));
    let agent = Agent::new_embedded(config(), model);
    agent.install_host_tools(host.clone()).unwrap();
    if use_jev {
        agent
            .set_judgment_client(Arc::new(ControlledJudgment { block: false }))
            .await;
    } else {
        agent.set_auto_reviewer(reviewer.clone()).await;
    }
    let directory = tempfile::tempdir().unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    agent.set_execution_mode(chat, mode).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, "Close the selected Tick.".into())
        .await
        .unwrap();
    loop {
        match tickloop_harness_test_support::next_event(&mut events)
            .await
            .kind
        {
            EventKind::PermissionRequested { .. } => {
                panic!("{mode:?} requested a human for an ordinary Tick modification")
            }
            EventKind::ToolStarted { .. } => break,
            _ => {}
        }
    }
    wait_for_completion(&agent, chat.into()).await;
    assert_eq!(host.sent.lock().unwrap().len(), 1);
    assert_eq!(
        reviewer.requests.lock().unwrap().len(),
        usize::from(mode == ExecutionMode::Auto && !use_jev)
    );
    let history = agent.approval_history_view(chat).await.unwrap();
    assert_eq!(history.records.len(), 1);
    assert_eq!(
        history.records[0].path,
        match (mode, use_jev) {
            (ExecutionMode::Auto, true) => ApprovalPath::AutoJev,
            (ExecutionMode::Auto, false) => ApprovalPath::AutoLlm,
            _ => ApprovalPath::Direct,
        }
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn full_access_tick_modification_does_not_request_a_human() {
    assert_mode_admits_tick_modification_without_human(ExecutionMode::FullAccess, false).await;
}

#[tokio::test]
async fn auto_tick_modification_uses_reviewer_instead_of_forced_human_confirmation() {
    for use_jev in [false, true] {
        assert_mode_admits_tick_modification_without_human(ExecutionMode::Auto, use_jev).await;
    }
}

async fn wait_for_completion(agent: &Agent, chat: uuid::Uuid) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let chat_loop = agent.snapshot(chat.into()).await.unwrap().chat_loop;
            if chat_loop.state == LoopState::Idle && chat_loop.run_id.is_some() {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

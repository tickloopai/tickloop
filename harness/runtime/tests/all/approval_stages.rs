use serde_json::json;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
use tickloop_code_local_environment::AgentLocalExt;
use tickloop_harness_model::judgment::{
    Answer, Evaluation, EvaluationRequest, JudgmentClient, JudgmentError, Question,
};
use tickloop_harness_protocol::{ApprovalOutcome, ApprovalStage, EventKind, ExecutionMode, Usage};
use tickloop_harness_runtime::Agent;
use tickloop_harness_test_support::{
    ScriptedModel, auto_classification, call, config, next_event, text,
};
use tokio_util::sync::CancellationToken;

struct StagedJudgment {
    blocks: Vec<bool>,
    requests: Mutex<Vec<EvaluationRequest>>,
    cancel_authorization: Option<Arc<tokio::sync::Notify>>,
}

#[async_trait::async_trait]
impl JudgmentClient for StagedJudgment {
    fn model(&self) -> &str {
        "staged-jev"
    }
    async fn evaluate(
        &self,
        request: &EvaluationRequest,
        cancel: CancellationToken,
    ) -> Result<Evaluation, JudgmentError> {
        let (index, block) = {
            let mut requests = self.requests.lock().unwrap();
            let index = requests.len();
            let block = self.blocks[index];
            requests.push(request.clone());
            (index, block)
        };
        if index == 1
            && let Some(entered) = &self.cancel_authorization
        {
            entered.notify_one();
            cancel.cancelled().await;
            return Err(tickloop_harness_model::ModelError::Cancelled.into());
        }
        let choice = if block { "block_soft" } else { "allow" };
        let Question::Choice { criteria, .. } = &request.questions["decision"] else {
            panic!()
        };
        Ok(Evaluation {
            model: self.model().into(),
            usage: Usage::default(),
            answers: BTreeMap::from([(
                "decision".into(),
                Answer::Choice {
                    choice: choice.into(),
                    confidence: 1.0,
                    probabilities: criteria
                        .keys()
                        .map(|key| (key.clone(), f64::from(key == choice)))
                        .collect(),
                },
            )]),
        })
    }
}

#[tokio::test]
async fn both_backends_review_authorization_only_after_a_potential_risk() {
    for use_jev in [false, true] {
        for blocks in [vec![false], vec![true, false], vec![true, true]] {
            let directory = tempfile::tempdir().unwrap();
            let agent = Agent::new_embedded(
                config(),
                Arc::new(ScriptedModel::new(vec![
                    call("link", "Bash", json!({"command":"ln -s missing reviewed"})),
                    text("done"),
                ])),
            );
            agent.install_own_tickhost();
            let llm = Arc::new(ScriptedModel::new(
                blocks
                    .iter()
                    .map(|block| text(&auto_classification(*block).to_string()))
                    .collect(),
            ));
            let jev = Arc::new(StagedJudgment {
                blocks: blocks.clone(),
                requests: Mutex::new(Vec::new()),
                cancel_authorization: None,
            });
            if use_jev {
                agent.set_judgment_client(jev.clone()).await;
            } else {
                agent.set_auto_reviewer(llm.clone()).await;
            }
            let chat = agent.create_chat(directory.path().into()).await.unwrap();
            agent
                .set_execution_mode(chat, ExecutionMode::Auto)
                .await
                .unwrap();
            let mut events = agent.subscribe();
            agent
                .submit(chat, "Create exactly the reviewed symlink".into())
                .await
                .unwrap();
            let mut assessments = Vec::new();
            loop {
                match next_event(&mut events).await.kind {
                    EventKind::ApprovalAssessed { assessment } => assessments.push(*assessment),
                    EventKind::PermissionRequested { .. } => {
                        panic!("screening must not ask a human")
                    }
                    EventKind::Completed { .. } => break,
                    EventKind::Failed(error) => panic!("{error}"),
                    _ => {}
                }
            }
            let history = agent.recent_denials(chat).await.unwrap();
            agent.shutdown().await;
            let requests = if use_jev {
                jev.requests.lock().unwrap().len()
            } else {
                llm.requests.lock().unwrap().len()
            };
            assert_eq!(requests, blocks.len(), "jev={use_jev}, stages={blocks:?}");
            assert_eq!(
                assessments.len(),
                1,
                "a potential risk is not a final assessment"
            );
            let stages = assessments[0]
                .attempts
                .iter()
                .map(|attempt| attempt.stage)
                .collect::<Vec<_>>();
            assert_eq!(
                stages,
                if blocks.len() == 1 {
                    vec![Some(ApprovalStage::RiskScreen)]
                } else {
                    vec![
                        Some(ApprovalStage::RiskScreen),
                        Some(ApprovalStage::Authorization),
                    ]
                }
            );
            if blocks.len() == 2 {
                if use_jev {
                    let requests = jev.requests.lock().unwrap();
                    assert_eq!(requests[0].state, requests[1].state);
                } else {
                    let requests = llm.requests.lock().unwrap();
                    assert_eq!(requests[0].messages[1], requests[1].messages[1]);
                }
            }
            let blocked = *blocks.last().unwrap();
            assert_eq!(
                assessments[0].outcome,
                if blocked {
                    ApprovalOutcome::Block
                } else {
                    ApprovalOutcome::Allow
                }
            );
            assert_eq!(
                directory.path().join("reviewed").symlink_metadata().is_ok(),
                !blocked
            );
            assert_eq!(history.records.len(), usize::from(blocked));
        }
    }
}

#[tokio::test]
async fn twenty_screening_hits_cleared_by_authorization_never_trigger_denial_escalation() {
    for use_jev in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let mut replies = (0..20)
            .map(|index| {
                call(
                    &format!("link-{index}"),
                    "Bash",
                    json!({"command":format!("ln -s missing reviewed-{index}")}),
                )
            })
            .collect::<Vec<_>>();
        replies.push(text("done"));
        let agent = Agent::new_embedded(config(), Arc::new(ScriptedModel::new(replies)));
        agent.install_own_tickhost();
        let blocks = (0..20).flat_map(|_| [true, false]).collect::<Vec<_>>();
        if use_jev {
            agent
                .set_judgment_client(Arc::new(StagedJudgment {
                    blocks,
                    requests: Mutex::new(Vec::new()),
                    cancel_authorization: None,
                }))
                .await;
        } else {
            agent
                .set_auto_reviewer(Arc::new(ScriptedModel::new(
                    blocks
                        .into_iter()
                        .map(|block| text(&auto_classification(block).to_string()))
                        .collect(),
                )))
                .await;
        }
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        agent
            .set_execution_mode(chat, ExecutionMode::Auto)
            .await
            .unwrap();
        let mut events = agent.subscribe();
        agent
            .submit(chat, "Create exactly twenty reviewed symlinks".into())
            .await
            .unwrap();
        let mut assessments = 0;
        loop {
            match next_event(&mut events).await.kind {
                EventKind::ApprovalAssessed { assessment } => {
                    assessments += 1;
                    assert_eq!(assessment.outcome, ApprovalOutcome::Allow);
                }
                EventKind::PermissionRequested { .. } => {
                    panic!("potential risk was counted as a denial")
                }
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
        assert_eq!(assessments, 20);
        assert!(agent.recent_denials(chat).await.unwrap().records.is_empty());
        agent.shutdown().await;
        for index in 0..20 {
            assert!(
                directory
                    .path()
                    .join(format!("reviewed-{index}"))
                    .symlink_metadata()
                    .is_ok()
            );
        }
    }
}

struct CancelAuthorizationModel {
    entered: Arc<tokio::sync::Notify>,
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl tickloop_harness_model::ModelClient for CancelAuthorizationModel {
    fn identity(&self) -> tickloop_harness_protocol::ModelIdentity {
        ScriptedModel::new(Vec::new()).identity()
    }
    async fn complete(
        &self,
        _: tickloop_harness_model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_harness_model::ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<tickloop_harness_model::ModelOutput, tickloop_harness_model::ModelError> {
        if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 1 {
            self.entered.notify_one();
            cancel.cancelled().await;
            return Err(tickloop_harness_model::ModelError::Cancelled);
        }
        Ok(tickloop_harness_model::ModelOutput {
            private_source: None,
            content: text(&auto_classification(true).to_string()),
            usage: Usage::default(),
            stop_reason: "end_turn".into(),
        })
    }
}

#[tokio::test]
async fn cancellation_during_authorization_does_not_execute_or_record_a_screening_denial() {
    for use_jev in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let agent = Agent::new_embedded(
            config(),
            Arc::new(ScriptedModel::new(vec![
                call("link", "Bash", json!({"command":"ln -s missing reviewed"})),
                text("done"),
            ])),
        );
        let entered = Arc::new(tokio::sync::Notify::new());
        if use_jev {
            agent
                .set_judgment_client(Arc::new(StagedJudgment {
                    blocks: vec![true, false],
                    requests: Mutex::new(Vec::new()),
                    cancel_authorization: Some(entered.clone()),
                }))
                .await;
        } else {
            agent
                .set_auto_reviewer(Arc::new(CancelAuthorizationModel {
                    entered: entered.clone(),
                    calls: std::sync::atomic::AtomicUsize::new(0),
                }))
                .await;
        }
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        agent
            .set_execution_mode(chat, ExecutionMode::Auto)
            .await
            .unwrap();
        let mut events = agent.subscribe();
        agent
            .submit(chat, "Create the reviewed link".into())
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified())
            .await
            .unwrap();
        assert!(
            directory
                .path()
                .join("reviewed")
                .symlink_metadata()
                .is_err()
        );
        agent.interrupt(chat).await.unwrap();
        loop {
            match next_event(&mut events).await.kind {
                EventKind::Interrupted => break,
                EventKind::PermissionRequested { .. } | EventKind::ApprovalAssessed { .. } => {
                    panic!("an unfinished review has no final decision")
                }
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
        assert!(agent.recent_denials(chat).await.unwrap().records.is_empty());
        agent.shutdown().await;
        assert!(
            directory
                .path()
                .join("reviewed")
                .symlink_metadata()
                .is_err()
        );
    }
}

struct MalformedScreenJudgment {
    block: bool,
    requests: Mutex<Vec<EvaluationRequest>>,
}

#[async_trait::async_trait]
impl JudgmentClient for MalformedScreenJudgment {
    fn model(&self) -> &str {
        "malformed-screen-jev"
    }

    async fn evaluate(
        &self,
        request: &EvaluationRequest,
        _: CancellationToken,
    ) -> Result<Evaluation, JudgmentError> {
        self.requests.lock().unwrap().push(request.clone());
        let Question::Choice {
            instructions,
            criteria,
        } = &request.questions["decision"]
        else {
            panic!()
        };
        if instructions["stage"] == "risk_screen" {
            return Err(tickloop_harness_model::ModelError::InvalidResponse(
                "unparseable fixture result".into(),
            )
            .into());
        }
        let choice = if self.block { "block_soft" } else { "allow" };
        Ok(Evaluation {
            model: self.model().into(),
            usage: Usage::default(),
            answers: BTreeMap::from([(
                "decision".into(),
                Answer::Choice {
                    choice: choice.into(),
                    confidence: 1.0,
                    probabilities: criteria
                        .keys()
                        .map(|key| (key.clone(), f64::from(key == choice)))
                        .collect(),
                },
            )]),
        })
    }
}

#[tokio::test]
async fn malformed_risk_screen_can_recover_through_authorization_with_identical_evidence() {
    for use_jev in [false, true] {
        for block in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let agent = Agent::new_embedded(
                config(),
                Arc::new(ScriptedModel::new(vec![
                    call("link", "Bash", json!({"command":"ln -s missing recovered"})),
                    text("done"),
                ])),
            );
            agent.install_own_tickhost();
            let llm = Arc::new(ScriptedModel::new(vec![
                text("invalid"),
                text("invalid"),
                text(&auto_classification(block).to_string()),
            ]));
            let jev = Arc::new(MalformedScreenJudgment {
                block,
                requests: Mutex::new(Vec::new()),
            });
            if use_jev {
                agent.set_judgment_client(jev.clone()).await;
            } else {
                agent.set_auto_reviewer(llm.clone()).await;
            }
            let chat = agent.create_chat(directory.path().into()).await.unwrap();
            agent
                .set_execution_mode(chat, ExecutionMode::Auto)
                .await
                .unwrap();
            let mut events = agent.subscribe();
            agent
                .submit(chat, "Create the recovered symlink".into())
                .await
                .unwrap();
            let mut assessment = None;
            let mut prompted = false;
            loop {
                match next_event(&mut events).await.kind {
                    EventKind::PermissionRequested { request } => {
                        prompted = true;
                        agent
                            .answer_permission(chat, request.id, false)
                            .await
                            .unwrap();
                    }
                    EventKind::ApprovalAssessed { assessment: value } => assessment = Some(value),
                    EventKind::Completed { .. } | EventKind::Interrupted => break,
                    EventKind::Failed(error) => panic!("{error}"),
                    _ => {}
                }
            }
            let denials = agent.recent_denials(chat).await.unwrap();
            agent.shutdown().await;
            assert!(
                !prompted,
                "jev={use_jev}: screening parse errors should reach authorization"
            );
            assert_eq!(
                directory
                    .path()
                    .join("recovered")
                    .symlink_metadata()
                    .is_ok(),
                !block
            );
            assert_eq!(denials.records.len(), usize::from(block));
            let assessment = assessment.unwrap();
            assert_eq!(
                assessment.outcome,
                if block {
                    ApprovalOutcome::Block
                } else {
                    ApprovalOutcome::Allow
                }
            );
            assert_eq!(
                assessment
                    .attempts
                    .iter()
                    .map(|attempt| attempt.stage)
                    .collect::<Vec<_>>(),
                if use_jev {
                    vec![
                        Some(ApprovalStage::RiskScreen),
                        Some(ApprovalStage::Authorization),
                    ]
                } else {
                    vec![
                        Some(ApprovalStage::RiskScreen),
                        Some(ApprovalStage::RiskScreen),
                        Some(ApprovalStage::Authorization),
                    ]
                }
            );
            if use_jev {
                let requests = jev.requests.lock().unwrap();
                assert_eq!(requests.len(), 2);
                assert_eq!(requests[0].state, requests[1].state);
                assert!(assessment.fallback_from.is_none());
            } else {
                let requests = llm.requests.lock().unwrap();
                assert_eq!(requests.len(), 3);
                assert_eq!(requests[0].messages[1], requests[2].messages[1]);
            }
        }
    }
}

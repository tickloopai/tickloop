use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
use tickloop_code_local_environment::AgentLocalExt;
use tickloop_code_settings::local_agent::LocalAgent;

use serde_json::{Value, json};
use tickloop_harness::model::judgment::{
    Answer, Evaluation, EvaluationRequest, JudgmentClient, JudgmentError, Question,
};
use tickloop_harness::protocol::{
    ApprovalAssessmentDetails, ApprovalOutcome, ApprovalPath, ApprovalResult, ApprovalReviewerKind,
    EventKind, ExecutionMode, Usage,
};
use tickloop_harness::runtime::Agent;
use tickloop_harness::test_support::{
    ScriptedModel, call, config, next_event, text, transcript_file,
};
use tokio_util::sync::CancellationToken;

struct AllowingJudgment {
    requests: Mutex<Vec<EvaluationRequest>>,
}

#[async_trait::async_trait]
impl JudgmentClient for AllowingJudgment {
    fn model(&self) -> &str {
        "configured-jev"
    }

    async fn evaluate(
        &self,
        request: &EvaluationRequest,
        _: CancellationToken,
    ) -> Result<Evaluation, JudgmentError> {
        self.requests.lock().unwrap().push(request.clone());
        let answers = request
            .questions
            .iter()
            .map(|(id, question)| {
                let Question::Choice { criteria, .. } = question else {
                    panic!("the reviewer asked for a non-choice answer")
                };
                let selected = "allow";
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
            model: "reported-jev".into(),
            answers,
            usage: Usage::default(),
        })
    }
}

#[tokio::test]
async fn agent_reports_typed_assessment_for_each_actual_reviewer() {
    for use_jev in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let model = Arc::new(ScriptedModel::new(vec![
            call(
                "link",
                "Bash",
                json!({"command":"ln -s missing-target reviewed"}),
            ),
            text("done"),
        ]));
        let llm = Arc::new(ScriptedModel::new(vec![text(
            &tickloop_harness::test_support::auto_classification(false).to_string(),
        )]));
        let jev = Arc::new(AllowingJudgment {
            requests: Mutex::new(Vec::new()),
        });
        let agent = Agent::new_embedded(config(), model);
        agent.install_own_tickhost();
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
        let receipt = agent
            .submit(chat, "Create the reviewed symlink".into())
            .await
            .unwrap();
        let mut assessments = Vec::new();
        let mut started_reviewers = Vec::new();
        let mut recorded = Vec::new();
        loop {
            match next_event(&mut events).await.kind {
                EventKind::ApprovalReviewStarted { call_id, reviewer } => {
                    assert_eq!(call_id, "link");
                    assert!(assessments.is_empty());
                    started_reviewers.push(reviewer);
                }
                EventKind::ApprovalRecorded { record } => recorded.push(record),
                EventKind::ToolStarted { .. } => assert_eq!(recorded.len(), 1),
                EventKind::ApprovalAssessed { assessment } => assessments.push(assessment),
                EventKind::PermissionRequested { .. } => panic!("approved action asked the user"),
                EventKind::Failed(error) => panic!("review failed: {error}"),
                EventKind::Completed { .. } => break,
                _ => {}
            }
        }
        assert_eq!(
            started_reviewers,
            vec![if use_jev {
                ApprovalReviewerKind::Jev
            } else {
                ApprovalReviewerKind::Llm
            }]
        );
        assert_eq!(recorded.len(), 1);
        let [assessment] = assessments.as_slice() else {
            panic!("expected one assessment for one proposed operation")
        };
        assert_eq!(assessment.loop_id, receipt.loop_id);
        assert_eq!(assessment.run_id, receipt.run_id);
        assert_eq!(assessment.call_id, "link");
        let snapshot = agent.snapshot(chat).await.unwrap();
        assert_eq!(assessment.operation_id, snapshot.operations[0].id);
        assert_eq!(snapshot.approval_records, vec![*recorded[0].clone()]);
        assert_eq!(
            assessment.permission_policy_revision,
            snapshot.policy.revision
        );
        assert_eq!(assessment.outcome, ApprovalOutcome::Allow);
        assert_eq!(assessment.context_version, "trusted-approval-v11");
        assert_eq!(assessment.review_template_version, "auto-review-v19");
        let reviewer = assessment.reviewer.as_ref().unwrap();
        assert_eq!(
            reviewer.kind,
            if use_jev {
                ApprovalReviewerKind::Jev
            } else {
                ApprovalReviewerKind::Llm
            }
        );
        assert_eq!(
            reviewer.requested_model,
            if use_jev { "configured-jev" } else { "fixture" }
        );
        assert_eq!(
            reviewer.reported_model.as_deref(),
            use_jev.then_some("reported-jev")
        );
        assert!(directory.path().join("reviewed").symlink_metadata().is_ok());
        let history = agent.approval_history_view(chat).await.unwrap();
        assert_eq!(history.records.len(), 1);
        assert_eq!(history.records[0].outcome, ApprovalResult::Allow);
        assert_eq!(
            history.records[0].path,
            if use_jev {
                ApprovalPath::AutoJev
            } else {
                ApprovalPath::AutoLlm
            }
        );
        assert_eq!(history.records[0].operation_id, assessment.operation_id);
        assert!(
            history.records[0]
                .reason
                .contains(if use_jev { "Jev" } else { "LLM" })
        );
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn missing_original_evidence_reports_no_actual_reviewer() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("state");
    let first = Agent::open_embedded(
        root.clone(),
        config(),
        Arc::new(ScriptedModel::new(vec![text("first task finished")])),
    )
    .unwrap();
    let chat = first.create_chat(directory.path().into()).await.unwrap();
    let mut first_events = first.subscribe();
    first
        .submit(chat, "First task has a private restriction".into())
        .await
        .unwrap();
    loop {
        if matches!(
            next_event(&mut first_events).await.kind,
            EventKind::Completed { .. }
        ) {
            break;
        }
    }
    first.shutdown().await;
    drop(first);

    let path = transcript_file(&root, chat.uuid());
    let mut changed = false;
    let rewritten = std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(|line| {
            let mut entry: Value = serde_json::from_str(line).unwrap();
            if entry["type"] == "user" && entry["executionMode"].is_string() {
                changed = entry.as_object_mut().unwrap().remove("runId").is_some();
            }
            entry.to_string()
        })
        .collect::<Vec<_>>();
    assert!(changed);
    std::fs::write(&path, rewritten.join("\n") + "\n").unwrap();

    let model = Arc::new(ScriptedModel::new(vec![call(
        "link",
        "Bash",
        json!({"command":"ln -s missing-target reviewed"}),
    )]));
    let reviewer = Arc::new(ScriptedModel::new(Vec::new()));
    let resumed = Agent::open_embedded(root, config(), model).unwrap();
    resumed.resume_chat(chat).await.unwrap();
    resumed
        .set_execution_mode(chat, ExecutionMode::Auto)
        .await
        .unwrap();
    resumed.set_auto_reviewer(reviewer.clone()).await;
    let mut events = resumed.subscribe();
    resumed
        .submit(chat, "Continue the same Loop".into())
        .await
        .unwrap();
    let assessment: ApprovalAssessmentDetails = loop {
        match next_event(&mut events).await.kind {
            EventKind::ApprovalAssessed { assessment } => break *assessment,
            EventKind::ToolStarted { .. } => panic!("operation ran without original evidence"),
            EventKind::Failed(error) => panic!("run failed before assessment: {error}"),
            _ => {}
        }
    };
    assert_eq!(assessment.outcome, ApprovalOutcome::NeedsHuman);
    assert!(assessment.reviewer.is_none());
    assert!(reviewer.requests.lock().unwrap().is_empty());
    assert!(!directory.path().join("reviewed").exists());
    resumed.shutdown().await;
}

use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use tickloop_agent_settings::local_agent::LocalAgent;
use tickloop_local_environment::AgentLocalExt;

use serde_json::json;
use tickloop_agent_model::{
    ModelError,
    judgment::{Answer, Evaluation, EvaluationRequest, JudgmentClient, JudgmentError, Question},
};
use tickloop_agent_protocol::{
    AgentHostOptions, ApprovalFailureKind, ApprovalReviewerKind, Decision, DirectoryTrustPolicy,
    EventKind, ExecutionMode, ModelRequestKind, PermissionChange, PermissionRule, PromptPolicy,
    Protocol, RequestOutcome, Usage,
};
use tickloop_agent_runtime::Agent;
use tickloop_agent_test_support::{ScriptedModel, call, config, next_event, text};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy)]
enum Outcome {
    Allow,
    LowConfidence,
    MissingAnswer,
    RetryThenAllow,
    Authentication,
    Wait,
}

struct Reviewer {
    outcome: Outcome,
    calls: AtomicUsize,
    requests: Mutex<Vec<EvaluationRequest>>,
}

#[async_trait::async_trait]
impl JudgmentClient for Reviewer {
    fn model(&self) -> &str {
        "jev-test"
    }
    async fn evaluate(
        &self,
        request: &EvaluationRequest,
        cancel: CancellationToken,
    ) -> Result<Evaluation, JudgmentError> {
        let attempt = self.calls.fetch_add(1, Ordering::SeqCst);
        self.requests.lock().unwrap().push(request.clone());
        if matches!(self.outcome, Outcome::Authentication) {
            return Err(ModelError::Authentication.into());
        }
        if matches!(self.outcome, Outcome::Wait) {
            cancel.cancelled().await;
            return Err(ModelError::Cancelled.into());
        }
        if matches!(self.outcome, Outcome::RetryThenAllow) && attempt == 0 {
            return Err(ModelError::Http { status: 529 }.into());
        }
        let mut answers = BTreeMap::new();
        for (id, question) in &request.questions {
            let Question::Choice { criteria, .. } = question else {
                panic!()
            };
            let choice = "allow";
            answers.insert(
                id.clone(),
                Answer::Choice {
                    choice: choice.into(),
                    confidence: if id == "decision"
                        && matches!(self.outcome, Outcome::LowConfidence)
                    {
                        0.4
                    } else {
                        1.0
                    },
                    probabilities: criteria
                        .keys()
                        .map(|option| {
                            (
                                option.clone(),
                                if matches!(self.outcome, Outcome::LowConfidence) {
                                    if option == choice { 0.55 } else { 0.15 }
                                } else {
                                    f64::from(option == choice)
                                },
                            )
                        })
                        .collect(),
                },
            );
        }
        if matches!(self.outcome, Outcome::MissingAnswer) {
            answers.remove("decision");
        }
        Ok(Evaluation {
            model: "jev-test-1".into(),
            answers,
            usage: Usage {
                input_tokens: Some(321),
                output_tokens: Some(45),
                ..Default::default()
            },
        })
    }
}

async fn fixture(
    outcome: Outcome,
    prompt_policy: PromptPolicy,
) -> (tempfile::TempDir, Agent, Arc<Reviewer>, uuid::Uuid) {
    let root = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "link",
            "Bash",
            json!({"command":"ln -s missing-target reviewed"}),
        ),
        text(&tickloop_agent_test_support::auto_classification(false).to_string()),
        text("done"),
    ]));
    let agent = Agent::new_with_options(
        config(),
        model,
        AgentHostOptions {
            prompt_policy,
            directory_trust: DirectoryTrustPolicy::CallerManaged,
            ..Default::default()
        },
    );
    agent.install_own_tickhost();
    let reviewer = Arc::new(Reviewer {
        outcome,
        calls: AtomicUsize::new(0),
        requests: Mutex::new(Vec::new()),
    });
    agent.set_judgment_client(reviewer.clone()).await;
    let chat = agent.create_chat(root.path().into()).await.unwrap();
    assert_eq!(
        agent.snapshot(chat).await.unwrap().policy.mode,
        ExecutionMode::Auto
    );
    (root, agent, reviewer, chat.uuid())
}

#[tokio::test]
async fn selected_jev_configuration_failure_falls_back_to_the_conversation_model() {
    let directory = tempfile::tempdir().unwrap();
    let workdir = directory.path().canonicalize().unwrap();
    let settings_directory = tempfile::tempdir().unwrap();
    let root = settings_directory
        .path()
        .canonicalize()
        .unwrap()
        .join("state");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(
        root.join("settings.json"),
        r#"{"jev":{"endpoint":"invalid"}}"#,
    )
    .unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "link",
            "Bash",
            json!({"command":"ln -s missing-target reviewed"}),
        ),
        text(&tickloop_agent_test_support::auto_classification(false).to_string()),
        text("done"),
    ]));
    let agent = Agent::new_with_user_settings_embedded(
        config(),
        model.clone(),
        tickloop_agent_settings::UserSettings::in_root(&root),
    );
    agent.install_own_tickhost();
    let chat = agent.create_chat(workdir.clone()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::Auto)
        .await
        .unwrap();
    agent.save_jev_key("fixture-key").await.unwrap();
    let view = agent.permissions_view(chat).await.unwrap();
    agent
        .apply_permission_change(
            chat,
            PermissionChange::SelectReviewer {
                reviewer: tickloop_agent_protocol::AutoReviewerPreference::Jev,
            },
            view.revision,
        )
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(chat, "Create a symlink named reviewed".into())
        .await
        .unwrap();
    let mut checked = false;
    let mut assessment = None;
    loop {
        match next_event(&mut events).await.kind {
            EventKind::PermissionChecked { decision, reason } => {
                assert_eq!(decision, Decision::Allow);
                assert!(reason.contains("Auto policy"));
                checked = true;
            }
            EventKind::ApprovalAssessed { assessment: value } => assessment = Some(*value),
            EventKind::Completed { .. } => break,
            EventKind::PermissionRequested { request } => {
                panic!("unexpected human prompt: {request:?}")
            }
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    assert!(checked);
    let assessment = assessment.unwrap();
    assert_eq!(assessment.reviewer.unwrap().kind, ApprovalReviewerKind::Llm);
    assert_eq!(
        assessment.fallback_failure,
        Some(ApprovalFailureKind::Configuration)
    );
    assert_eq!(model.requests.lock().unwrap().len(), 3);
    assert!(workdir.join("reviewed").symlink_metadata().is_ok());
    agent.shutdown().await;
}

#[tokio::test]
async fn typed_allow_executes_and_records_separate_judgment_usage() {
    for outcome in [Outcome::Allow, Outcome::RetryThenAllow] {
        let (root, agent, reviewer, chat) = fixture(outcome, PromptPolicy::Interactive).await;
        let mut events = agent.subscribe();
        agent
            .submit(
                tickloop_ids::Id::from(chat),
                "Create a symlink named reviewed".into(),
            )
            .await
            .unwrap();
        loop {
            match next_event(&mut events).await.kind {
                EventKind::Completed { .. } => break,
                EventKind::PermissionRequested { .. } => panic!("confident review prompted"),
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
        assert!(root.path().join("reviewed").symlink_metadata().is_ok());
        let snapshot = agent.snapshot(tickloop_ids::Id::from(chat)).await.unwrap();
        let checks: Vec<_> = snapshot
            .request_usage
            .entries()
            .iter()
            .filter(|entry| entry.kind == ModelRequestKind::PermissionCheck)
            .collect();
        assert_eq!(
            checks.len(),
            if matches!(outcome, Outcome::RetryThenAllow) {
                2
            } else {
                1
            }
        );
        let accepted = checks.last().unwrap();
        assert_eq!(accepted.model.protocol, Protocol::SystemOne);
        assert_eq!(accepted.usage.input_tokens, Some(321));
        assert_eq!(accepted.outcome, RequestOutcome::Accepted);
        assert_eq!(
            reviewer.requests.lock().unwrap()[0].state["original_user_inputs"][0]["text"],
            json!("Create a symlink named reviewed")
        );
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn overall_allow_is_not_overridden_by_an_extra_confidence_gate() {
    for prompt_policy in [PromptPolicy::Interactive, PromptPolicy::DenyOnPrompt] {
        let (root, agent, reviewer, chat) = fixture(Outcome::LowConfidence, prompt_policy).await;
        let mut events = agent.subscribe();
        agent
            .submit(
                tickloop_ids::Id::from(chat),
                "Create a symlink named reviewed".into(),
            )
            .await
            .unwrap();
        loop {
            match next_event(&mut events).await.kind {
                EventKind::PermissionRequested { .. } => panic!("unexpected confidence gate"),
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
        assert!(root.path().join("reviewed").symlink_metadata().is_ok());
        assert_eq!(reviewer.requests.lock().unwrap()[0].questions.len(), 1);
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn invalid_answers_and_auth_failures_fall_back_to_the_chat_model() {
    for outcome in [Outcome::MissingAnswer, Outcome::Authentication] {
        let (root, agent, reviewer, chat) = fixture(outcome, PromptPolicy::Interactive).await;
        let mut events = agent.subscribe();
        agent
            .submit(
                tickloop_ids::Id::from(chat),
                "Create a symlink named reviewed".into(),
            )
            .await
            .unwrap();
        loop {
            match next_event(&mut events).await.kind {
                EventKind::Completed { .. } => break,
                EventKind::PermissionRequested { .. } => panic!("fallback model requested a human"),
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
        assert!(root.path().join("reviewed").symlink_metadata().is_ok());
        assert_eq!(
            reviewer.calls.load(Ordering::SeqCst),
            if matches!(outcome, Outcome::MissingAnswer) {
                2
            } else {
                1
            }
        );
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn explicit_deny_precedes_jev_and_cancellation_cannot_admit_an_action() {
    let (root, agent, reviewer, chat) = fixture(Outcome::Allow, PromptPolicy::Interactive).await;
    agent
        .set_permission_rules(
            tickloop_ids::Id::from(chat),
            vec![PermissionRule {
                id: uuid::Uuid::new_v4(),
                decision: Decision::Deny,
                tool: "Bash".into(),
                specifier: None,
                input: None,
                loop_id: None,
                persistent: false,
            }],
        )
        .await
        .unwrap();
    let mut events = agent.subscribe();
    agent
        .submit(
            tickloop_ids::Id::from(chat),
            "Create a symlink named reviewed".into(),
        )
        .await
        .unwrap();
    loop {
        if matches!(
            next_event(&mut events).await.kind,
            EventKind::Completed { .. }
        ) {
            break;
        }
    }
    assert_eq!(reviewer.calls.load(Ordering::SeqCst), 0);
    assert!(root.path().join("reviewed").symlink_metadata().is_err());
    agent.shutdown().await;

    let (root, agent, reviewer, chat) = fixture(Outcome::Wait, PromptPolicy::Interactive).await;
    agent
        .submit(
            tickloop_ids::Id::from(chat),
            "Create a symlink named reviewed".into(),
        )
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while reviewer.calls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    agent.interrupt(tickloop_ids::Id::from(chat)).await.unwrap();
    agent.shutdown().await;
    assert!(root.path().join("reviewed").symlink_metadata().is_err());
    assert!(
        agent
            .snapshot(tickloop_ids::Id::from(chat))
            .await
            .unwrap()
            .request_usage
            .entries()
            .iter()
            .any(|entry| entry.kind == ModelRequestKind::PermissionCheck
                && entry.outcome == RequestOutcome::Cancelled)
    );
}

use std::collections::BTreeMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use serde_json::json;
use tickloop_agent_model::{
    ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest,
    judgment::{Answer, Evaluation, EvaluationRequest, JudgmentClient, JudgmentError, Question},
};
use tickloop_agent_protocol::{
    AgentHostOptions, ApprovalAssessmentDetails, ApprovalFailureKind, ApprovalFallbackReason,
    ApprovalOutcome, ApprovalReviewerKind, AutoBlockKind, AutoReviewerHealth, AutoReviewerNotice,
    AutoRuleChange, AutoRuleKind, Content, DirectoryTrustPolicy, Event, EventKind, ExecutionMode,
    ModelIdentity, ModelProvider, PermissionChange, PromptPolicy, Protocol, Usage,
};
use tickloop_agent_runtime::Agent;
use tickloop_agent_test_support::{call, config, text};
use tokio::sync::{broadcast, mpsc::UnboundedSender};
use tokio_util::sync::CancellationToken;

struct ConversationModel {
    normal_requests: AtomicUsize,
    review_requests: Mutex<Vec<ModelRequest>>,
}

#[derive(Clone, Copy)]
enum FallbackBehavior {
    ContextLimit,
    WaitForCancel,
}

struct FallbackModel {
    behavior: FallbackBehavior,
    normal_requests: AtomicUsize,
    review_requests: AtomicUsize,
}

struct RepeatingModel {
    normal_requests: AtomicUsize,
    review_requests: AtomicUsize,
}

#[async_trait::async_trait]
impl ModelClient for RepeatingModel {
    async fn generate_title(
        &self,
        request: tickloop_agent_model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_agent_model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_agent_model::ModelOutput, tickloop_agent_model::ModelError> {
        Ok(tickloop_agent_test_support::title_output(request))
    }

    fn identity(&self) -> ModelIdentity {
        ModelIdentity {
            provider: ModelProvider::Api,
            profile: "repeated-chat".into(),
            protocol: Protocol::Messages,
            requested_model: "repeated-chat".into(),
        }
    }

    async fn complete(
        &self,
        request: ModelRequest,
        _: UnboundedSender<ModelDelta>,
        _: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        if request.structured_output.is_some() {
            self.review_requests.fetch_add(1, Ordering::SeqCst);
            return Err(ModelError::Connection);
        }
        let index = self.normal_requests.fetch_add(1, Ordering::SeqCst);
        Ok(ModelOutput {
            private_source: None,
            content: if index < 3 {
                call(
                    &format!("link-{index}"),
                    "Bash",
                    json!({"command":format!("ln -s missing-target reviewed-{index}")}),
                )
            } else {
                text("Review is unavailable; continue other work.")
            },
            usage: Usage::default(),
            stop_reason: "end_turn".into(),
        })
    }
}

#[async_trait::async_trait]
impl ModelClient for FallbackModel {
    async fn generate_title(
        &self,
        request: tickloop_agent_model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_agent_model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_agent_model::ModelOutput, tickloop_agent_model::ModelError> {
        Ok(tickloop_agent_test_support::title_output(request))
    }

    fn identity(&self) -> ModelIdentity {
        ModelIdentity {
            provider: ModelProvider::Api,
            profile: "fallback-chat".into(),
            protocol: Protocol::Messages,
            requested_model: "fallback-chat".into(),
        }
    }

    async fn complete(
        &self,
        request: ModelRequest,
        _: UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        if request.structured_output.is_some() {
            self.review_requests.fetch_add(1, Ordering::SeqCst);
            return match self.behavior {
                FallbackBehavior::ContextLimit => Err(ModelError::ResponseRejected(
                    tickloop_agent_model::ModelResponseRejection::ContextLimit,
                )),
                FallbackBehavior::WaitForCancel => {
                    cancel.cancelled().await;
                    Err(ModelError::Cancelled)
                }
            };
        }
        let index = self.normal_requests.fetch_add(1, Ordering::SeqCst);
        let content = if index == 0 {
            call(
                "link",
                "Bash",
                json!({"command":"ln -s missing-target reviewed"}),
            )
        } else {
            text("done")
        };
        Ok(ModelOutput {
            private_source: None,
            content,
            usage: Usage::default(),
            stop_reason: "end_turn".into(),
        })
    }
}

#[async_trait::async_trait]
impl ModelClient for ConversationModel {
    async fn generate_title(
        &self,
        request: tickloop_agent_model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_agent_model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_agent_model::ModelOutput, tickloop_agent_model::ModelError> {
        Ok(tickloop_agent_test_support::title_output(request))
    }

    fn identity(&self) -> ModelIdentity {
        ModelIdentity {
            provider: ModelProvider::Api,
            profile: "chat-review".into(),
            protocol: Protocol::Messages,
            requested_model: "chat-review".into(),
        }
    }

    async fn complete(
        &self,
        request: ModelRequest,
        _: UnboundedSender<ModelDelta>,
        _: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        let content = if request.structured_output.is_some() {
            let input = request
                .messages
                .iter()
                .flat_map(|message| &message.content)
                .find_map(|content| {
                    if let Content::Text { text } = content {
                        serde_json::from_str::<serde_json::Value>(text).ok()
                    } else {
                        None
                    }
                })
                .expect("review contains a JSON ApprovalSnapshot");
            let rules = input["auto_policy"]["rules"].as_array().unwrap().iter()
                .filter(|rule| rule["kind"] != "soft_allow")
                .map(|rule| json!({"id":rule["id"], "applicability":"absent", "clearance":"missing"}))
                .collect::<Vec<_>>();
            self.review_requests.lock().unwrap().push(request);
            text(&json!({"task_scope":"within_task", "rules":rules}).to_string())
        } else {
            let index = self.normal_requests.fetch_add(1, Ordering::SeqCst);
            if index.is_multiple_of(2) {
                call(
                    &format!("link-{index}"),
                    "Bash",
                    json!({"command":format!("ln -s missing-target link-{index}")}),
                )
            } else {
                text("done")
            }
        };
        Ok(ModelOutput {
            private_source: None,
            content,
            usage: Usage::default(),
            stop_reason: "end_turn".into(),
        })
    }
}

#[derive(Clone, Copy)]
enum JevBehavior {
    Authentication,
    AuthThenBlock,
    AuthThenWait,
    Block,
    Uncertain,
    Invalid,
    TransientThenBlock,
    Wait,
}

struct JevAdapter {
    behavior: JevBehavior,
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl JudgmentClient for JevAdapter {
    fn model(&self) -> &str {
        "jev-fixture"
    }

    async fn evaluate(
        &self,
        request: &EvaluationRequest,
        cancel: CancellationToken,
    ) -> Result<Evaluation, JudgmentError> {
        let call_index = self.calls.fetch_add(1, Ordering::SeqCst);
        match self.behavior {
            JevBehavior::Authentication => return Err(ModelError::Authentication.into()),
            JevBehavior::AuthThenBlock | JevBehavior::AuthThenWait if call_index == 0 => {
                return Err(ModelError::Authentication.into());
            }
            JevBehavior::TransientThenBlock if call_index == 0 => {
                return Err(ModelError::Http { status: 529 }.into());
            }
            JevBehavior::Wait | JevBehavior::AuthThenWait => {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(20)) => {},
                    _ = cancel.cancelled() => return Err(ModelError::Cancelled.into()),
                }
            }
            JevBehavior::AuthThenBlock
            | JevBehavior::Block
            | JevBehavior::Uncertain
            | JevBehavior::Invalid
            | JevBehavior::TransientThenBlock => {}
        }
        let answers = request
            .questions
            .iter()
            .map(|(id, question)| {
                let Question::Choice { criteria, .. } = question else {
                    panic!("expected Choice")
                };
                let selected = if id == "task_scope" {
                    if matches!(self.behavior, JevBehavior::Uncertain) {
                        "uncertain"
                    } else {
                        "outside_task"
                    }
                } else if id.starts_with("app:") {
                    "absent"
                } else {
                    "missing"
                };
                assert!(criteria.contains_key(selected));
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
            .collect::<BTreeMap<_, _>>();
        let mut answers = answers;
        if matches!(self.behavior, JevBehavior::Invalid) {
            answers.remove("task_scope");
        }
        Ok(Evaluation {
            model: "jev-fixture".into(),
            answers,
            usage: Usage {
                input_tokens: Some(9),
                ..Default::default()
            },
        })
    }
}

async fn fixture(
    behavior: JevBehavior,
) -> (
    tempfile::TempDir,
    Agent,
    Arc<ConversationModel>,
    Arc<JevAdapter>,
    uuid::Uuid,
) {
    let root = tempfile::tempdir().unwrap();
    let model = Arc::new(ConversationModel {
        normal_requests: AtomicUsize::new(0),
        review_requests: Mutex::new(Vec::new()),
    });
    let adapter = Arc::new(JevAdapter {
        behavior,
        calls: AtomicUsize::new(0),
    });
    let agent = Agent::new_embedded(config(), model.clone());
    agent.set_judgment_client(adapter.clone()).await;
    let chat = agent.create_chat(root.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::Auto)
        .await
        .unwrap();
    (root, agent, model, adapter, chat)
}

async fn completed(
    events: &mut broadcast::Receiver<Event>,
    run_id: uuid::Uuid,
) -> (ApprovalAssessmentDetails, Vec<AutoReviewerNotice>) {
    let mut assessment = None;
    let mut notices = Vec::new();
    let mut observed = Vec::new();
    loop {
        let event = events.recv().await.unwrap();
        if event.run_id != run_id {
            continue;
        }
        observed.push(format!("{:?}", event.kind));
        match event.kind {
            EventKind::ApprovalAssessed { assessment: value } => assessment = Some(*value),
            EventKind::AutoReviewerNotice { notice } => notices.push(notice),
            EventKind::Completed { .. } => {
                return (
                    assessment.unwrap_or_else(|| panic!("no assessment: {observed:?}")),
                    notices,
                );
            }
            EventKind::Failed(message) => panic!("run failed: {message}"),
            EventKind::PermissionRequested { .. } => {
                panic!("fallback unexpectedly requested a human")
            }
            _ => {}
        }
    }
}

#[tokio::test(start_paused = true)]
async fn authentication_failure_falls_back_then_cools_until_next_probe() {
    let (root, agent, model, jev, chat) = fixture(JevBehavior::Authentication).await;
    let mut events = agent.subscribe();
    let run = agent
        .submit(chat, None, "Create the requested symlink".into())
        .await
        .unwrap();
    let (first, notices) = completed(&mut events, run.run_id).await;
    assert_eq!(first.reviewer.unwrap().kind, ApprovalReviewerKind::Llm);
    assert_eq!(first.fallback_from, Some(ApprovalReviewerKind::Jev));
    assert_eq!(
        first.fallback_failure,
        Some(ApprovalFailureKind::Authentication)
    );
    assert_eq!(
        first.fallback_reason,
        Some(ApprovalFallbackReason::TechnicalFailure(
            ApprovalFailureKind::Authentication
        ))
    );
    assert_eq!(first.attempts.len(), 2);
    assert_eq!(notices.len(), 1);
    assert!(matches!(
        notices[0],
        AutoReviewerNotice::Fallback {
            failure: ApprovalFailureKind::Authentication
        }
    ));
    assert_eq!(jev.calls.load(Ordering::SeqCst), 1);
    assert!(matches!(
        agent.auto_reviewer_health().await,
        AutoReviewerHealth::CoolingDown { .. }
    ));

    let run = agent
        .submit(chat, None, "Create another requested symlink".into())
        .await
        .unwrap();
    let (second, notices) = completed(&mut events, run.run_id).await;
    assert_eq!(second.reviewer.unwrap().kind, ApprovalReviewerKind::Llm);
    assert_eq!(
        second.fallback_reason,
        Some(ApprovalFallbackReason::CoolingDown)
    );
    assert!(notices.is_empty());
    assert_eq!(jev.calls.load(Ordering::SeqCst), 1);

    tokio::time::advance(Duration::from_secs(60)).await;
    let run = agent
        .submit(chat, None, "Create one more requested symlink".into())
        .await
        .unwrap();
    let (_, notices) = completed(&mut events, run.run_id).await;
    assert!(notices.is_empty());
    assert_eq!(jev.calls.load(Ordering::SeqCst), 2);
    assert_eq!(model.review_requests.lock().unwrap().len(), 3);
    assert!(root.path().join("link-4").symlink_metadata().is_ok());
    agent.shutdown().await;
}

#[tokio::test]
async fn valid_jev_refusal_does_not_seek_llm_approval() {
    let (root, agent, model, jev, chat) = fixture(JevBehavior::Block).await;
    let mut events = agent.subscribe();
    let run = agent
        .submit(chat, None, "Create the requested symlink".into())
        .await
        .unwrap();
    let (assessment, notices) = completed(&mut events, run.run_id).await;
    assert_eq!(assessment.outcome, ApprovalOutcome::Block);
    assert_eq!(assessment.reviewer.unwrap().kind, ApprovalReviewerKind::Jev);
    assert_eq!(assessment.block_kind, Some(AutoBlockKind::TaskScope));
    assert!(assessment.fallback_from.is_none());
    assert!(notices.is_empty());
    assert_eq!(jev.calls.load(Ordering::SeqCst), 1);
    assert_eq!(model.review_requests.lock().unwrap().len(), 0);
    assert!(root.path().join("link-0").symlink_metadata().is_err());
    agent.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn jev_wait_uses_one_shared_ten_second_budget_before_llm_fallback() {
    let (_root, agent, _, jev, chat) = fixture(JevBehavior::Wait).await;
    let mut events = agent.subscribe();
    let run = agent
        .submit(chat, None, "Create the requested symlink".into())
        .await
        .unwrap();
    tokio::time::advance(Duration::from_secs(10)).await;
    let (assessment, _) = completed(&mut events, run.run_id).await;
    assert_eq!(assessment.reviewer.unwrap().kind, ApprovalReviewerKind::Llm);
    assert_eq!(
        assessment.fallback_failure,
        Some(ApprovalFailureKind::Timeout)
    );
    assert_eq!(jev.calls.load(Ordering::SeqCst), 1);
    assert_eq!(assessment.attempts.len(), 2);
    assert!(assessment.attempts[0].elapsed_ms <= 10_000);
    agent.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn transient_jev_failure_retries_once_within_budget_and_keeps_valid_refusal() {
    let (_root, agent, model, jev, chat) = fixture(JevBehavior::TransientThenBlock).await;
    let mut events = agent.subscribe();
    let run = agent
        .submit(chat, None, "Create the requested symlink".into())
        .await
        .unwrap();
    tokio::time::advance(Duration::from_millis(500)).await;
    let (assessment, notices) = completed(&mut events, run.run_id).await;
    assert_eq!(assessment.outcome, ApprovalOutcome::Block);
    assert_eq!(assessment.reviewer.unwrap().kind, ApprovalReviewerKind::Jev);
    assert_eq!(assessment.attempts.len(), 2);
    assert_eq!(jev.calls.load(Ordering::SeqCst), 2);
    assert_eq!(model.review_requests.lock().unwrap().len(), 0);
    assert!(notices.is_empty());
    agent.shutdown().await;
}

#[tokio::test]
async fn invalid_jev_response_falls_back_but_valid_uncertainty_does_not() {
    let (_root, agent, model, jev, chat) = fixture(JevBehavior::Invalid).await;
    let mut events = agent.subscribe();
    let run = agent
        .submit(chat, None, "Create the requested symlink".into())
        .await
        .unwrap();
    let (assessment, _) = completed(&mut events, run.run_id).await;
    assert_eq!(
        assessment.fallback_failure,
        Some(ApprovalFailureKind::InvalidResponse)
    );
    assert_eq!(assessment.reviewer.unwrap().kind, ApprovalReviewerKind::Llm);
    assert_eq!(jev.calls.load(Ordering::SeqCst), 1);
    assert_eq!(model.review_requests.lock().unwrap().len(), 1);
    agent.shutdown().await;

    let (_root, agent, model, jev, chat) = fixture(JevBehavior::Uncertain).await;
    let mut events = agent.subscribe();
    let run = agent
        .submit(chat, None, "Create the requested symlink".into())
        .await
        .unwrap();
    let assessment = loop {
        let event = events.recv().await.unwrap();
        if event.run_id != run.run_id {
            continue;
        }
        if let EventKind::ApprovalAssessed { assessment } = event.kind {
            break *assessment;
        }
    };
    assert_eq!(assessment.outcome, ApprovalOutcome::NeedsHuman);
    assert_eq!(assessment.reviewer.unwrap().kind, ApprovalReviewerKind::Jev);
    assert!(assessment.fallback_from.is_none());
    assert_eq!(jev.calls.load(Ordering::SeqCst), 1);
    assert_eq!(model.review_requests.lock().unwrap().len(), 0);
    agent.interrupt(chat).await.unwrap();
    agent.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn cancellation_during_jev_wait_does_not_fallback_or_change_health() {
    let (_root, agent, model, jev, chat) = fixture(JevBehavior::Wait).await;
    let mut events = agent.subscribe();
    let run = agent
        .submit(chat, None, "Create the requested symlink".into())
        .await
        .unwrap();
    while jev.calls.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
    agent.interrupt(chat).await.unwrap();
    loop {
        let event = events.recv().await.unwrap();
        if event.run_id != run.run_id {
            continue;
        }
        match event.kind {
            EventKind::Interrupted => break,
            EventKind::ApprovalAssessed { .. }
            | EventKind::AutoReviewerNotice { .. }
            | EventKind::ToolStarted { .. } => panic!("cancelled review had a side effect"),
            _ => {}
        }
    }
    assert_eq!(model.review_requests.lock().unwrap().len(), 0);
    assert_eq!(
        agent.auto_reviewer_health().await,
        AutoReviewerHealth::Ready
    );
    agent.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn replacing_connection_while_old_review_waits_ignores_its_late_failure() {
    let (_root, agent, model, old, chat) = fixture(JevBehavior::Wait).await;
    let mut events = agent.subscribe();
    let run = agent
        .submit(chat, None, "Create the requested symlink".into())
        .await
        .unwrap();
    while old.calls.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
    let replacement = Arc::new(JevAdapter {
        behavior: JevBehavior::Block,
        calls: AtomicUsize::new(0),
    });
    agent.set_judgment_client(replacement.clone()).await;
    tokio::time::advance(Duration::from_secs(10)).await;
    let (assessment, notices) = completed(&mut events, run.run_id).await;
    assert_eq!(assessment.reviewer.unwrap().kind, ApprovalReviewerKind::Llm);
    assert!(notices.is_empty());
    assert_eq!(
        agent.auto_reviewer_health().await,
        AutoReviewerHealth::Ready
    );

    let run = agent
        .submit(chat, None, "Create another requested symlink".into())
        .await
        .unwrap();
    let (assessment, _) = completed(&mut events, run.run_id).await;
    assert_eq!(assessment.reviewer.unwrap().kind, ApprovalReviewerKind::Jev);
    assert_eq!(replacement.calls.load(Ordering::SeqCst), 1);
    assert_eq!(model.review_requests.lock().unwrap().len(), 1);
    agent.shutdown().await;
}

#[tokio::test]
async fn jev_capacity_fallback_passes_full_custom_policy_to_chat_llm_without_denial() {
    let root = tempfile::tempdir().unwrap();
    let model = Arc::new(ConversationModel {
        normal_requests: AtomicUsize::new(0),
        review_requests: Mutex::new(Vec::new()),
    });
    let agent = Agent::open_embedded(
        root.path().canonicalize().unwrap().join("state"),
        config(),
        model.clone(),
    )
    .unwrap();
    let jev = Arc::new(JevAdapter {
        behavior: JevBehavior::Block,
        calls: AtomicUsize::new(0),
    });
    agent.set_judgment_client(jev.clone()).await;
    let chat = agent.create_chat(root.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::Auto)
        .await
        .unwrap();
    let large_rule = "Only use the requested target. ".repeat(850);
    let view = agent.permissions_view(chat).await.unwrap();
    agent
        .apply_permission_change(
            chat,
            PermissionChange::ChangeAutoRules {
                change: AutoRuleChange::Add {
                    kind: AutoRuleKind::SoftDeny,
                    text: large_rule.clone(),
                },
            },
            view.revision,
        )
        .await
        .unwrap();

    let mut events = agent.subscribe();
    let run = agent
        .submit(chat, None, "Create the requested symlink".into())
        .await
        .unwrap();
    let (assessment, notices) = completed(&mut events, run.run_id).await;
    assert_eq!(assessment.reviewer.unwrap().kind, ApprovalReviewerKind::Llm);
    assert_eq!(
        assessment.fallback_failure,
        Some(ApprovalFailureKind::ContextLimit)
    );
    assert_eq!(assessment.fallback_from, Some(ApprovalReviewerKind::Jev));
    assert_eq!(jev.calls.load(Ordering::SeqCst), 0);
    assert!(matches!(
        notices.as_slice(),
        [AutoReviewerNotice::Fallback {
            failure: ApprovalFailureKind::ContextLimit
        }]
    ));
    {
        let requests = model.review_requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let serialized = requests[0]
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .find_map(|content| {
                if let Content::Text { text } = content {
                    serde_json::from_str::<serde_json::Value>(text).ok()
                } else {
                    None
                }
            })
            .unwrap();
        let rules = serialized["auto_policy"]["rules"].as_array().unwrap();
        assert!(rules.iter().any(|rule| rule["text"] == large_rule));
        assert_eq!(
            serialized["original_user_inputs"][0]["text"],
            "Create the requested symlink"
        );
    }
    assert!(root.path().join("link-0").symlink_metadata().is_ok());
    assert!(agent.recent_denials(chat).await.unwrap().records.is_empty());
    agent.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn only_one_recovery_probe_runs_while_another_chat_uses_llm() {
    let (root, agent, _, jev, first_chat) = fixture(JevBehavior::AuthThenWait).await;
    let mut events = agent.subscribe();
    let first_run = agent
        .submit(first_chat, None, "Create the requested symlink".into())
        .await
        .unwrap();
    completed(&mut events, first_run.run_id).await;
    assert_eq!(jev.calls.load(Ordering::SeqCst), 1);

    let other_model = Arc::new(ConversationModel {
        normal_requests: AtomicUsize::new(0),
        review_requests: Mutex::new(Vec::new()),
    });
    let other_workdir = root.path().join("other");
    std::fs::create_dir(&other_workdir).unwrap();
    let other_chat = agent.create_chat(other_workdir).await.unwrap();
    agent
        .set_model(other_chat, config(), other_model.clone())
        .await
        .unwrap();
    agent
        .set_execution_mode(other_chat, ExecutionMode::Auto)
        .await
        .unwrap();

    tokio::time::advance(Duration::from_secs(60)).await;
    let mut probe_events = agent.subscribe();
    let probe = agent
        .submit(first_chat, None, "Create another requested symlink".into())
        .await
        .unwrap();
    while jev.calls.load(Ordering::SeqCst) < 2 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        agent.auto_reviewer_health().await,
        AutoReviewerHealth::Probing
    );

    let concurrent = agent
        .submit(
            other_chat,
            None,
            "Create a symlink in the other Chat".into(),
        )
        .await
        .unwrap();
    assert_eq!(probe.chat_id, first_chat);
    assert_eq!(concurrent.chat_id, other_chat);
    assert_ne!(probe.run_id, concurrent.run_id);
    let (concurrent_assessment, notices) = completed(&mut events, concurrent.run_id).await;
    assert_eq!(
        concurrent_assessment.reviewer.unwrap().kind,
        ApprovalReviewerKind::Llm
    );
    assert_eq!(
        concurrent_assessment.fallback_from,
        Some(ApprovalReviewerKind::Jev)
    );
    assert_eq!(
        concurrent_assessment.fallback_reason,
        Some(ApprovalFallbackReason::ProbeInProgress)
    );
    assert!(notices.is_empty());
    assert_eq!(other_model.review_requests.lock().unwrap().len(), 1);
    assert_eq!(jev.calls.load(Ordering::SeqCst), 2);

    tokio::time::advance(Duration::from_secs(10)).await;
    let (probe_assessment, notices) = completed(&mut probe_events, probe.run_id).await;
    assert_eq!(
        probe_assessment.fallback_failure,
        Some(ApprovalFailureKind::Timeout)
    );
    assert!(notices.is_empty());
    assert!(matches!(
        agent.auto_reviewer_health().await,
        AutoReviewerHealth::CoolingDown { .. }
    ));
    agent.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn valid_refusal_after_cooldown_recovers_jev_once_without_llm_fallback() {
    let (_root, agent, model, jev, chat) = fixture(JevBehavior::AuthThenBlock).await;
    let mut events = agent.subscribe();
    let first = agent
        .submit(chat, None, "Create the requested symlink".into())
        .await
        .unwrap();
    let (_, first_notices) = completed(&mut events, first.run_id).await;
    assert!(matches!(
        first_notices.as_slice(),
        [AutoReviewerNotice::Fallback { .. }]
    ));
    tokio::time::advance(Duration::from_secs(60)).await;

    let second = agent
        .submit(chat, None, "Create another requested symlink".into())
        .await
        .unwrap();
    let (assessment, notices) = completed(&mut events, second.run_id).await;
    assert_eq!(assessment.outcome, ApprovalOutcome::Block);
    assert_eq!(assessment.reviewer.unwrap().kind, ApprovalReviewerKind::Jev);
    assert!(matches!(
        notices.as_slice(),
        [AutoReviewerNotice::Recovered]
    ));
    assert_eq!(
        agent.auto_reviewer_health().await,
        AutoReviewerHealth::Ready
    );

    let third = agent
        .submit(chat, None, "Create one more requested symlink".into())
        .await
        .unwrap();
    let (_, notices) = completed(&mut events, third.run_id).await;
    assert!(notices.is_empty());
    assert_eq!(jev.calls.load(Ordering::SeqCst), 3);
    assert_eq!(model.review_requests.lock().unwrap().len(), 1);
    agent.shutdown().await;
}

async fn limited_fallback_fixture(
    behavior: FallbackBehavior,
    prompt_policy: PromptPolicy,
) -> (tempfile::TempDir, Agent, Arc<FallbackModel>, uuid::Uuid) {
    let root = tempfile::tempdir().unwrap();
    let model = Arc::new(FallbackModel {
        behavior,
        normal_requests: AtomicUsize::new(0),
        review_requests: AtomicUsize::new(0),
    });
    let agent = Agent::new_with_options(
        config(),
        model.clone(),
        AgentHostOptions {
            prompt_policy,
            directory_trust: DirectoryTrustPolicy::CallerManaged,
            ..Default::default()
        },
    );
    agent
        .set_judgment_client(Arc::new(JevAdapter {
            behavior: JevBehavior::Authentication,
            calls: AtomicUsize::new(0),
        }))
        .await;
    let chat = agent.create_chat(root.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::Auto)
        .await
        .unwrap();
    (root, agent, model, chat)
}

#[tokio::test]
async fn both_reviewers_at_capacity_ask_a_human_or_stop_headless() {
    for prompt_policy in [PromptPolicy::Interactive, PromptPolicy::DenyOnPrompt] {
        let (root, agent, model, chat) =
            limited_fallback_fixture(FallbackBehavior::ContextLimit, prompt_policy).await;
        let mut events = agent.subscribe();
        let run = agent
            .submit(chat, None, "Create the requested symlink".into())
            .await
            .unwrap();
        let mut assessment = None;
        let mut prompted = false;
        let mut stopped = false;
        loop {
            let event = events.recv().await.unwrap();
            if event.run_id != run.run_id {
                continue;
            }
            match event.kind {
                EventKind::ApprovalAssessed { assessment: value } => assessment = Some(*value),
                EventKind::PermissionRequested { request } => {
                    assert_eq!(prompt_policy, PromptPolicy::Interactive);
                    prompted = true;
                    agent
                        .answer_permission(chat, request.id, true)
                        .await
                        .unwrap();
                }
                EventKind::ToolStarted { .. } if prompt_policy == PromptPolicy::DenyOnPrompt => {
                    panic!("headless capacity failure admitted tool")
                }
                EventKind::Failed(_) => {
                    stopped = true;
                    break;
                }
                EventKind::Completed { .. } => break,
                _ => {}
            }
        }
        let assessment = assessment.unwrap();
        assert_eq!(assessment.outcome, ApprovalOutcome::ContextLimit);
        assert_eq!(assessment.reviewer.unwrap().kind, ApprovalReviewerKind::Llm);
        assert_eq!(
            assessment.fallback_failure,
            Some(ApprovalFailureKind::Authentication)
        );
        assert_eq!(model.review_requests.load(Ordering::SeqCst), 1);
        assert_eq!(prompted, prompt_policy == PromptPolicy::Interactive);
        assert_eq!(stopped, prompt_policy == PromptPolicy::DenyOnPrompt);
        assert_eq!(
            root.path().join("reviewed").symlink_metadata().is_ok(),
            prompted
        );
        assert_eq!(
            agent.recent_denials(chat).await.unwrap().records.len(),
            usize::from(stopped)
        );
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn cancelling_waiting_llm_fallback_leaves_no_denial_or_execution() {
    let (root, agent, model, chat) =
        limited_fallback_fixture(FallbackBehavior::WaitForCancel, PromptPolicy::DenyOnPrompt).await;
    let mut events = agent.subscribe();
    let run = agent
        .submit(chat, None, "Create the requested symlink".into())
        .await
        .unwrap();
    while model.review_requests.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
    agent.interrupt(chat).await.unwrap();
    loop {
        let event = events.recv().await.unwrap();
        if event.run_id != run.run_id {
            continue;
        }
        match event.kind {
            EventKind::Interrupted => break,
            EventKind::ApprovalAssessed { .. } | EventKind::ToolStarted { .. } => {
                panic!("cancelled fallback admitted or denied action")
            }
            _ => {}
        }
    }
    assert!(agent.recent_denials(chat).await.unwrap().records.is_empty());
    assert!(root.path().join("reviewed").symlink_metadata().is_err());
    agent.shutdown().await;
}

#[tokio::test]
async fn headless_auto_counts_valid_refusals_but_not_failed_reviews() {
    for behavior in [JevBehavior::Authentication, JevBehavior::Block] {
        let root = tempfile::tempdir().unwrap();
        let model = Arc::new(RepeatingModel {
            normal_requests: AtomicUsize::new(0),
            review_requests: AtomicUsize::new(0),
        });
        let agent = Agent::new_with_options(
            config(),
            model.clone(),
            AgentHostOptions {
                prompt_policy: PromptPolicy::DenyOnPrompt,
                directory_trust: DirectoryTrustPolicy::CallerManaged,
                ..Default::default()
            },
        );
        agent
            .set_judgment_client(Arc::new(JevAdapter {
                behavior,
                calls: AtomicUsize::new(0),
            }))
            .await;
        let chat = agent.create_chat(root.path().into()).await.unwrap();
        agent
            .set_execution_mode(chat, ExecutionMode::Auto)
            .await
            .unwrap();
        let mut events = agent.subscribe();
        let run = agent
            .submit(chat, None, "Create the requested symlinks".into())
            .await
            .unwrap();
        let mut assessments = Vec::new();
        let stopped = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let event = events.recv().await.unwrap();
                if event.run_id != run.run_id {
                    continue;
                }
                match event.kind {
                    EventKind::ApprovalAssessed { assessment } => assessments.push(*assessment),
                    EventKind::Failed(reason) => break Some(reason),
                    EventKind::Completed { .. } => break None,
                    EventKind::PermissionRequested { .. } | EventKind::ToolStarted { .. } => {
                        panic!("headless refusal opened an approval or executed a tool")
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("the bounded sequence should finish or reach the valid denial limit");
        if matches!(behavior, JevBehavior::Block) {
            let reason = stopped.expect("three valid refusals should stop headless execution");
            assert!(reason.contains("too many classifier denials"), "{reason}");
            assert!(
                assessments
                    .iter()
                    .all(|assessment| assessment.outcome == ApprovalOutcome::Block)
            );
        } else {
            assert!(
                stopped.is_none(),
                "technical failures must not reach the denial limit: {stopped:?}"
            );
            assert!(
                assessments
                    .iter()
                    .all(|assessment| assessment.outcome != ApprovalOutcome::Block)
            );
        }
        assert_eq!(assessments.len(), 3);
        assert_eq!(agent.recent_denials(chat).await.unwrap().records.len(), 3);
        assert_eq!(
            model.normal_requests.load(Ordering::SeqCst),
            if matches!(behavior, JevBehavior::Block) {
                3
            } else {
                4
            }
        );
        assert!(root.path().join("reviewed-0").symlink_metadata().is_err());
        agent.shutdown().await;
    }
}

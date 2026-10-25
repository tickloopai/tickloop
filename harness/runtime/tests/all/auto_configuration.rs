use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use tickloop_code_local_environment::AgentLocalExt;
use tickloop_code_settings::local_agent::LocalAgent;

use serde_json::json;
use tickloop_harness_model::{
    ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest,
    credentials::{load_api_key, save_api_key},
};
use tickloop_harness_protocol::{
    ApiKeyStatus, ApprovalAssessmentDetails, ApprovalReviewerKind, AutoReviewerPreference,
    EventKind, ExecutionMode, ModelConfig, ModelIdentity, ModelProvider, PermissionChange,
    Protocol, Usage,
};
use tickloop_harness_runtime::{Agent, PermissionChangeError};
use tickloop_harness_test_support::harness::{ChosenModels, embedded_agent};
use tickloop_harness_test_support::{call, config, next_event, text};
use tokio::sync::{Notify, mpsc::UnboundedSender};
use tokio_util::sync::CancellationToken;

struct ReviewGate {
    entered: Notify,
    release: Notify,
}

struct ChatModel {
    name: &'static str,
    requests: Mutex<Vec<ModelRequest>>,
    normal_requests: AtomicUsize,
    gate: Option<Arc<ReviewGate>>,
}

impl ChatModel {
    fn new(name: &'static str) -> Arc<Self> {
        Self::with_gate(name, None)
    }

    fn with_gate(name: &'static str, gate: Option<Arc<ReviewGate>>) -> Arc<Self> {
        Arc::new(Self {
            name,
            requests: Mutex::new(Vec::new()),
            normal_requests: AtomicUsize::new(0),
            gate,
        })
    }

    fn configuration(&self) -> ModelConfig {
        let mut config = config();
        config.name = self.name.into();
        config.model = self.name.into();
        config
    }

    fn review_count(&self) -> usize {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.structured_output.is_some())
            .count()
    }
}

#[async_trait::async_trait]
impl ModelClient for ChatModel {
    async fn generate_title(
        &self,
        request: tickloop_harness_model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_harness_model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_harness_model::ModelOutput, tickloop_harness_model::ModelError> {
        Ok(tickloop_harness_test_support::title_output(request))
    }

    fn identity(&self) -> ModelIdentity {
        ModelIdentity {
            provider: ModelProvider::Api,
            profile: self.name.into(),
            protocol: Protocol::Messages,
            requested_model: self.name.into(),
        }
    }

    async fn complete(
        &self,
        request: ModelRequest,
        _: UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        let is_review = request.structured_output.is_some();
        self.requests.lock().unwrap().push(request);
        let content = if is_review {
            if let Some(gate) = &self.gate {
                gate.entered.notify_one();
                tokio::select! {
                    _ = cancel.cancelled() => return Err(ModelError::Cancelled),
                    _ = gate.release.notified() => {}
                }
            }
            text(&tickloop_harness_test_support::auto_classification(false).to_string())
        } else {
            let index = self.normal_requests.fetch_add(1, Ordering::SeqCst);
            if index.is_multiple_of(2) {
                call(
                    &format!("{}-{index}", self.name),
                    "Bash",
                    json!({"command":format!("ln -s missing-target {}-{index}", self.name)}),
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

async fn finish(
    events: &mut tokio::sync::broadcast::Receiver<tickloop_harness_protocol::Event>,
    run: uuid::Uuid,
) -> ApprovalAssessmentDetails {
    let mut assessment = None;
    loop {
        let event = next_event(events).await;
        if event.run_id != run {
            continue;
        }
        match event.kind {
            EventKind::ApprovalAssessed { assessment: value } => assessment = Some(*value),
            EventKind::PermissionRequested { .. } => {
                panic!("an available approving LLM unexpectedly asked a human")
            }
            EventKind::Failed(error) => panic!("operation failed: {error}"),
            EventKind::Completed { .. } => {
                return assessment.expect("automatic review was observed");
            }
            _ => {}
        }
    }
}

#[tokio::test]
async fn default_llm_uses_the_chat_model_even_when_a_legacy_jev_key_exists() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap().join("state");
    save_api_key(&root, "TYPESAFE_API_KEY", "legacy-key-fixture").unwrap();
    let model = ChatModel::new("current-chat");
    let agent = Agent::open_embedded(root.clone(), model.configuration(), model.clone()).unwrap();
    agent.install_own_tickhost();
    let chat = agent.create_chat(temp.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::Auto)
        .await
        .unwrap();
    let view = agent.permissions_view(chat).await.unwrap();
    assert_eq!(view.preferred_reviewer, AutoReviewerPreference::Llm);
    assert_eq!(view.jev_key_status, ApiKeyStatus::Saved);
    assert_eq!(view.execution_mode, ExecutionMode::Auto);
    let mut events = agent.subscribe();
    let receipt = agent
        .submit(
            chat,
            "Create the requested symlink in this directory".into(),
        )
        .await
        .unwrap();
    let assessment = finish(&mut events, receipt.run_id.uuid()).await;
    assert_eq!(assessment.reviewer.unwrap().kind, ApprovalReviewerKind::Llm);
    assert_eq!(model.review_count(), 1);
    assert!(
        temp.path()
            .join("current-chat-0")
            .symlink_metadata()
            .is_ok()
    );
    {
        let requests = model.requests.lock().unwrap();
        let review = requests
            .iter()
            .find(|request| request.structured_output.is_some())
            .unwrap();
        assert!(review.tools.is_empty());
        assert!(!format!("{review:?}").contains("legacy-key-fixture"));
    }
    for mode in [
        ExecutionMode::Plan,
        ExecutionMode::AcceptEdits,
        ExecutionMode::Auto,
    ] {
        agent.set_execution_mode(chat, mode).await.unwrap();
        assert_eq!(
            agent
                .permissions_view(chat)
                .await
                .unwrap()
                .preferred_reviewer,
            AutoReviewerPreference::Llm
        );
    }
    agent.shutdown().await;
}

#[tokio::test]
async fn llm_approval_tracks_each_chat_effective_model_and_freezes_an_inflight_model() {
    let temp = tempfile::tempdir().unwrap();
    let first_model = ChatModel::new("first-chat-model");
    let other_model = ChatModel::new("other-chat-model");
    let models = ChosenModels::new(first_model.clone());
    let agent = embedded_agent(first_model.configuration(), models.clone());
    let first = agent.create_chat(temp.path().into()).await.unwrap();
    let other = agent.create_chat(temp.path().into()).await.unwrap();
    agent
        .set_execution_mode(first, ExecutionMode::Auto)
        .await
        .unwrap();
    models.choose(other_model.clone());
    agent
        .set_model(other, other_model.configuration())
        .await
        .unwrap();
    let mut events = agent.subscribe();
    let receipt = agent
        .submit(first, "Create a symlink".into())
        .await
        .unwrap();
    let assessment = finish(&mut events, receipt.run_id.uuid()).await;
    assert_eq!(
        assessment.reviewer.unwrap().requested_model,
        "first-chat-model"
    );
    assert_eq!(other_model.review_count(), 0);

    let gate = Arc::new(ReviewGate {
        entered: Notify::new(),
        release: Notify::new(),
    });
    let active_model = ChatModel::with_gate("active-chat-model", Some(gate.clone()));
    let pending_model = ChatModel::new("pending-chat-model");
    models.choose(active_model.clone());
    agent
        .set_model(first, active_model.configuration())
        .await
        .unwrap();
    let receipt = agent
        .submit(first, "Create another symlink".into())
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), gate.entered.notified())
        .await
        .unwrap();
    models.choose(pending_model.clone());
    agent
        .set_model(first, pending_model.configuration())
        .await
        .unwrap();
    gate.release.notify_one();
    let assessment = finish(&mut events, receipt.run_id.uuid()).await;
    assert_eq!(
        assessment.reviewer.unwrap().requested_model,
        "active-chat-model"
    );
    assert_eq!(pending_model.review_count(), 0);
    let receipt = agent
        .submit(first, "Create the next symlink".into())
        .await
        .unwrap();
    let assessment = finish(&mut events, receipt.run_id.uuid()).await;
    assert_eq!(
        assessment.reviewer.unwrap().requested_model,
        "pending-chat-model"
    );
    assert_eq!(first_model.review_count(), 1);
    assert_eq!(active_model.review_count(), 1);
    assert_eq!(pending_model.review_count(), 1);
    agent.shutdown().await;
}

#[tokio::test]
async fn global_reviewer_selection_preserves_modes_and_reports_cross_agent_conflicts() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap().join("state");
    let model = ChatModel::new("configuration-model");
    let first = Agent::open_embedded(root.clone(), model.configuration(), model.clone()).unwrap();
    let second = Agent::open_embedded(root.clone(), model.configuration(), model.clone()).unwrap();
    let a = first.create_chat(temp.path().into()).await.unwrap();
    let b = second.create_chat(temp.path().into()).await.unwrap();
    first
        .set_execution_mode(a, ExecutionMode::Plan)
        .await
        .unwrap();
    let initial = first.permissions_view(a).await.unwrap();
    let stale_second = second.permissions_view(b).await.unwrap();
    let select_jev = || PermissionChange::SelectReviewer {
        reviewer: AutoReviewerPreference::Jev,
    };
    assert!(
        first
            .apply_permission_change(a, select_jev(), initial.revision.clone())
            .await
            .is_err()
    );
    assert!(first.save_jev_key("invalid\nkey").await.is_err());
    assert_eq!(
        first.save_jev_key("shared-jev-fixture").await.unwrap(),
        ApiKeyStatus::Saved
    );
    let view = first.permissions_view(a).await.unwrap();
    let chosen = first
        .apply_permission_change(a, select_jev(), view.revision)
        .await
        .unwrap();
    assert_eq!(chosen.preferred_reviewer, AutoReviewerPreference::Jev);
    assert_eq!(chosen.execution_mode, ExecutionMode::Plan);
    let error = second
        .apply_permission_change(
            b,
            PermissionChange::SelectReviewer {
                reviewer: AutoReviewerPreference::Llm,
            },
            stale_second.revision,
        )
        .await
        .unwrap_err();
    let PermissionChangeError::Conflict { latest } = error else {
        panic!("expected a current projection on conflict")
    };
    assert_eq!(latest.preferred_reviewer, AutoReviewerPreference::Jev);
    assert_eq!(latest.execution_mode, ExecutionMode::AcceptEdits);
    assert!(!format!("{latest:?}").contains("shared-jev-fixture"));
    let path = root.join("settings.json");
    let mut settings: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    settings["unrelatedPreference"] = json!("keep-me");
    std::fs::write(&path, settings.to_string()).unwrap();
    let view = second.permissions_view(b).await.unwrap();
    second
        .apply_permission_change(
            b,
            PermissionChange::SelectReviewer {
                reviewer: AutoReviewerPreference::Llm,
            },
            view.revision,
        )
        .await
        .unwrap();
    assert_eq!(
        first.permissions_view(a).await.unwrap().preferred_reviewer,
        AutoReviewerPreference::Llm
    );
    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(saved["unrelatedPreference"], "keep-me");
    first.shutdown().await;
    second.shutdown().await;
}

#[tokio::test]
async fn a_saved_key_survives_a_failed_selection_without_changing_the_reviewer() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap().join("state");
    let model = ChatModel::new("configuration-model");
    let agent = Agent::open_embedded(root.clone(), model.configuration(), model).unwrap();
    let chat = agent.create_chat(temp.path().into()).await.unwrap();
    let view = agent.permissions_view(chat).await.unwrap();
    agent
        .save_jev_key("saved-before-selection-fails")
        .await
        .unwrap();
    let settings = root.join("settings.json");
    let before = std::fs::read(&settings).ok();
    if settings.exists() {
        std::fs::remove_file(&settings).unwrap();
    }
    std::fs::create_dir(&settings).unwrap();
    let error = agent
        .apply_permission_change(
            chat,
            PermissionChange::SelectReviewer {
                reviewer: AutoReviewerPreference::Jev,
            },
            view.revision,
        )
        .await
        .unwrap_err();
    assert!(!error.to_string().contains("saved-before-selection-fails"));
    assert_eq!(
        load_api_key(&root, "TYPESAFE_API_KEY").unwrap().as_deref(),
        Some("saved-before-selection-fails")
    );
    std::fs::remove_dir(&settings).unwrap();
    if let Some(before) = before {
        std::fs::write(&settings, before).unwrap();
    }
    assert_eq!(
        agent
            .permissions_view(chat)
            .await
            .unwrap()
            .preferred_reviewer,
        AutoReviewerPreference::Llm
    );
    agent.shutdown().await;
}

async fn read_judgment_request(socket: &mut tokio::net::TcpStream) -> (String, serde_json::Value) {
    use tokio::io::AsyncReadExt;
    let mut bytes = Vec::new();
    let mut buffer = [0; 4096];
    loop {
        let count = socket.read(&mut buffer).await.unwrap();
        assert_ne!(count, 0);
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(offset) = bytes.windows(4).position(|value| value == b"\r\n\r\n") {
            let headers = String::from_utf8(bytes[..offset].to_vec()).unwrap();
            let length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap();
            if bytes.len() >= offset + 4 + length {
                return (
                    headers,
                    serde_json::from_slice(&bytes[offset + 4..offset + 4 + length]).unwrap(),
                );
            }
        }
    }
}

fn approving_judgment(body: &serde_json::Value) -> serde_json::Value {
    let answers = body["questions"].as_object().unwrap().iter().map(|(id, question)| {
        let criteria = question["criteria"].as_object().unwrap();
        let choice = if id == "route" { "help" } else { "allow" };
        assert!(criteria.contains_key(choice), "fixture answer must be one of the requested options");
        let probabilities = criteria.keys().map(|key| (key.clone(), json!(f64::from(key == choice)))).collect::<serde_json::Map<_, _>>();
        (id.clone(), json!({"type":"choice","choice":choice,"confidence":1.0,"probabilities":probabilities}))
    }).collect::<serde_json::Map<_, _>>();
    json!({"model":format!("{}-reported",body["model"].as_str().unwrap()),"answers":answers,"usage":{"input_tokens":17,"output_tokens":9}})
}

#[tokio::test]
async fn jev_retry_keeps_its_connection_while_later_chats_and_generic_judgments_use_the_updated_key()
 {
    use std::collections::BTreeMap;
    use tickloop_harness_model::judgment::{
        EvaluationRequest, JevClient, JevConfig, JudgmentClient, Question,
    };
    use tokio::io::AsyncWriteExt;

    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap().join("state");
    save_api_key(&root, "TYPESAFE_API_KEY", "first-bound-key").unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1/systemone", listener.local_addr().unwrap());
    let settings_path = root.join("settings.json");
    std::fs::write(
        &settings_path,
        json!({"jev":{"endpoint":endpoint,"model":"jev-one"}}).to_string(),
    )
    .unwrap();
    let first_model = ChatModel::new("jev-first-chat");
    let next_model = ChatModel::new("jev-next-chat");
    let first = Agent::open_embedded(
        root.clone(),
        first_model.configuration(),
        first_model.clone(),
    )
    .unwrap();
    let next =
        Agent::open_embedded(root.clone(), next_model.configuration(), next_model.clone()).unwrap();
    let a = first.create_chat(temp.path().into()).await.unwrap();
    let b = next.create_chat(temp.path().into()).await.unwrap();
    first
        .set_execution_mode(a, ExecutionMode::Auto)
        .await
        .unwrap();
    next.set_execution_mode(b, ExecutionMode::Auto)
        .await
        .unwrap();
    let view = first.permissions_view(a).await.unwrap();
    first
        .apply_permission_change(
            a,
            PermissionChange::SelectReviewer {
                reviewer: AutoReviewerPreference::Jev,
            },
            view.revision,
        )
        .await
        .unwrap();
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let server_entered = entered.clone();
    let server_release = release.clone();
    let server = tokio::spawn(async move {
        let mut captured = Vec::new();
        for index in 0..4 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_judgment_request(&mut socket).await;
            let body = if index == 0 {
                server_entered.notify_one();
                server_release.notified().await;
                "temporary fixture failure".into()
            } else {
                approving_judgment(&request.1).to_string()
            };
            let status = if index == 0 { 503 } else { 200 };
            socket.write_all(format!("HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nRetry-After: 0\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            captured.push(request);
        }
        captured
    });
    let mut first_events = first.subscribe();
    let receipt = first
        .submit(a, "Create the first symlink".into())
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified())
        .await
        .unwrap();
    first.save_jev_key("second-bound-key").await.unwrap();
    let mut settings: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&settings_path).unwrap()).unwrap();
    settings["jev"]["model"] = json!("jev-two");
    std::fs::write(&settings_path, settings.to_string()).unwrap();
    release.notify_one();
    let assessment = finish(&mut first_events, receipt.run_id.uuid()).await;
    assert_eq!(assessment.reviewer.unwrap().requested_model, "jev-one");
    let mut next_events = next.subscribe();
    let receipt = next
        .submit(b, "Create the next symlink".into())
        .await
        .unwrap();
    let assessment = finish(&mut next_events, receipt.run_id.uuid()).await;
    assert_eq!(assessment.reviewer.unwrap().requested_model, "jev-two");
    assert_eq!(first_model.review_count() + next_model.review_count(), 0);
    let generic = JevClient::new(
        JevConfig {
            endpoint,
            model: "generic-routing".into(),
        },
        Some(root),
    )
    .unwrap();
    let request = EvaluationRequest {
        state: json!({"support_ticket":"I need help"}),
        questions: BTreeMap::from([(
            "route".into(),
            Question::Choice {
                instructions: json!("Choose a support team"),
                criteria: BTreeMap::from([
                    ("help".into(), json!("Support")),
                    ("sales".into(), json!("Sales")),
                ]),
            },
        )]),
    };
    generic
        .evaluate(&request, CancellationToken::new())
        .await
        .unwrap()
        .validate(&request)
        .unwrap();
    let requests = tokio::time::timeout(std::time::Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
    for (index, (headers, body)) in requests.iter().enumerate() {
        let key = if index < 2 {
            "first-bound-key"
        } else {
            "second-bound-key"
        };
        assert!(headers.contains(&format!("Bearer {key}")));
        assert!(!body.to_string().contains(key));
    }
    assert_eq!(
        requests[0].1, requests[1].1,
        "a retry must keep the entire original request"
    );
    assert_eq!(requests[2].1["model"], "jev-two");
    assert_eq!(requests[3].1["state"], request.state);
    first.shutdown().await;
    next.shutdown().await;
}

#[tokio::test]
async fn invalid_global_auto_policy_requires_a_human_without_calling_a_reviewer() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap().join("state");
    let model = ChatModel::new("invalid-policy-model");
    let agent = Agent::open_embedded(root.clone(), model.configuration(), model.clone()).unwrap();
    let chat = agent.create_chat(temp.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::Auto)
        .await
        .unwrap();
    std::fs::write(
        root.join("settings.json"),
        r#"{"autoApproval":{"reviewer":"invalid"}}"#,
    )
    .unwrap();
    assert!(agent.permissions_view(chat).await.is_err());
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, "Create a symlink".into()).await.unwrap();
    let mut asked = false;
    loop {
        let event = next_event(&mut events).await;
        if event.run_id != receipt.run_id {
            continue;
        }
        match event.kind {
            EventKind::PermissionRequested { request } => {
                asked = true;
                agent
                    .answer_permission(chat, request.id, false)
                    .await
                    .unwrap();
            }
            EventKind::Completed { .. } | EventKind::Interrupted => break,
            EventKind::Failed(error) => panic!("unexpected run failure: {error}"),
            _ => {}
        }
    }
    assert!(asked);
    assert_eq!(model.review_count(), 0);
    assert!(
        temp.path()
            .join("invalid-policy-model-0")
            .symlink_metadata()
            .is_err()
    );
    agent.shutdown().await;
}

//! Live classification at the Agent admission seam. Candidate commands are never dispatched.

use std::{collections::HashSet, path::Path, sync::Arc, time::Instant};

use serde_json::json;
use sha2::{Digest, Sha256};
use tickloop_harness_model::{
    ModelClient, ModelClients,
    judgment::{JevClient, JevConfig, JudgmentClient},
};
use tickloop_harness_protocol::{
    Content, EventKind, ExecutionMode, Message, OperationRecord, OperationState, Role,
};
use tickloop_harness_test_support::{ScriptedModel, config, next_event, text};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::Agent;
use tickloop_harness_tools::permissions::PermissionPolicyExt;

const DATASET: &str = include_str!("../tests/fixtures/approval-parity/benchmark-cases.tsv");
const DIALOGUES: &str = include_str!("../tests/fixtures/approval-parity/benchmark-dialogues.json");

struct RecordingReviewer {
    client: Arc<dyn ModelClient>,
    outputs: std::sync::Mutex<Vec<serde_json::Value>>,
}

impl RecordingReviewer {
    fn record(
        &self,
        result: &Result<tickloop_harness_model::ModelOutput, tickloop_harness_model::ModelError>,
    ) {
        if let Ok(output) = result {
            let text = output
                .content
                .iter()
                .filter_map(|block| match block {
                    Content::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<String>();
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
                self.outputs.lock().unwrap().push(value);
            }
        }
    }
}

#[async_trait::async_trait]
impl ModelClient for RecordingReviewer {
    fn connection_id(&self) -> Option<String> {
        self.client.connection_id()
    }
    fn identity(&self) -> tickloop_harness_protocol::ModelIdentity {
        self.client.identity()
    }
    fn supports_reasoning_effort(
        &self,
        effort: tickloop_harness_protocol::ReasoningEffort,
    ) -> bool {
        self.client.supports_reasoning_effort(effort)
    }
    async fn close_chat(&self, chat: tickloop_harness_protocol::ChatId) {
        self.client.close_chat(chat).await;
    }
    async fn complete(
        &self,
        request: tickloop_harness_model::ModelRequest,
        events: tokio::sync::mpsc::UnboundedSender<tickloop_harness_model::ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<tickloop_harness_model::ModelOutput, tickloop_harness_model::ModelError> {
        let result = self.client.complete(request, events, cancel).await;
        self.record(&result);
        result
    }
    async fn complete_for_chat_with_context(
        &self,
        chat: tickloop_harness_protocol::ChatId,
        request: tickloop_harness_model::ModelRequest,
        events: tokio::sync::mpsc::UnboundedSender<tickloop_harness_model::ModelDelta>,
        cancel: CancellationToken,
        context: tickloop_harness_model::ModelRequestContext,
    ) -> Result<tickloop_harness_model::ModelOutput, tickloop_harness_model::ModelError> {
        let result = self
            .client
            .complete_for_chat_with_context(chat, request, events, cancel, context)
            .await;
        self.record(&result);
        result
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Dialogue {
    initial: String,
    proposal: String,
    reply: String,
}

fn dialogues() -> std::collections::BTreeMap<String, Dialogue> {
    serde_json::from_str(DIALOGUES).unwrap()
}

struct Case<'a> {
    split: &'a str,
    expected: &'a str,
    id: &'a str,
    user: &'a str,
    command: &'a str,
    rationale: &'a str,
}

fn cases() -> Vec<Case<'static>> {
    DATASET
        .lines()
        .skip(1)
        .map(|line| {
            let columns = line.split('\t').collect::<Vec<_>>();
            assert_eq!(columns.len(), 6, "invalid benchmark row: {line}");
            Case {
                split: columns[0],
                expected: columns[1],
                id: columns[2],
                user: columns[3],
                command: columns[4],
                rationale: columns[5],
            }
        })
        .collect()
}

fn workspace(directory: &Path) -> tickloop_harness_tools::Workspace {
    tickloop_harness_tools::Workspace {
        folder: Some(directory.canonicalize().unwrap()),
        additional_directories: Vec::new(),
    }
}

fn prepare(case: &Case<'_>, directory: &Path) -> tickloop_harness_tools::PreparedTool {
    tickloop_harness_tools::prepare(
        &workspace(directory),
        "Bash",
        &json!({"command":case.command}),
    )
    .unwrap_or_else(|error| panic!("{}: {error}", case.id))
}

#[test]
fn benchmark_has_two_balanced_independent_frozen_sets() {
    let frozen: serde_json::Value = serde_json::from_str(include_str!(
        "../tests/fixtures/approval-parity/benchmark-freeze.json"
    ))
    .unwrap();
    assert_eq!(
        frozen["sha256"],
        format!("{:x}", Sha256::digest([DATASET, DIALOGUES].concat())),
        "frozen dataset changed; document and version any label correction before rerunning acceptance"
    );
    let cases = cases();
    assert_eq!(cases.len(), 200);
    let frozen_dialogues: std::collections::BTreeMap<String, serde_json::Value> =
        serde_json::from_str(DIALOGUES).unwrap();
    let mut acceptance = json!({
        "cases": cases.iter().filter(|case| case.split == "acceptance").map(|case| json!({
            "split":case.split, "expected":case.expected, "id":case.id, "user":case.user,
            "command":case.command, "rationale":case.rationale,
        })).collect::<Vec<_>>(),
        "dialogues": frozen_dialogues.into_iter().filter(|(key, _)| key.starts_with("acceptance/"))
            .collect::<std::collections::BTreeMap<_, _>>(),
    });
    acceptance.sort_all_objects();
    assert_eq!(
        frozen["acceptance_sha256"],
        format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&acceptance).unwrap())
        ),
        "the acceptance cases must remain frozen across development corrections"
    );
    let mut ids = HashSet::new();
    let mut operations = HashSet::new();
    let directory = tempfile::tempdir().unwrap();

    for case in &cases {
        assert!(matches!(case.split, "development" | "acceptance"));
        assert!(matches!(case.expected, "allow" | "block"));
        assert!(ids.insert((case.split, case.id)), "duplicate ID");
        assert!(
            operations.insert((case.user, case.command)),
            "duplicate operation and authority"
        );
        assert!(!case.rationale.is_empty());
        prepare(case, directory.path());
    }
    for split in ["development", "acceptance"] {
        for expected in ["allow", "block"] {
            assert_eq!(
                cases
                    .iter()
                    .filter(|case| case.split == split && case.expected == expected)
                    .count(),
                50,
                "{split}/{expected}"
            );
        }
    }
    for (key, dialogue) in dialogues() {
        assert!(
            cases
                .iter()
                .any(|case| key == format!("{}/{}", case.split, case.id))
        );
        assert!(
            !dialogue.initial.is_empty()
                && !dialogue.proposal.is_empty()
                && !dialogue.reply.is_empty()
        );
    }
}

fn install_declared_fixture(directory: &Path) {
    for (name, body) in [
        (
            "package.json",
            r#"{"name":"benchmark","devDependencies":{"typescript":"5.6.3"}}"#,
        ),
        (
            "package-lock.json",
            r#"{"name":"benchmark","lockfileVersion":3,"packages":{"":{"devDependencies":{"typescript":"5.6.3"}}}}"#,
        ),
        (
            "Cargo.toml",
            "[package]\nname = 'benchmark'\nversion = '0.1.0'\nedition = '2024'\n",
        ),
        ("Cargo.lock", "version = 4\n"),
        (
            "pyproject.toml",
            "[project]\nname = 'benchmark'\nversion = '0.1.0'\ndependencies = ['requests==2.32.3']\n",
        ),
        ("uv.lock", "version = 1\n"),
        ("requirements.txt", "requests==2.32.3\n"),
        (
            "Gemfile.lock",
            "GEM\n  specs:\n    rake (13.2.1)\nDEPENDENCIES\n  rake\n",
        ),
    ] {
        std::fs::write(directory.join(name), body).unwrap();
    }
}

async fn review_case(
    case: &Case<'_>,
    llm: Arc<dyn ModelClient>,
    jev: Option<Arc<dyn JudgmentClient>>,
) -> serde_json::Value {
    let llm = Arc::new(RecordingReviewer {
        client: llm,
        outputs: Default::default(),
    });
    let directory = tempfile::tempdir().unwrap();
    install_declared_fixture(directory.path());
    let prepared = prepare(case, directory.path());
    let policy = tickloop_harness_protocol::PermissionPolicy {
        mode: ExecutionMode::Auto,
        ..Default::default()
    };
    assert_eq!(
        policy.decide(&prepared, &workspace(directory.path()), Uuid::new_v4()),
        tickloop_harness_protocol::Decision::Check,
        "{} does not enter the classifier through production permission admission",
        case.id
    );
    let facts = prepared.approval_facts();
    let dialogue = dialogues().remove(&format!("{}/{}", case.split, case.id));
    let mut responses = Vec::new();
    if let Some(dialogue) = &dialogue {
        responses.push(text(&dialogue.proposal));
    }
    responses.push(text("I will evaluate the proposed operation."));
    let agent = Agent::new_embedded(config(), Arc::new(ScriptedModel::new(responses)));
    let chat_id = agent.create_chat(directory.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat_id, ExecutionMode::Auto)
        .await
        .unwrap();
    let mut events = agent.subscribe();
    let prompts = dialogue.as_ref().map_or_else(
        || vec![case.user],
        |dialogue| vec![dialogue.initial.as_str(), dialogue.reply.as_str()],
    );
    let mut receipt = None;
    for prompt in prompts {
        receipt = Some(agent.submit(chat_id, prompt.into()).await.unwrap());
        loop {
            match next_event(&mut events).await.kind {
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("benchmark setup failed: {error}"),
                _ => {}
            }
        }
    }
    let receipt = receipt.unwrap();
    let operation = OperationRecord {
        parent_call_id: None,
        task_binding: None,
        id: Uuid::new_v4(),
        loop_id: receipt.loop_id,
        run_id: receipt.run_id,
        call_id: "benchmark-candidate".into(),
        name: "Bash".into(),
        input: prepared.input,
        file_change: None,
        state: OperationState::Proposed,
        result: None,
    };
    let cancel = CancellationToken::new();
    {
        let mut chats = agent.inner.chats.lock().await;
        let chat = chats.get_mut(&chat_id).unwrap();
        chat.active_run = Some(receipt.run_id);
        chat.active = Some(cancel.clone());
        let index = chat.snapshot.messages.len();
        chat.snapshot.message_ids.insert(index, Uuid::new_v4());
        chat.snapshot.message_runs.insert(index, receipt.run_id);
        chat.snapshot.messages.push(Message {
            role: Role::Assistant,
            content: vec![Content::ToolUse {
                id: operation.call_id.clone(),
                name: operation.name.clone(),
                input: operation.input.clone(),
            }],
        });
        // The chat client is only the production fallback; the synthetic conversation is complete.
        chat.client = llm.clone();
    }
    let backend = if let Some(jev) = jev {
        agent.set_judgment_client(jev).await;
        "jev"
    } else {
        agent.set_auto_reviewer(llm.clone()).await;
        "llm"
    };
    let started = Instant::now();
    let assessment = agent
        .review_auto_operation(&receipt, &operation, Some(facts), cancel)
        .await;
    let elapsed_ms = started.elapsed().as_millis();
    let mut result = match assessment {
        Ok(assessment) => {
            let outcome = match assessment.verdict {
                super::AutoReview::Allow => "allow",
                super::AutoReview::Block { .. } => "block",
                _ => "error",
            };
            json!({
                "outcome":outcome,
                "details":assessment.details,
                "elapsed_ms":elapsed_ms,
            })
        }
        Err(error) => json!({"outcome":"error","error":error.to_string(),"elapsed_ms":elapsed_ms}),
    };
    if result["outcome"] != case.expected || result["details"]["fallback_from"].is_string() {
        result["classification_outputs"] = json!(llm.outputs.lock().unwrap().clone());
    }
    agent.shutdown().await;
    json!({
        "backend":backend, "split":case.split, "case":case.id, "expected":case.expected,
        "result":result, "admission":"check",
    })
}

#[tokio::test]
async fn benchmark_uses_both_production_stages_without_dispatching_the_candidate() {
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("must-remain");
    std::fs::write(&target, "not dispatched").unwrap();
    let command = format!(
        "python3 -c 'from pathlib import Path; Path({:?}).unlink()'",
        target.to_str().unwrap()
    );
    let user = format!("Remove the exact file {}", target.display());
    let case = Case {
        split: "development",
        expected: "allow",
        id: "harness-side-effect",
        user: &user,
        command: &command,
        rationale: "Harness verification",
    };
    let reviewer = Arc::new(ScriptedModel::new(vec![
        text(&tickloop_harness_test_support::auto_classification(true).to_string()),
        text(&tickloop_harness_test_support::auto_classification(false).to_string()),
    ]));
    let record = review_case(&case, reviewer.clone(), None).await;
    assert_eq!(record["result"]["outcome"], "allow", "{record}");
    assert_eq!(reviewer.requests.lock().unwrap().len(), 2);
    assert_eq!(
        record["result"]["details"]["attempts"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(std::fs::read_to_string(target).unwrap(), "not dispatched");
}

#[tokio::test]
async fn benchmark_preserves_the_proposal_for_a_short_confirmation() {
    let cases = cases();
    let case = cases
        .iter()
        .find(|case| case.id == "process-named-stop")
        .unwrap();
    let reviewer = Arc::new(ScriptedModel::new(vec![text(
        &tickloop_harness_test_support::auto_classification(false).to_string(),
    )]));
    let record = review_case(case, reviewer.clone(), None).await;
    assert_eq!(record["result"]["outcome"], "allow", "{record}");
    let requests = reviewer.requests.lock().unwrap();
    let Content::Text { text } = &requests[0].messages[1].content[0] else {
        panic!("missing review context");
    };
    let context: serde_json::Value = serde_json::from_str(text).unwrap();
    assert_eq!(context["reply_history"]["complete"], true);
    let inputs = context["original_user_inputs"].as_array().unwrap();
    let reply = inputs
        .iter()
        .find(|input| input["text"] == "Yes, stop it.")
        .unwrap();
    assert!(
        !inputs
            .iter()
            .any(|input| input["text"].as_str().unwrap().contains("8123"))
    );
    let history = context["reply_history"]["contexts"].as_array().unwrap();
    let linked = history
        .iter()
        .find(|entry| entry["user_message_id"] == reply["message_id"])
        .unwrap();
    assert!(
        linked["preceding"][0]["text"]
            .as_str()
            .unwrap()
            .contains("PID 8123")
    );
}

async fn live_benchmark(use_jev: bool) {
    let split = std::env::var("AUTO_BENCHMARK_SPLIT").unwrap_or_else(|_| "development".into());
    assert!(matches!(split.as_str(), "development" | "acceptance"));
    let selected = std::env::var("AUTO_BENCHMARK_CASES").ok();
    assert!(
        selected.is_none() || split == "development",
        "acceptance requires the complete set"
    );
    let repetitions: usize = std::env::var("AUTO_BENCHMARK_ROUNDS")
        .map_or(1, |value| value.parse().expect("invalid rounds"));
    assert!((1..=2).contains(&repetitions));
    let fixture = tempfile::tempdir().unwrap();
    install_declared_fixture(fixture.path());
    let policy = tickloop_harness_protocol::PermissionPolicy {
        mode: ExecutionMode::Auto,
        ..Default::default()
    };
    let bypasses = cases()
        .into_iter()
        .filter(|case| case.split == split)
        .filter(|case| {
            selected
                .as_ref()
                .is_none_or(|ids| ids.split(',').any(|id| id == case.id))
        })
        .filter(|case| {
            policy.decide(
                &prepare(case, fixture.path()),
                &workspace(fixture.path()),
                Uuid::new_v4(),
            ) != tickloop_harness_protocol::Decision::Check
        })
        .map(|case| case.id.to_owned())
        .collect::<Vec<_>>();
    assert!(
        bypasses.is_empty(),
        "classifier benchmark contains deterministic permission routes: {bypasses:?}; audit the corpus before spending model tokens"
    );
    let root = std::env::var_os("AUTO_BENCHMARK_CREDENTIAL_ROOT").map(std::path::PathBuf::from);
    let provider =
        std::env::var("AUTO_BENCHMARK_PROVIDER").unwrap_or_else(|_| "minimax-cn-oauth".into());
    let model = std::env::var("AUTO_BENCHMARK_MODEL").unwrap_or_else(|_| "MiniMax-M3".into());
    assert!(
        !provider.starts_with("deepseek"),
        "DeepSeek live testing was explicitly stopped by the user"
    );
    let config = tickloop_harness_model::cards::ModelCatalog::defaults()
        .get(&provider, &model)
        .expect("unknown benchmark provider/model selection");
    let isolated_login = benchmark_login_copy(&config, root.as_deref());
    let llm_root = isolated_login
        .as_ref()
        .map(|directory| directory.path().canonicalize().unwrap())
        .or_else(|| root.clone());
    let llm = tickloop_harness_model::LocalModelClients::new(llm_root.clone())
        .client(&config)
        .unwrap();
    let jev = use_jev.then(|| {
        Arc::new(JevClient::bind(JevConfig::default(), root).unwrap()) as Arc<dyn JudgmentClient>
    });
    let dataset_hash = format!("{:x}", Sha256::digest([DATASET, DIALOGUES].concat()));
    let template_hash = format!(
        "{:x}",
        Sha256::digest(
            [
                include_str!("auto_policy_guidance.md"),
                include_str!("auto_rules.rs"),
                include_str!("auto_mode.rs"),
                include_str!("../../model/src/lib.rs"),
                include_str!("../../model/src/structured_output.rs"),
                include_str!("../../model/src/structured_response.rs"),
                include_str!("../../model/src/chat_completions.rs"),
                include_str!("../../model/src/codex.rs"),
                include_str!("jev_review.rs"),
                include_str!("auto_approval.rs"),
                include_str!("model_request.rs"),
                include_str!("approval_context.rs"),
                include_str!("approval_context/reply_history.rs"),
                include_str!("lib.rs"),
            ]
            .concat()
        )
    );
    let policy_hash = format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&tickloop_harness_policy::AutoSettings::default().policy()).unwrap()
        )
    );
    let cases = cases()
        .into_iter()
        .filter(|case| case.split == split)
        .collect::<Vec<_>>();
    let selection = selected
        .as_ref()
        .map(|ids| ids.split(',').collect::<HashSet<_>>());
    if let Some(selection) = &selection {
        assert!(
            !selection.is_empty()
                && selection
                    .iter()
                    .all(|id| cases.iter().any(|case| case.id == *id)),
            "unknown development case"
        );
    }
    for round in 1..=repetitions {
        for case in &cases {
            if selection.as_ref().is_some_and(|ids| !ids.contains(case.id)) {
                continue;
            }
            let mut record = review_case(case, llm.clone(), jev.clone()).await;
            record["round"] = json!(round);
            record["dataset_sha256"] = json!(dataset_hash);
            record["policy_sha256"] = json!(policy_hash);
            record["template_sha256"] = json!(template_hash);
            record["llm_model"] = json!(config.model);
            record["llm_provider"] = json!(config.provider_id);
            record["llm_auth_source"] = json!(config.auth_source);
            record["model_card"] = json!(config.card);
            record["adapter"] = json!(config.adapter());
            record["full_set"] = json!(selection.is_none());
            println!("AUTO_BENCHMARK {record}");
        }
    }
}

#[tokio::test]
#[ignore = "real Jev and fallback LLM; synthetic candidates are never executed"]
async fn live_agent_benchmark_jev() {
    live_benchmark(true).await;
}

#[tokio::test]
#[ignore = "real LLM; synthetic candidates are never executed"]
async fn live_agent_benchmark_llm() {
    live_benchmark(false).await;
}

#[test]
fn benchmark_admission_distinguishes_workspace_copy_from_external_source_copy() {
    let directory = tempfile::tempdir().unwrap();
    let policy = tickloop_harness_protocol::PermissionPolicy {
        mode: ExecutionMode::Auto,
        ..Default::default()
    };
    for (id, expected) in [
        ("files-copy", tickloop_harness_protocol::Decision::Allow),
        (
            "agent-local-transcript",
            tickloop_harness_protocol::Decision::Check,
        ),
    ] {
        let cases = cases();
        let case = cases
            .iter()
            .find(|case| case.split == "development" && case.id == id)
            .unwrap();
        assert_eq!(
            policy.decide(
                &prepare(case, directory.path()),
                &workspace(directory.path()),
                Uuid::new_v4()
            ),
            expected,
            "{id}"
        );
    }
    for case in cases() {
        let decision = policy.decide(
            &prepare(&case, directory.path()),
            &workspace(directory.path()),
            Uuid::new_v4(),
        );
        if decision != tickloop_harness_protocol::Decision::Check {
            println!(
                "AUTO_BENCHMARK_ROUTING {}/{} {decision:?}",
                case.split, case.id
            );
        }
    }
}

fn benchmark_auth_file(auth: tickloop_harness_protocol::ModelAuthSource) -> Option<&'static str> {
    use tickloop_harness_protocol::ModelAuthSource;
    match auth {
        ModelAuthSource::MiniMaxOAuthCn => Some("minimax-cn.json"),
        ModelAuthSource::MiniMaxOAuthGlobal => Some("minimax-global.json"),
        ModelAuthSource::CodexSubscription => Some("openai-codex-credentials.json"),
        ModelAuthSource::KimiOAuth => Some("kimi-code-oauth.json"),
        _ => None,
    }
}

fn benchmark_login_copy(
    config: &tickloop_harness_protocol::ModelConfig,
    root: Option<&Path>,
) -> Option<tempfile::TempDir> {
    if config.auth_source == tickloop_harness_protocol::ModelAuthSource::ApiKeyEnv {
        return None;
    }
    use std::{
        io::Write,
        os::unix::fs::{OpenOptionsExt, PermissionsExt},
    };
    let name = benchmark_auth_file(config.auth_source)
        .expect("benchmark does not copy this authentication type");
    let root = root.expect("subscription benchmarking requires AUTO_BENCHMARK_CREDENTIAL_ROOT");
    let source = std::fs::read(root.join("auth").join(name)).expect("no saved login");
    let state: serde_json::Value = serde_json::from_slice(&source).unwrap();
    let credentials = state.get("credentials").unwrap_or(&state);
    let minimum_expiry =
        tickloop_harness_base::private_fs::unix_time_milliseconds() + 24 * 60 * 60 * 1000;
    assert!(
        credentials["expires_at_ms"]
            .as_u64()
            .is_some_and(|expiry| expiry > minimum_expiry),
        "saved subscription needs renewal before benchmarking; do not rotate refresh tokens in a copied login"
    );
    let directory = tempfile::tempdir().unwrap();
    let auth_directory = directory.path().join("auth");
    std::fs::create_dir(&auth_directory).unwrap();
    std::fs::set_permissions(&auth_directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut files = vec![(name, source)];
    if config.auth_source == tickloop_harness_protocol::ModelAuthSource::CodexSubscription {
        let name = "openai-codex-generation.json";
        if let Ok(source) = std::fs::read(root.join("auth").join(name)) {
            files.push((name, source));
        }
    }
    for (name, source) in files {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(auth_directory.join(name))
            .unwrap()
            .write_all(&source)
            .unwrap();
    }
    Some(directory)
}

#[test]
#[ignore = "read-only local credential inventory; never prints secrets or calls a model"]
fn live_benchmark_inventory() {
    let root = std::env::var_os("AUTO_BENCHMARK_CREDENTIAL_ROOT")
        .map(std::path::PathBuf::from)
        .expect("credential root required");
    for config in tickloop_harness_model::cards::ModelCatalog::defaults().profiles() {
        if config.provider_id.starts_with("deepseek") {
            continue;
        }
        let readiness = if config.auth_source
            == tickloop_harness_protocol::ModelAuthSource::ApiKeyEnv
        {
            if config.api_key_env.is_empty() {
                continue;
            }
            let status =
                tickloop_harness_model::credentials::api_key_status(&root, &config.api_key_env)
                    .unwrap();
            if status == tickloop_harness_protocol::ApiKeyStatus::Missing {
                continue;
            }
            "key_present"
        } else if let Some(name) = benchmark_auth_file(config.auth_source) {
            let Ok(bytes) = std::fs::read(root.join("auth").join(name)) else {
                continue;
            };
            let state: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            let credentials = state.get("credentials").unwrap_or(&state);
            if credentials.is_null() {
                continue;
            }
            let expiry = credentials["expires_at_ms"].as_u64().unwrap_or_default();
            if expiry
                > tickloop_harness_base::private_fs::unix_time_milliseconds() + 24 * 60 * 60 * 1000
            {
                "saved_login_valid"
            } else {
                "saved_login_needs_renewal"
            }
        } else {
            continue;
        };
        println!(
            "AUTO_BENCHMARK_MODEL {}",
            json!({"provider":config.provider_id,"model_id":config.model_id,"model":config.model,"name":config.name,"auth":config.auth_source,"readiness":readiness})
        );
    }
}

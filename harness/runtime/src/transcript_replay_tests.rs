//! Explicit live review of recorded operations. No tool executor is installed or called.

use std::{
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::PathBuf,
    sync::Arc,
    time::Instant,
};

use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tickloop_harness_artifacts::ArtifactClient;
use tickloop_harness_model::{ModelClients, judgment::JevClient};
use tickloop_harness_protocol::{Content, ExecutionMode, OperationRecord, OperationState, Receipt};
use tickloop_harness_test_support::{ScriptedModel, config};
use tokio_util::sync::CancellationToken;

use crate::{
    Agent, AutoReview, ChatBookkeeping, approval_context, auto_approval, empty_snapshot,
    host_tools::HostToolProvider, jev_review, mcp_tools::McpHostToolProvider, transcript,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplayConfig {
    transcript: PathBuf,
    credential_root: PathBuf,
    operations: Vec<PathBuf>,
    auto_schema_version: u64,
    auto_reviewer: tickloop_harness_protocol::AutoReviewerPreference,
    auto_rules: tickloop_harness_protocol::AutoRuleSettings,
    auto_revision: tickloop_harness_protocol::AutoSettingsRevision,
    jev: tickloop_harness_model::judgment::JevConfig,
    model: tickloop_harness_protocol::ModelConfig,
    #[serde(default)]
    llm_fallback: bool,
}

fn private_json(path: &std::path::Path, value: &Value) {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    file.write_all(&serde_json::to_vec_pretty(value).unwrap())
        .unwrap();
}

#[tokio::test]
#[ignore = "reads the private transcript-replay request.json and calls real Jev; never dispatches tools"]
async fn live_recorded_artifact_reviews_without_dispatch() {
    let output = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../.scratch/approval-parity/results/transcript-replay");
    let request: ReplayConfig =
        serde_json::from_slice(&std::fs::read(output.join("request.json")).unwrap()).unwrap();
    let source_bytes = std::fs::read(&request.transcript).unwrap();
    let source_hash = format!("{:x}", Sha256::digest(&source_bytes));
    let lines = std::str::from_utf8(&source_bytes)
        .unwrap()
        .lines()
        .collect::<Vec<_>>();
    let auto = tickloop_harness_policy::AutoSettings {
        schema_version: request.auto_schema_version,
        reviewer: request.auto_reviewer,
        rules: request.auto_rules.clone(),
    };
    let revision = request.auto_revision.clone();
    let jev_config = request.jev.clone();
    let llm_config = request.model.clone();

    let credentials = tempfile::tempdir().unwrap();
    let credential_root = credentials.path().canonicalize().unwrap();
    std::fs::set_permissions(&credential_root, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::copy(
        request.credential_root.join(".env"),
        credential_root.join(".env"),
    )
    .unwrap();
    std::fs::set_permissions(
        credential_root.join(".env"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let original_artifacts = ArtifactClient::new(&request.credential_root).unwrap();
    let credential = original_artifacts.credential_snapshot().unwrap().unwrap();
    let credential_file = format!("{:x}.json", Sha256::digest(credential.origin.as_bytes()));
    let auth_directory = credential_root.join("artifacts/auth");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&auth_directory)
        .unwrap();
    std::fs::copy(
        request
            .credential_root
            .join("artifacts/auth")
            .join(&credential_file),
        auth_directory.join(credential_file),
    )
    .unwrap();
    let provider = McpHostToolProvider::new(
        ArtifactClient::for_origin(&credential_root, &credential.origin).unwrap(),
        None,
    );
    provider.refresh(CancellationToken::new()).await.unwrap();
    assert_eq!(
        provider.service_connections()["artifacts"]["tool_directory"],
        "available",
        "read-only Artifact preparation is unavailable"
    );
    let client = Arc::new(JevClient::bind(jev_config, Some(credential_root.clone())).unwrap());
    let llm = if request.llm_fallback {
        assert_eq!(llm_config.provider_id, "xai-oauth");
        let login = request.credential_root.join("auth/xai-oauth.json");
        let bytes = std::fs::read(login).unwrap();
        let state: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            state["credentials"]["expires_at_ms"].as_u64().unwrap()
                > tickloop_harness_base::private_fs::unix_time_milliseconds() + 600_000,
            "the replay must not refresh a copied OAuth login"
        );
        let directory = credential_root.join("auth");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .unwrap();
        private_json(&directory.join("xai-oauth.json"), &state);
        Some(
            tickloop_harness_model::LocalModelClients::new(Some(credential_root.clone()))
                .client(&llm_config)
                .unwrap(),
        )
    } else {
        None
    };
    let mut records = Vec::new();

    for log_path in &request.operations {
        let log: Value = serde_json::from_slice(&std::fs::read(log_path).unwrap()).unwrap();
        assert_eq!(json!(revision), log["review"]["auto_settings_revision"]);
        let call_id = log["call_id"].as_str().unwrap();
        let cutoff = lines
            .iter()
            .position(|line| {
                let Ok(line) = serde_json::from_str::<tickloop_harness_transcript::Line>(line)
                else {
                    return false;
                };
                line.chain_entry().is_some_and(|entry| {
                    entry.message.content.iter().any(
                        |content| matches!(content, Content::ToolUse { id, .. } if id == call_id),
                    )
                })
            })
            .unwrap();
        let prefix = credentials.path().join("prefix.jsonl");
        std::fs::write(&prefix, lines[..=cutoff].join("\n")).unwrap();
        let loaded = transcript::load_file(&prefix).unwrap().unwrap();
        assert!(loaded.warnings.is_empty());
        let entry = loaded.chain.last().unwrap();
        let original = entry
            .message
            .content
            .iter()
            .find_map(|content| match content {
                Content::ToolUse { id, name, input } if id == call_id => {
                    assert_eq!(name, "ArtifactEdit");
                    Some(input.clone())
                }
                _ => None,
            })
            .unwrap();
        let receipt = Receipt {
            chat_id: entry.chat_id.into(),
            loop_id: entry.loop_id.unwrap().into(),
            run_id: entry.run_id.unwrap().into(),
        };
        let (restored, warnings) = super::restore_chain(&loaded.chain);
        assert!(warnings.is_empty());
        assert_eq!(
            entry
                .model
                .as_ref()
                .and_then(tickloop_harness_transcript::Recorded::known)
                .unwrap()
                .requested_model,
            llm_config.model
        );
        let mut snapshot =
            empty_snapshot(entry.chat_id.into(), entry.cwd.clone(), llm_config.clone());
        let mut bookkeeping = ChatBookkeeping::default();
        restored.fill_conversation(&mut snapshot, &mut bookkeeping);
        assert!(snapshot.message_compactions.is_empty());
        snapshot.policy.mode = ExecutionMode::Auto;
        let prepared = provider
            .prepare_call(
                entry.chat_id.into(),
                "ArtifactEdit",
                &original,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(prepared.input(), &original);
        let mut operation = OperationRecord {
            input_message_id: None,
            parent_call_id: None,
            parent_call_index: None,
            id: serde_json::from_value(log["operation_id"].clone()).unwrap(),
            loop_id: receipt.loop_id,
            run_id: receipt.run_id,
            call_id: call_id.into(),
            name: "ArtifactEdit".into(),
            input: original.clone(),
            task_binding: None,
            file_change: None,
            state: OperationState::Proposed,
            result: None,
        };
        let old_input: Value =
            serde_json::from_str(log["request_details"]["text"].as_str().unwrap()).unwrap();
        assert_eq!(
            old_input["content"]["bytes"],
            original["content"].as_str().unwrap().len()
        );

        for (variant, input) in [
            ("historical_digest", old_input),
            ("complete_content", original.clone()),
        ] {
            if request.llm_fallback && variant == "historical_digest" {
                continue;
            }
            let variant = if request.llm_fallback {
                "complete_content_llm"
            } else {
                variant
            };
            operation.input = input.clone();
            let folder = snapshot
                .folder
                .as_deref()
                .expect("the replayed Chat has a Folder");
            let mut context = approval_context::capture(
                &tickloop_ids::IdContext::new(),
                &snapshot,
                &bookkeeping,
                &operation,
                None,
                None,
                Some(folder),
                auto.policy_for_directory(folder),
            )
            .unwrap();
            let mut facts = prepared.approval_facts().clone();
            facts.normalized_input = input;
            context.prepared_facts = Some(facts);
            if variant == "historical_digest" {
                context.context_version = "trusted-approval-v8";
            }
            let context_json = serde_json::to_value(&context).unwrap();
            private_json(
                &output.join(format!("{call_id}.{variant}.context.json")),
                &context_json,
            );
            let agent = Agent::new_embedded(config(), Arc::new(ScriptedModel::new(Vec::new())));
            let placeholder = agent.create_chat(credentials.path().into()).await.unwrap();
            let cancel = CancellationToken::new();
            {
                let mut chats = agent.inner.chats.lock().await;
                let mut chat = chats.remove(&placeholder).unwrap();
                chat.snapshot = snapshot.clone();
                chat.active_run = Some(receipt.run_id);
                chat.active = Some(cancel.clone());
                chats.insert(receipt.chat_id, chat);
            }
            let start = Instant::now();
            let result = if let Some(llm) = &llm {
                agent
                    .review_with_llm(
                        &receipt,
                        &context,
                        crate::auto_mode::review_budget_bytes(
                            &snapshot.model,
                            crate::context::context_window(&snapshot),
                        ),
                        llm.clone(),
                        cancel,
                    )
                    .await
            } else {
                agent
                    .review_with_jev(
                        &receipt,
                        &context,
                        jev_review::review_budget_bytes(),
                        client.clone(),
                        tokio::time::Instant::now() + auto_approval::JEV_REVIEW_BUDGET,
                        cancel,
                    )
                    .await
            }
            .unwrap();
            let reason = match &result.review {
                AutoReview::Block { reason, .. } => Some(reason.clone()),
                _ => None,
            };
            let record = json!({
                "call_id":call_id, "variant":variant,
                "outcome":match result.review {
                    AutoReview::Allow => "allow",
                    AutoReview::Block { .. } => "block",
                    AutoReview::ContextLimit => "context_limit",
                    _ => "unavailable",
                },
                "attempts":result.attempts, "failure":result.failure,
                "block_reason":reason,
                "requested_reasoning_effort":llm.as_ref().and_then(|client| client.minimum_reasoning_effort()),
                "reviewer":result.reviewer, "elapsed_ms":start.elapsed().as_millis(),
                "original_body_bytes":original["content"].as_str().unwrap().len(),
                "current_input_json_bytes":serde_json::to_vec(&operation.input).unwrap().len(),
                "context_sha256":format!("{:x}", Sha256::digest(serde_json::to_vec(&context).unwrap())),
                "original_user_inputs":context.original_user_inputs,
                "tool_history_calls":context.tool_history.calls.len(),
                "historical_tool_history_calls":log["context_metrics"]["tool_history_calls"],
                "tool_history_complete":context.tool_history.complete,
                "prepared_facts":context.prepared_facts.as_ref().map(|facts| json!({"publication":facts.artifact_publication})),
                "candidate_dispatched":false,
            });
            println!(
                "TRANSCRIPT_REPLAY {}",
                serde_json::to_string(&record).unwrap()
            );
            records.push(record);
            agent.shutdown().await;
        }
    }
    assert_eq!(source_bytes, std::fs::read(&request.transcript).unwrap());
    private_json(
        &output.join(if request.llm_fallback {
            "report-llm.json"
        } else {
            "report.json"
        }),
        &json!({
            "transcript_sha256":source_hash,
            "settings_revision":revision,
            "limitations":["Historical prepared publication and tool declaration were not saved; fresh read-only preparation supplies current metadata", "Historical in-memory operation paths and dependency baselines were not persisted", "Jev and LLM fallback are measured separately through the production review methods; no executor is installed or called"],
            "results":records,
            "source_unchanged":true,
            "candidate_dispatched":false,
        }),
    );
}

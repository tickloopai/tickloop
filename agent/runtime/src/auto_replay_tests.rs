use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tickloop_agent_model::{
    HttpModelClient, ModelClient,
    judgment::{JevClient, JevConfig, JudgmentClient},
};
use tickloop_agent_protocol::{OriginalUserInput, PreparedApprovalFacts};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{
    approval_context::{
        ApprovalOperation, ApprovalSnapshot, DependencyEvidence, TickCreationReceipt,
    },
    auto_mode,
    auto_rules::RuleVerdict,
    jev_review,
};

const CASES: &str = include_str!("../../../.scratch/approval-parity/replay-cases.json");
const DEPENDENCY_CASES: &str =
    include_str!("../../../.scratch/approval-parity/dependency-review-cases.json");
const HISTORY_CASES: &str =
    include_str!("../../../.scratch/approval-parity/history-review-cases.json");

const DEFAULT_POLICY_CASES: &str =
    include_str!("../../../.scratch/approval-parity/default-policy-review-cases.json");

fn context(case: &Value) -> ApprovalSnapshot {
    let policy = tickloop_agent_config::AutoSettings::default().policy();
    let owned = case["id"] == "owned_tick_cleanup";
    let instance = tick_protocol::InstanceRef::hosted(Uuid::from_u128(1));
    let mut input = case["input"].clone();
    if input.get("tick_instance_id").is_some() {
        input["tick_instance_id"] = json!(instance);
    }
    ApprovalSnapshot {
        reply_history: Default::default(),
        agent_messages: Vec::new(),
        tool_history: super::approval_context::ToolHistory {
            calls: case.get("tool_history").map(|history| serde_json::from_value(history.clone()).unwrap()).unwrap_or_default(),
            ..Default::default()
        },
        context_version: "trusted-approval-v6", review_template_version: "auto-review-v15",
        operation: ApprovalOperation { id: Uuid::nil(), call_id: "replay".into(), loop_id: Uuid::nil(), run_id: Uuid::nil(), tool: case["tool"].as_str().unwrap().into(), input: input.clone() },
        original_user_inputs: vec![OriginalUserInput { chat_id: Uuid::nil(), loop_id: Uuid::nil(), run_id: Uuid::nil(), message_id: Uuid::nil(), text: case["user"].as_str().unwrap().into() }],
        working_directory: "/project".into(), additional_directories: Vec::new(), permission_policy_revision: 0, auto_policy: policy,
        prepared_facts: Some(PreparedApprovalFacts { normalized_input: input, canonical_path: (case["tool"] == "Bash").then(|| "/project".into()), in_workspace: Some(true), tick_instance: matches!(case["tool"].as_str(), Some("ModifyTick" | "CallTickTool")).then_some(instance.clone()), closes_tick: case["tool"] == "ModifyTick", external_declaration: (case["tool"] == "CallTickTool").then(|| json!({"tools":[{"name":"place_order","description":"Place a stock order","mutates":true,"params":{"type":"object"}}]})), ..Default::default() }),
        creation_receipt: owned.then_some(TickCreationReceipt { loop_id: Uuid::nil(), operation_id: Uuid::from_u128(2), instance }),
        dependency_files: Some(vec![DependencyEvidence { path: "/project/package.json".into(), unchanged_since_loop_start: true }, DependencyEvidence { path: "/project/package-lock.json".into(), unchanged_since_loop_start: true }]),
    }
}

async fn replay(use_jev: bool) {
    let mut cases: Vec<Value> = serde_json::from_str(CASES).unwrap();
    cases.extend(serde_json::from_str::<Vec<Value>>(DEPENDENCY_CASES).unwrap());
    cases.extend(serde_json::from_str::<Vec<Value>>(HISTORY_CASES).unwrap());
    cases.extend(serde_json::from_str::<Vec<Value>>(DEFAULT_POLICY_CASES).unwrap());
    if let Ok(selected) = std::env::var("AUTO_REPLAY_CASES") {
        let ids = selected.split(',').collect::<Vec<_>>();
        assert!(
            !selected.is_empty()
                && ids
                    .iter()
                    .all(|id| cases.iter().any(|case| case["id"] == *id)),
            "invalid AUTO_REPLAY_CASES"
        );
        cases.retain(|case| ids.iter().any(|id| case["id"] == *id));
    }
    let repetitions = std::env::var("AUTO_REPLAY_REPEATS").map_or(1, |value| {
        value.parse::<usize>().expect("invalid AUTO_REPLAY_REPEATS")
    });
    assert!(
        (1..=10).contains(&repetitions),
        "AUTO_REPLAY_REPEATS must be 1 through 10"
    );
    let config = tickloop_agent_model::cards::ModelCatalog::defaults()
        .get("deepseek-api", "deepseek-flash")
        .unwrap();
    let llm = (!use_jev).then(|| HttpModelClient::new(config.clone()).unwrap());
    let jev = use_jev.then(|| {
        JevClient::bind(
            JevConfig::default(),
            std::env::var_os("AUTO_REPLAY_JEV_ROOT").map(Into::into),
        )
        .unwrap()
    });
    let mut failures = Vec::new();
    for repetition in 0..repetitions {
        for group in cases.chunks(3) {
            let results = futures_util::future::join_all(group.iter().map(|case| async {
                let context = context(case);
                let cancel = CancellationToken::new();
                let start = Instant::now();
                let mut stage_results = Vec::new();
                let mut final_verdict = Err("no classification".to_owned());
                for stage in [tickloop_agent_protocol::ApprovalStage::RiskScreen, tickloop_agent_protocol::ApprovalStage::Authorization] {
                    let stage_started = Instant::now();
                    let mut usage = None;
                    let mut answer = None;
                let verdict = if let Some(jev) = &jev {
                    let request = jev_review::review_request(
                        &context,
                        stage,
                    )
                    .unwrap();
                    match tokio::time::timeout(
                        Duration::from_secs(30),
                        jev.evaluate(&request, cancel.clone()),
                    )
                    .await
                    {
                        Ok(Ok(evaluation)) => {
                            usage = Some(evaluation.usage.clone());
                            answer = Some(json!(evaluation.answers));
                            jev_review::verdict(&evaluation, &context.auto_policy)
                        }
                        Ok(Err(error)) => {
                            usage = error.usage;
                            Err(match &error.error {
                                tickloop_agent_model::ModelError::InvalidResponse(detail) => {
                                    format!("provider InvalidResponse: {detail}")
                                }
                                _ => format!("provider: {:?}", error.error.kind()),
                            })
                        }
                        Err(_) => Err("timeout".into()),
                    }
                } else {
                    let request = auto_mode::classifier_request(
                        &context,
                        stage,
                    );
                    let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
                    match tokio::time::timeout(
                        Duration::from_secs(60),
                        llm.as_ref()
                            .unwrap()
                            .complete(request, sender, cancel.clone()),
                    )
                    .await
                    {
                        Ok(Ok(output)) => {
                            usage = Some(output.usage.clone());
                            auto_mode::parse_verdict(&output, &context.auto_policy)
                                .map_err(str::to_owned)
                        }
                        Ok(Err(error)) => Err(format!("provider: {:?}", error.kind())),
                        Err(_) => Err("timeout".into()),
                    }
                };
                    let needs_authorization = stage == tickloop_agent_protocol::ApprovalStage::RiskScreen && matches!(verdict, Ok(RuleVerdict::Block { .. }));
                    stage_results.push(json!({"stage":stage, "outcome":match &verdict { Ok(RuleVerdict::Allow) => "allow", Ok(RuleVerdict::Block { .. }) => "block", Err(_) => "error" }, "answer":answer, "usage":usage, "elapsed_ms":stage_started.elapsed().as_millis()}));
                    final_verdict = verdict;
                    if !needs_authorization { break; }
                }
                let verdict = final_verdict;
                cancel.cancel();
                let outcome = match &verdict {
                    Ok(RuleVerdict::Allow) => "allow",
                    Ok(RuleVerdict::Block { .. }) => "block",
                    Err(_) => "error",
                };
                let expected = if case["routine"] == true {
                    "allow"
                } else {
                    "block"
                };
                let (block_kind, block_reason) = match &verdict {
                    Ok(RuleVerdict::Block { kind, reason, .. }) => (Some(*kind), Some(reason)),
                    _ => (None, None),
                };
                let kind_mismatch = case.get("expected_block_kind").is_some_and(|expected| Some(expected) != block_kind.map(|kind| json!(kind)).as_ref());
                println!(
                    "AUTO_REPLAY {}",
                    json!({
                        "reviewer":if use_jev {"jev-1.13.0"} else {config.model.as_str()},
                        "template":context.review_template_version, "repetition":repetition,
                        "case":case["id"], "expected":expected, "outcome":outcome,
                        "error":verdict.as_ref().err(), "stages":stage_results, "block_kind":block_kind, "block_reason":block_reason,
                        "elapsed_ms":start.elapsed().as_millis(),
                    })
                );
                (outcome != expected || kind_mismatch)
                    .then(|| json!({"case":case["id"],"outcome":outcome,"expected":expected}))
            }))
            .await;
            failures.extend(results.into_iter().flatten());
        }
    }
    assert!(
        failures.is_empty(),
        "classification mismatches or failures: {failures:?}"
    );
}

#[tokio::test]
#[ignore = "uses configured DeepSeek credentials; reviews synthetic operations without executing them"]
async fn live_auto_review_replay_llm() {
    replay(false).await;
}

#[tokio::test]
#[ignore = "uses Jev credentials; reviews synthetic operations without executing them"]
async fn live_auto_review_replay_jev() {
    replay(true).await;
}

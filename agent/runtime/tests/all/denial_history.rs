use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use tickloop_agent_model::{ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest};
use tickloop_agent_protocol::{
    AgentHostOptions, ApprovalPath, ApprovalResult, AutoBlockKind, BranchRequest, ChatId, Decision,
    DenialCategory, DenialPersistence, EventKind, ExecutionMode, LoopState, ModelIdentity,
    PermissionApprovalSource, PermissionRule, PermissionScope, PromptPolicy, RewindMode,
    ToolDefinition, ToolResult,
};
use tickloop_agent_runtime::{
    Agent,
    host_tools::{HostToolPermission, HostToolProvider},
};
use tickloop_agent_test_support::{ScriptedModel, call, config, next_event, text, transcript_file};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// A run of a hundred denied calls takes about two seconds alone and much longer on a busy machine.
const COMPLETION_TIMEOUT: Duration = Duration::from_secs(30);
const COMPLETION_POLL_INTERVAL: Duration = Duration::from_millis(10);

struct HeldReviewerModel {
    scripted: ScriptedModel,
    requests: AtomicUsize,
    review_started: tokio::sync::Notify,
    release_review: tokio::sync::Notify,
}

#[async_trait]
impl ModelClient for HeldReviewerModel {
    async fn generate_title(
        &self,
        request: tickloop_agent_model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_agent_model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_agent_model::ModelOutput, tickloop_agent_model::ModelError> {
        Ok(tickloop_agent_test_support::title_output(request))
    }

    fn identity(&self) -> ModelIdentity {
        self.scripted.identity()
    }

    async fn complete(
        &self,
        request: ModelRequest,
        events: tokio::sync::mpsc::UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        if self.requests.fetch_add(1, Ordering::SeqCst) == 1 {
            self.review_started.notify_one();
            tokio::select! {
                _ = self.release_review.notified() => {},
                _ = cancel.cancelled() => return Err(ModelError::Cancelled),
            }
        }
        self.scripted.complete(request, events, cancel).await
    }
}

fn write_rule(decision: Decision) -> PermissionRule {
    PermissionRule {
        id: Uuid::new_v4(),
        decision,
        tool: "Write".into(),
        specifier: None,
        input: None,
        loop_id: None,
        persistent: false,
    }
}

#[derive(Default)]
struct PanelHost {
    calls: AtomicUsize,
    fails: bool,
}

#[async_trait]
impl HostToolProvider for PanelHost {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "change_panel".into(),
            description: "Change a Panel.".into(),
            input_schema: json!({"type":"object","properties":{},"additionalProperties":false}),
        }]
    }

    fn permission(&self, _: ChatId, _: &str, _: &serde_json::Value) -> HostToolPermission {
        HostToolPermission::Automatic
    }

    async fn execute(
        &self,
        _: ChatId,
        _: &str,
        _: serde_json::Value,
        _: CancellationToken,
    ) -> ToolResult {
        self.calls.fetch_add(1, Ordering::SeqCst);
        ToolResult {
            content: if self.fails {
                "execution failed"
            } else {
                "changed"
            }
            .into(),
            is_error: self.fails,
            output_ref: None,
        }
    }
}

async fn wait_for_completion(agent: &Agent, chat: Uuid) {
    tokio::time::timeout(COMPLETION_TIMEOUT, async {
        loop {
            if agent
                .snapshot(chat)
                .await
                .unwrap()
                .loops
                .last()
                .is_some_and(|loop_| loop_.state == LoopState::Completed)
            {
                return;
            }
            // Snapshots clone the whole Chat, so polling without a pause starves the run under load.
            tokio::time::sleep(COMPLETION_POLL_INTERVAL).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn final_rule_denial_is_recorded_once_with_private_display_facts() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("state");
    let workdir = temp.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "denied",
            "Write",
            json!({"file_path":"report.txt","content":"top-secret-123"}),
        ),
        text("I could not write it"),
    ]));
    let agent = Agent::open_embedded(root.clone(), config(), model).unwrap();
    let chat = agent.create_chat(workdir.clone()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    let rule = write_rule(Decision::Deny);
    let rule_id = rule.id;
    agent.set_permission_rules(chat, vec![rule]).await.unwrap();
    let attempt = agent
        .submit(chat, None, "prepare the report".into())
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;

    let view = agent.recent_denials(chat).await.unwrap();
    assert_eq!(view.records.len(), 1);
    let history = agent.approval_history_view(chat).await.unwrap();
    assert_eq!(history.records.len(), 1);
    assert_eq!(history.records[0].outcome, ApprovalResult::Deny);
    let record = &view.records[0];
    assert_eq!(record.chat_id, chat);
    assert_eq!(record.loop_id, attempt.loop_id);
    assert_eq!(record.run_id, attempt.run_id);
    assert_eq!(record.call_id, "denied");
    assert_eq!(record.tool, "Write");
    assert_eq!(record.category, DenialCategory::Rule);
    assert!(
        record
            .matching_rules
            .iter()
            .any(|matched| matched.id == rule_id)
    );
    assert!(!record.input_summary.text.contains("top-secret-123"));
    assert!(!workdir.join("report.txt").exists());
    agent.flush_transcripts().await;
    assert_eq!(
        agent.recent_denials(chat).await.unwrap().persistence,
        DenialPersistence::Saved
    );
    agent.shutdown().await;

    let sidecar = std::fs::read_dir(root.join("projects"))
        .unwrap()
        .flat_map(|project| std::fs::read_dir(project.unwrap().path()).unwrap())
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with(".denials.json")
        })
        .unwrap();
    let saved = std::fs::read_to_string(&sidecar).unwrap();
    assert!(!saved.contains("top-secret-123"));
    let stored: serde_json::Value = serde_json::from_str(&saved).unwrap();
    assert_eq!(stored["version"], 2);
    let mut legacy_record = serde_json::to_value(record).unwrap();
    legacy_record
        .as_object_mut()
        .unwrap()
        .remove("request_details");
    std::fs::write(
        &sidecar,
        json!({"version":1,"chat_id":chat,"records":[legacy_record]}).to_string(),
    )
    .unwrap();
    let restored =
        Agent::open_embedded(root, config(), Arc::new(ScriptedModel::new(Vec::new()))).unwrap();
    restored.resume_chat(chat).await.unwrap();
    let view = restored.recent_denials(chat).await.unwrap();
    assert_eq!(view.records.len(), 1);
    assert_eq!(view.records[0].id, record.id);
    assert_eq!(view.records[0].category, DenialCategory::Rule);
    assert!(view.records[0].request_details.is_none());
    assert!(
        restored.approval_history_view(chat).await.unwrap().records[0]
            .request_details
            .is_none()
    );
    restored.shutdown().await;
}

#[tokio::test]
async fn a_retry_starts_a_new_loop_and_obeys_the_current_ask_rule() {
    let temp = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "original",
            "Write",
            json!({"file_path":"held.txt","content":"first"}),
        ),
        text("blocked"),
        call(
            "retry",
            "Write",
            json!({"file_path":"held.txt","content":"second"}),
        ),
        text("done"),
    ]));
    let agent = Agent::new_embedded(config(), model);
    let chat = agent.create_chat(temp.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    agent
        .set_permission_rules(chat, vec![write_rule(Decision::Deny)])
        .await
        .unwrap();
    let original = agent
        .submit(chat, None, "write held.txt".into())
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;
    let record_id = agent.recent_denials(chat).await.unwrap().records[0].id;
    assert_eq!(
        agent.recent_denials(chat).await.unwrap().persistence,
        DenialPersistence::Disabled
    );
    agent
        .set_permission_rules(chat, vec![write_rule(Decision::Ask)])
        .await
        .unwrap();
    let mut events = agent.subscribe();
    let retry = agent.request_denial_retry(chat, record_id).await.unwrap();
    assert_ne!(retry.loop_id, original.loop_id);
    loop {
        let event = next_event(&mut events).await;
        if event.run_id != retry.run_id {
            continue;
        }
        match event.kind {
            EventKind::PermissionRequested { request } => {
                assert_eq!(
                    request.approval_source,
                    PermissionApprovalSource::ExplicitAsk
                );
                assert!(!temp.path().join("held.txt").exists());
                agent
                    .answer_permission(chat, request.id, false)
                    .await
                    .unwrap();
                break;
            }
            EventKind::ToolStarted { .. } => panic!("retry bypassed the current Ask rule"),
            EventKind::Completed { .. } => panic!("retry completed without current approval"),
            EventKind::Failed(error) => panic!("retry failed before approval: {error}"),
            _ => {}
        }
    }
    assert!(!temp.path().join("held.txt").exists());
    agent.shutdown().await;
}

#[tokio::test]
async fn the_latest_hundred_denials_are_physically_bounded_and_restore_in_order() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("state");
    let workdir = temp.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    let mut calls = Vec::new();
    for index in 0..=100 {
        calls.extend(call(
            &format!("call-{index}"),
            "Write",
            json!({"file_path":format!("file-{index}.txt"),"content":"never saved"}),
        ));
    }
    let model = Arc::new(ScriptedModel::new(vec![calls, text("blocked all calls")]));
    let agent = Agent::open_embedded(root.clone(), config(), model).unwrap();
    let chat = agent.create_chat(workdir).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    agent
        .set_permission_rules(chat, vec![write_rule(Decision::Deny)])
        .await
        .unwrap();
    agent
        .submit(chat, None, "try each file".into())
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;
    let view = agent.recent_denials(chat).await.unwrap();
    assert_eq!(view.records.len(), 100);
    assert_eq!(
        agent
            .approval_history_view(chat)
            .await
            .unwrap()
            .records
            .len(),
        100
    );
    assert_eq!(view.records.first().unwrap().call_id, "call-100");
    assert_eq!(view.records.last().unwrap().call_id, "call-1");
    agent.flush_transcripts().await;
    let path = transcript_file(&root, chat).with_extension("denials.json");
    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(saved["records"].as_array().unwrap().len(), 100);
    agent.shutdown().await;
    let restored =
        Agent::open_embedded(root, config(), Arc::new(ScriptedModel::new(Vec::new()))).unwrap();
    restored.resume_chat(chat).await.unwrap();
    let restored_view = restored.recent_denials(chat).await.unwrap();
    assert_eq!(restored_view.records.len(), 100);
    assert_eq!(restored_view.records[0].call_id, "call-100");
    assert_eq!(restored_view.records[99].call_id, "call-1");
    restored.shutdown().await;
}

#[tokio::test]
async fn mixed_allow_and_deny_history_keeps_the_latest_hundred_results() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("state");
    let workdir = temp.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    std::fs::write(workdir.join("safe.txt"), "safe").unwrap();
    let mut calls = Vec::new();
    for index in 0..=100 {
        let (tool, input) = if index % 2 == 0 {
            ("Read", json!({"file_path":"safe.txt"}))
        } else {
            (
                "Write",
                json!({"file_path":"blocked.txt","content":"blocked"}),
            )
        };
        calls.extend(call(&format!("call-{index}"), tool, input));
    }
    let model = Arc::new(ScriptedModel::new(vec![calls, text("done")]));
    let agent = Agent::open_embedded(root.clone(), config(), model).unwrap();
    let chat = agent.create_chat(workdir).await.unwrap();
    agent
        .set_permission_rules(chat, vec![write_rule(Decision::Deny)])
        .await
        .unwrap();
    agent
        .submit(chat, None, "read and write".into())
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;
    let history = agent.approval_history_view(chat).await.unwrap();
    assert_eq!(history.records.len(), 100);
    assert_eq!(history.records[0].call_id, "call-100");
    assert_eq!(history.records[99].call_id, "call-1");
    assert_eq!(
        history
            .records
            .iter()
            .filter(|record| record.outcome == ApprovalResult::Allow)
            .count(),
        50
    );
    assert_eq!(agent.recent_denials(chat).await.unwrap().records.len(), 50);
    agent.flush_transcripts().await;
    agent.shutdown().await;
    let restored =
        Agent::open_embedded(root, config(), Arc::new(ScriptedModel::new(Vec::new()))).unwrap();
    restored.resume_chat(chat).await.unwrap();
    assert_eq!(
        restored
            .approval_history_view(chat)
            .await
            .unwrap()
            .records
            .len(),
        100
    );
    restored.shutdown().await;
}

#[tokio::test]
async fn disabled_persistence_keeps_denials_only_in_memory() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("state");
    let workdir = temp.path().join("work");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&workdir).unwrap();
    std::fs::write(root.join("settings.json"), r#"{"cleanupPeriodDays":0}"#).unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "denied",
            "Write",
            json!({"file_path":"held.txt","content":"held"}),
        ),
        text("blocked"),
    ]));
    let agent = Agent::open_embedded(root.clone(), config(), model).unwrap();
    assert!(!agent.saves_chats());
    let chat = agent.create_chat(workdir).await.unwrap();
    agent
        .set_permission_rules(chat, vec![write_rule(Decision::Deny)])
        .await
        .unwrap();
    agent.submit(chat, None, "write".into()).await.unwrap();
    wait_for_completion(&agent, chat).await;
    let view = agent.recent_denials(chat).await.unwrap();
    assert_eq!(view.records.len(), 1);
    assert_eq!(view.persistence, DenialPersistence::Disabled);
    agent.shutdown().await;
    assert!(!root.join("projects").exists());
}

#[tokio::test]
async fn damaged_history_warns_without_blocking_chat_recovery() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("state");
    let workdir = temp.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "denied",
            "Write",
            json!({"file_path":"held.txt","content":"held"}),
        ),
        text("blocked"),
    ]));
    let agent = Agent::open_embedded(root.clone(), config(), model).unwrap();
    let chat = agent.create_chat(workdir).await.unwrap();
    agent
        .set_permission_rules(chat, vec![write_rule(Decision::Deny)])
        .await
        .unwrap();
    agent.submit(chat, None, "write".into()).await.unwrap();
    wait_for_completion(&agent, chat).await;
    agent.shutdown().await;
    let sidecar = transcript_file(&root, chat).with_extension("denials.json");
    std::fs::write(sidecar, b"{broken").unwrap();
    let restored =
        Agent::open_embedded(root, config(), Arc::new(ScriptedModel::new(Vec::new()))).unwrap();
    restored.resume_chat(chat).await.unwrap();
    let view = restored.recent_denials(chat).await.unwrap();
    assert!(view.records.is_empty());
    assert!(
        view.load_warning
            .as_deref()
            .is_some_and(|warning| warning.contains("damaged"))
    );
    assert!(!restored.snapshot(chat).await.unwrap().messages.is_empty());
    restored.shutdown().await;
}

#[tokio::test]
async fn failed_history_save_keeps_memory_and_retries_without_losing_records() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("state");
    let workdir = temp.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "one",
            "Write",
            json!({"file_path":"one.txt","content":"one"}),
        ),
        text("first blocked"),
        call(
            "two",
            "Write",
            json!({"file_path":"two.txt","content":"two"}),
        ),
        text("second blocked"),
    ]));
    let agent = Agent::open_embedded(root.clone(), config(), model).unwrap();
    let chat = agent.create_chat(workdir).await.unwrap();
    agent
        .set_permission_rules(chat, vec![write_rule(Decision::Deny)])
        .await
        .unwrap();
    agent.submit(chat, None, "first".into()).await.unwrap();
    wait_for_completion(&agent, chat).await;
    agent.flush_transcripts().await;
    let sidecar = transcript_file(&root, chat).with_extension("denials.json");
    std::fs::remove_file(&sidecar).unwrap();
    std::fs::create_dir(&sidecar).unwrap();
    agent.submit(chat, None, "second".into()).await.unwrap();
    wait_for_completion(&agent, chat).await;
    agent.flush_transcripts().await;
    let unsaved = agent.recent_denials(chat).await.unwrap();
    assert_eq!(unsaved.records.len(), 2);
    assert_eq!(unsaved.records[0].call_id, "two");
    assert!(matches!(
        unsaved.persistence,
        DenialPersistence::Unsaved { .. }
    ));
    std::fs::remove_dir(&sidecar).unwrap();
    agent.flush_transcripts().await;
    assert_eq!(
        agent.recent_denials(chat).await.unwrap().persistence,
        DenialPersistence::Saved
    );
    agent.shutdown().await;
    let restored =
        Agent::open_embedded(root, config(), Arc::new(ScriptedModel::new(Vec::new()))).unwrap();
    restored.resume_chat(chat).await.unwrap();
    assert_eq!(
        restored.recent_denials(chat).await.unwrap().records.len(),
        2
    );
    restored.shutdown().await;
}

#[tokio::test]
async fn host_rule_denial_is_recorded_without_executing_the_host_tool() {
    let temp = tempfile::tempdir().unwrap();
    let host = Arc::new(PanelHost::default());
    let model = Arc::new(ScriptedModel::new(vec![
        call("host-denied", "change_panel", json!({})),
        text("blocked"),
    ]));
    let agent = Agent::new_embedded(config(), model);
    agent.install_host_tools(host.clone()).unwrap();
    let chat = agent.create_chat(temp.path().into()).await.unwrap();
    let mut rule = write_rule(Decision::Deny);
    rule.tool = "change_panel".into();
    agent
        .set_permission_rules(chat, vec![rule.clone()])
        .await
        .unwrap();
    agent
        .submit(chat, None, "change panel".into())
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;
    let records = agent.recent_denials(chat).await.unwrap().records;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].call_id, "host-denied");
    assert_eq!(records[0].category, DenialCategory::Rule);
    assert!(
        records[0]
            .matching_rules
            .iter()
            .any(|matched| matched.id == rule.id)
    );
    assert_eq!(host.calls.load(Ordering::SeqCst), 0);
    agent.shutdown().await;
}

#[tokio::test]
async fn direct_read_and_host_admission_each_have_one_allow_record() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("state");
    let workdir = temp.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    std::fs::write(workdir.join("input.txt"), "safe").unwrap();
    let host = Arc::new(PanelHost::default());
    let model = Arc::new(ScriptedModel::new(vec![
        call("read", "Read", json!({"file_path":"input.txt"})),
        text("read"),
        call("host", "change_panel", json!({})),
        text("changed"),
    ]));
    let agent = Agent::open_embedded(root.clone(), config(), model).unwrap();
    agent.install_host_tools(host.clone()).unwrap();
    let chat = agent.create_chat(workdir).await.unwrap();
    agent
        .submit(chat, None, "read input.txt".into())
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;
    agent
        .submit(chat, None, "change panel".into())
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;
    let history = agent.approval_history_view(chat).await.unwrap();
    assert_eq!(history.records.len(), 2);
    assert_eq!(history.records[0].call_id, "host");
    assert_eq!(history.records[1].call_id, "read");
    assert!(history.records[0].reason.contains("Host tool"));
    assert!(history.records[1].reason.contains("Read-only"));
    assert!(history.records.iter().all(|record| {
        record.outcome == ApprovalResult::Allow && record.path == ApprovalPath::Direct
    }));
    assert!(agent.recent_denials(chat).await.unwrap().records.is_empty());
    assert_eq!(host.calls.load(Ordering::SeqCst), 1);
    assert!(
        agent
            .request_denial_retry(chat, history.records[0].id)
            .await
            .is_err()
    );
    agent.flush_transcripts().await;
    agent.shutdown().await;
    let restored =
        Agent::open_embedded(root, config(), Arc::new(ScriptedModel::new(Vec::new()))).unwrap();
    restored.resume_chat(chat).await.unwrap();
    let restored_history = restored.approval_history_view(chat).await.unwrap();
    assert_eq!(restored_history.records.len(), 2);
    assert_eq!(restored_history.records[0].id, history.records[0].id);
    assert!(
        restored_history.records[1]
            .request_details
            .as_ref()
            .unwrap()
            .text
            .contains("input.txt")
    );
    restored.shutdown().await;
}

#[tokio::test]
async fn a_tool_failure_after_admission_keeps_its_allow_result() {
    let temp = tempfile::tempdir().unwrap();
    let host = Arc::new(PanelHost {
        fails: true,
        ..Default::default()
    });
    let model = Arc::new(ScriptedModel::new(vec![
        call("failing", "change_panel", json!({})),
        text("failed"),
    ]));
    let agent = Agent::new_embedded(config(), model);
    agent.install_host_tools(host.clone()).unwrap();
    let chat = agent.create_chat(temp.path().into()).await.unwrap();
    agent
        .submit(chat, None, "change panel".into())
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;
    let history = agent.approval_history_view(chat).await.unwrap();
    assert_eq!(history.records.len(), 1);
    assert_eq!(history.records[0].outcome, ApprovalResult::Allow);
    assert_eq!(history.records[0].path, ApprovalPath::Direct);
    assert!(agent.recent_denials(chat).await.unwrap().records.is_empty());
    assert_eq!(host.calls.load(Ordering::SeqCst), 1);
    agent.shutdown().await;
}

#[tokio::test]
async fn human_refusal_and_approval_have_distinct_final_results() {
    for allow in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let model = Arc::new(ScriptedModel::new(vec![
            call(
                "ask",
                "Write",
                json!({"file_path":"human.txt","content":"created"}),
            ),
            text("done"),
        ]));
        let agent = Agent::new_embedded(config(), model);
        let chat = agent.create_chat(temp.path().into()).await.unwrap();
        agent
            .set_permission_rules(chat, vec![write_rule(Decision::Ask)])
            .await
            .unwrap();
        let mut events = agent.subscribe();
        let attempt = agent
            .submit(chat, None, "write human.txt".into())
            .await
            .unwrap();
        let request = loop {
            let event = next_event(&mut events).await;
            if event.run_id != attempt.run_id {
                continue;
            }
            match event.kind {
                EventKind::PermissionRequested { request } => break request,
                EventKind::ToolStarted { .. } => panic!("tool ran before human answer"),
                EventKind::Completed { .. } => panic!("run completed before human answer"),
                _ => {}
            }
        };
        agent
            .answer_permission_with_comment(
                chat,
                request.id,
                allow,
                PermissionScope::Once,
                (!allow).then(|| "private account token 12345".into()),
            )
            .await
            .unwrap();
        loop {
            let event = next_event(&mut events).await;
            if event.run_id != attempt.run_id {
                continue;
            }
            if matches!(
                event.kind,
                EventKind::Completed { .. } | EventKind::Interrupted
            ) {
                break;
            }
        }
        let records = agent.recent_denials(chat).await.unwrap().records;
        assert_eq!(records.len(), usize::from(!allow));
        let history = agent.approval_history_view(chat).await.unwrap();
        assert_eq!(history.records.len(), 1);
        assert_eq!(history.records[0].path, ApprovalPath::Human);
        assert_eq!(
            history.records[0].outcome,
            if allow {
                ApprovalResult::Allow
            } else {
                ApprovalResult::Deny
            }
        );
        if allow {
            assert!(history.records[0].reason.contains("Human approved"));
            let details = &history.records[0].request_details.as_ref().unwrap().text;
            assert!(details.contains("human.txt"));
            assert!(details.contains("created"));
        }
        if !allow {
            assert_eq!(records[0].category, DenialCategory::Human);
            assert!(!records[0].reason.contains("12345"));
            assert_eq!(
                records[0].approval_source,
                Some(PermissionApprovalSource::ExplicitAsk)
            );
        }
        assert_eq!(temp.path().join("human.txt").exists(), allow);
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn headless_explicit_ask_is_a_final_no_human_denial() {
    let temp = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "ask",
            "Write",
            json!({"file_path":"headless.txt","content":"no"}),
        ),
        text("blocked"),
    ]));
    let agent = Agent::new_with_options(
        config(),
        model,
        AgentHostOptions {
            prompt_policy: PromptPolicy::DenyOnPrompt,
            ..AgentHostOptions::embedded_defaults()
        },
    );
    let chat = agent.create_chat(temp.path().into()).await.unwrap();
    agent
        .set_permission_rules(chat, vec![write_rule(Decision::Ask)])
        .await
        .unwrap();
    agent.submit(chat, None, "write".into()).await.unwrap();
    wait_for_completion(&agent, chat).await;
    let records = agent.recent_denials(chat).await.unwrap().records;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].category, DenialCategory::NoHuman);
    assert_eq!(
        records[0].approval_source,
        Some(PermissionApprovalSource::ExplicitAsk)
    );
    assert!(!temp.path().join("headless.txt").exists());
    agent.shutdown().await;
}

#[tokio::test]
async fn cancelling_a_held_permission_request_does_not_create_a_denial() {
    let temp = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![call(
        "held",
        "Write",
        json!({"file_path":"cancelled.txt","content":"no"}),
    )]));
    let agent = Agent::new_embedded(config(), model);
    let chat = agent.create_chat(temp.path().into()).await.unwrap();
    agent
        .set_permission_rules(chat, vec![write_rule(Decision::Ask)])
        .await
        .unwrap();
    let mut events = agent.subscribe();
    let attempt = agent.submit(chat, None, "write".into()).await.unwrap();
    loop {
        let event = next_event(&mut events).await;
        if event.run_id != attempt.run_id {
            continue;
        }
        if matches!(event.kind, EventKind::PermissionRequested { .. }) {
            break;
        }
    }
    agent.stop(chat, attempt.loop_id).await.unwrap();
    assert!(agent.recent_denials(chat).await.unwrap().records.is_empty());
    assert!(
        agent
            .approval_history_view(chat)
            .await
            .unwrap()
            .records
            .is_empty()
    );
    assert!(!temp.path().join("cancelled.txt").exists());
    agent.shutdown().await;
}

#[tokio::test]
async fn branch_starts_without_source_denials_while_source_rewind_retains_history() {
    let temp = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "denied",
            "Write",
            json!({"file_path":"past.txt","content":"held"}),
        ),
        text("blocked"),
    ]));
    let agent = Agent::new_embedded(config(), model);
    let chat = agent.create_chat(temp.path().into()).await.unwrap();
    agent
        .set_permission_rules(chat, vec![write_rule(Decision::Deny)])
        .await
        .unwrap();
    let receipt = agent
        .submit(chat, None, "write past.txt".into())
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;
    let record = agent.recent_denials(chat).await.unwrap().records[0].clone();
    let child = agent
        .branch_chat(BranchRequest {
            request_id: Uuid::new_v4(),
            source_chat_id: chat,
            source_loop_id: Some(receipt.loop_id),
            name: Some("alternate".into()),
        })
        .await
        .unwrap();
    assert!(
        agent
            .recent_denials(child.child_chat_id)
            .await
            .unwrap()
            .records
            .is_empty()
    );
    let point = agent.snapshot(chat).await.unwrap().rewind_points()[0].message_id;
    agent
        .rewind(chat, point, RewindMode::Conversation)
        .await
        .unwrap();
    let retained = agent.recent_denials(chat).await.unwrap().records;
    assert_eq!(retained.len(), 1);
    assert_eq!(retained[0].id, record.id);
    assert!(!temp.path().join("past.txt").exists());
    agent.shutdown().await;
}

#[tokio::test]
async fn approval_invalidated_by_a_new_deny_is_a_final_admission_refusal() {
    let temp = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "held",
            "Write",
            json!({"file_path":"revoked.txt","content":"no"}),
        ),
        text("blocked"),
    ]));
    let agent = Agent::new_embedded(config(), model);
    let chat = agent.create_chat(temp.path().into()).await.unwrap();
    agent
        .set_permission_rules(chat, vec![write_rule(Decision::Ask)])
        .await
        .unwrap();
    let mut events = agent.subscribe();
    let receipt = agent
        .submit(chat, None, "write revoked.txt".into())
        .await
        .unwrap();
    let request = loop {
        let event = next_event(&mut events).await;
        if event.run_id == receipt.run_id
            && let EventKind::PermissionRequested { request } = event.kind
        {
            break request;
        }
    };
    agent
        .set_permission_rules(chat, vec![write_rule(Decision::Deny)])
        .await
        .unwrap();
    agent
        .answer_permission(chat, request.id, true)
        .await
        .unwrap();
    loop {
        let event = next_event(&mut events).await;
        if event.run_id == receipt.run_id
            && matches!(
                event.kind,
                EventKind::Completed { .. } | EventKind::Failed(_)
            )
        {
            break;
        }
    }
    assert!(!temp.path().join("revoked.txt").exists());
    let records = agent.recent_denials(chat).await.unwrap().records;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].category, DenialCategory::FinalAdmission);
    assert_eq!(records[0].call_id, "held");
    let history = agent.approval_history_view(chat).await.unwrap();
    assert_eq!(history.records.len(), 1);
    assert_eq!(history.records[0].outcome, ApprovalResult::Deny);
    agent.shutdown().await;
}

#[tokio::test]
async fn automatic_hard_deny_keeps_the_frozen_rule_and_reviewer_path_after_policy_edit() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("state");
    let workdir = temp.path().join("work");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&workdir).unwrap();
    let hard_id = Uuid::new_v4().to_string();
    let original_rule = "Never create the protected symlink";
    let settings = json!({"autoApproval":{"reviewer":"llm","rules":{
        "softDeny":{"includeBuiltins":false,"custom":[]},
        "hardDeny":{"includeBuiltins":true,"custom":[{"id":hard_id,"text":original_rule}]}
    }}});
    std::fs::write(root.join("settings.json"), settings.to_string()).unwrap();
    let finding = json!({"decision":"block_hard","rule_id":hard_id,"reason":"Never create the protected symlink"});
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "blocked",
            "Bash",
            json!({"command":"ln -s missing-target protected.txt"}),
        ),
        text(&finding.to_string()),
        text(&finding.to_string()),
        text("blocked"),
    ]));
    let agent = Agent::open_embedded(root.clone(), config(), model).unwrap();
    let chat = agent.create_chat(workdir.clone()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::Auto)
        .await
        .unwrap();
    agent
        .submit(chat, None, "create the protected symlink".into())
        .await
        .unwrap();
    wait_for_completion(&agent, chat).await;
    let record = agent.recent_denials(chat).await.unwrap().records.remove(0);
    assert_eq!(record.category, DenialCategory::Automatic);
    assert_eq!(
        record.approval_source,
        Some(PermissionApprovalSource::AutoReviewer)
    );
    let review = record.review.unwrap();
    assert_eq!(review.block_kind, Some(AutoBlockKind::HardDeny));
    let rule = review.block_rule.unwrap();
    assert_eq!(rule.id, hard_id);
    assert_eq!(rule.text.text, original_rule);
    assert!(review.block_finding.is_none());
    assert_eq!(review.attempts.len(), 2);
    assert!(workdir.join("protected.txt").symlink_metadata().is_err());
    agent.shutdown().await;
    std::fs::write(
        root.join("settings.json"),
        r#"{"autoApproval":{"reviewer":"llm"}}"#,
    )
    .unwrap();
    let resumed =
        Agent::open_embedded(root, config(), Arc::new(ScriptedModel::new(Vec::new()))).unwrap();
    resumed.resume_chat(chat).await.unwrap();
    let restored = resumed
        .recent_denials(chat)
        .await
        .unwrap()
        .records
        .remove(0);
    assert_eq!(
        restored.review.unwrap().block_rule.unwrap().text.text,
        original_rule
    );
    resumed.shutdown().await;
}

#[tokio::test]
async fn stale_auto_allow_is_reviewed_again_against_the_new_hard_deny() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("state");
    std::fs::create_dir_all(&root).unwrap();
    let settings_path = root.join("settings.json");
    let initial = json!({"autoApproval":{"reviewer":"llm","rules":{
        "softDeny":{"includeBuiltins":false,"custom":[]}
    }}});
    std::fs::write(&settings_path, initial.to_string()).unwrap();
    let hard_id = Uuid::new_v4().to_string();
    let blocked = json!({"decision":"block_hard","rule_id":hard_id,"reason":"Never create the protected symlink"});
    let model = Arc::new(HeldReviewerModel {
        scripted: ScriptedModel::new(vec![
            call(
                "stale-allow",
                "Bash",
                json!({"command":"ln -s missing-target held-auto"}),
            ),
            text(&tickloop_agent_test_support::auto_classification(false).to_string()),
            text(&blocked.to_string()),
            text(&blocked.to_string()),
            text("blocked"),
        ]),
        requests: AtomicUsize::new(0),
        review_started: tokio::sync::Notify::new(),
        release_review: tokio::sync::Notify::new(),
    });
    let agent = Agent::open_embedded(root, config(), model.clone()).unwrap();
    let chat = agent.create_chat(temp.path().into()).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::Auto)
        .await
        .unwrap();
    let receipt = agent
        .submit(chat, None, "create the held symlink".into())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), model.review_started.notified())
        .await
        .unwrap();
    let changed = json!({"autoApproval":{"reviewer":"llm","rules":{
        "softDeny":{"includeBuiltins":false,"custom":[]},
        "hardDeny":{"includeBuiltins":true,"custom":[{"id":hard_id,"text":"Never create this symlink"}]}
    }}});
    std::fs::write(&settings_path, changed.to_string()).unwrap();
    model.release_review.notify_one();
    wait_for_completion(&agent, chat).await;
    assert!(temp.path().join("held-auto").symlink_metadata().is_err());
    let records = agent.recent_denials(chat).await.unwrap().records;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].run_id, receipt.run_id);
    assert_eq!(records[0].category, DenialCategory::Automatic);
    assert_eq!(
        records[0].review.as_ref().unwrap().block_kind,
        Some(AutoBlockKind::HardDeny)
    );
    assert_eq!(
        records[0]
            .review
            .as_ref()
            .unwrap()
            .block_rule
            .as_ref()
            .unwrap()
            .id,
        hard_id
    );
    assert_eq!(model.requests.load(Ordering::SeqCst), 5);
    agent.shutdown().await;
}

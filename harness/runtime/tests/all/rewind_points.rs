use crate::support;
use tickloop_code_local_environment::AgentLocalExt;
use tickloop_code_settings::local_agent::LocalAgent;

use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use serde_json::json;
use tickloop_harness_model::{ModelClient, ModelDelta, ModelError, ModelOutput, ModelRequest};
use tickloop_harness_protocol::{
    ChatId, Content, Event, EventKind, ExecutionMode, InputContext, ModelIdentity, PermissionScope,
    RewindPoint, Role, RunId, TextContext, UserShellState,
};
use tickloop_harness_runtime::Agent;
use tickloop_harness_test_support::shell_ticks::ShellTick;
use tickloop_harness_test_support::{ScriptedModel, call, config, text};
use tokio::sync::{Notify, broadcast, mpsc::UnboundedSender};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const TEST_DEADLINE: Duration = Duration::from_secs(5);

struct GatedFirstRequest {
    model: Arc<ScriptedModel>,
    entered: Notify,
    release: Notify,
    calls: AtomicUsize,
}

impl GatedFirstRequest {
    fn new(model: Arc<ScriptedModel>) -> Self {
        Self {
            model,
            entered: Notify::new(),
            release: Notify::new(),
            calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl ModelClient for GatedFirstRequest {
    async fn generate_title(
        &self,
        request: tickloop_harness_model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_harness_model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_harness_model::ModelOutput, tickloop_harness_model::ModelError> {
        Ok(tickloop_harness_test_support::title_output(request))
    }

    fn identity(&self) -> ModelIdentity {
        self.model.identity()
    }

    async fn complete(
        &self,
        request: ModelRequest,
        events: UnboundedSender<ModelDelta>,
        cancel: CancellationToken,
    ) -> Result<ModelOutput, ModelError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            self.entered.notify_one();
            tokio::select! {
                _ = cancel.cancelled() => return Err(ModelError::Cancelled),
                _ = self.release.notified() => {}
            }
        }
        self.model.complete(request, events, cancel).await
    }
}

async fn completion(events: &mut broadcast::Receiver<Event>, run: Option<RunId>) -> Event {
    tokio::time::timeout(TEST_DEADLINE, async {
        loop {
            let event = events.recv().await.unwrap();
            if run.is_some_and(|run| event.run_id != run) {
                continue;
            }
            match &event.kind {
                EventKind::Completed { .. } => return event,
                EventKind::Failed(error) => panic!("checkpoint fixture failed: {error}"),
                EventKind::Interrupted => panic!("checkpoint fixture was interrupted"),
                _ => {}
            }
        }
    })
    .await
    .expect("checkpoint fixture did not complete before its deadline")
}

async fn checkpoints(agent: &Agent, chat: ChatId) -> Vec<RewindPoint> {
    agent.snapshot(chat).await.unwrap().rewind_points()
}

#[tokio::test]
async fn first_and_followup_prompts_capture_exact_inputs_attachments_and_pre_input_boundaries() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("中文-notes.txt");
    let attachment = "第一行\r\nsecond line without final newline";
    std::fs::write(&path, attachment).unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call("read", "Read", json!({"file_path":"中文-notes.txt"})),
        text("Read the reference"),
        text("Followup received"),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let first_prompt = "  保留原文\n阅读 @中文-notes.txt\t  ";
    let first = agent
        .submit_with_file_references(chat, first_prompt.into(), vec![path.clone()])
        .await
        .unwrap();
    completion(&mut events, Some(first.run_id)).await;
    let before = agent.snapshot(chat).await.unwrap();
    assert_eq!(before.messages.len(), 4);
    assert_eq!(before.operations.len(), 1);
    let followup_prompt = "继续\r\nexact followup";
    let followup = agent
        .submit_with_file_references(chat, followup_prompt.into(), vec![path])
        .await
        .unwrap();
    completion(&mut events, Some(followup.run_id)).await;
    let snapshot = agent.snapshot(chat).await.unwrap();
    let entries = snapshot.rewind_points();
    assert_eq!(entries.len(), 2);
    for (entry, receipt, prompt, boundary) in [
        (&entries[0], &first, first_prompt, 0),
        (&entries[1], &followup, followup_prompt, 4),
    ] {
        assert_eq!(snapshot.message_runs[&boundary], receipt.run_id);
        assert_eq!(entry.prompt, prompt);
        assert_eq!(entry.message_index, boundary);
        assert_eq!(entry.message_id, snapshot.message_ids[&boundary]);
        assert_eq!(entry.execution_mode, ExecutionMode::AcceptEdits);
        assert_eq!(entry.contexts.len(), 1);
        assert_eq!(
            entry.contexts[0],
            InputContext::File(TextContext {
                path: Path::new("中文-notes.txt").into(),
                content: attachment.into(),
                truncated: false,
            })
        );
        assert_eq!(entry.contexts, snapshot.message_contexts[&boundary]);
        assert!(
            matches!(&snapshot.messages[boundary].content[..], [Content::Text { text }] if text == prompt)
        );
    }
    assert_ne!(entries[0].message_id, entries[1].message_id);
    assert_eq!(model.requests.lock().unwrap().len(), 3);
    agent.shutdown().await;
}

#[tokio::test]
async fn two_queued_inputs_merged_after_a_tool_keep_individual_receipts_and_one_execution_run() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("source.txt");
    std::fs::write(&path, "source material").unwrap();
    let scripted = Arc::new(ScriptedModel::new(vec![
        call("read", "Read", json!({"file_path":"source.txt"})),
        text("Both real corrections applied"),
    ]));
    let model = Arc::new(GatedFirstRequest::new(scripted.clone()));
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let first = agent.submit(chat, "Read the source".into()).await.unwrap();
    tokio::time::timeout(TEST_DEADLINE, model.entered.notified())
        .await
        .unwrap();
    let second = agent
        .submit_with_file_references(chat, "修正一\n带附件".into(), vec![path])
        .await
        .unwrap();
    let third = agent.submit(chat, "修正二  保留空格".into()).await.unwrap();
    let waiting = agent.snapshot(chat).await.unwrap();
    assert_eq!(waiting.rewind_points().len(), 1);
    assert_eq!(waiting.pending_inputs.len(), 2);
    model.release.notify_one();
    completion(&mut events, Some(first.run_id)).await;
    let snapshot = agent.snapshot(chat).await.unwrap();
    let entries = snapshot.rewind_points();
    assert_eq!(entries.len(), 3);
    assert_eq!(snapshot.messages.len(), 6);
    assert_eq!(snapshot.operations.len(), 1);
    assert!(snapshot.pending_inputs.is_empty());
    for (entry, receipt, boundary, prompt) in [
        (&entries[1], &second, 3, "修正一\n带附件"),
        (&entries[2], &third, 4, "修正二  保留空格"),
    ] {
        assert_eq!(receipt.loop_id, first.loop_id);
        assert_ne!(receipt.run_id, first.run_id);
        assert_eq!(entry.message_index, boundary);
        assert_eq!(entry.prompt, prompt);
        assert_eq!(snapshot.message_runs[&boundary], receipt.run_id);
        assert!(snapshot.events.iter().any(|event| event.run_id == receipt.run_id && matches!(event.kind, EventKind::InputMerged { into_run_id } if into_run_id == first.run_id)));
    }
    assert!(matches!(
        &entries[1].contexts[0],
        InputContext::File(file) if file.content == "source material"
    ));
    assert!(entries[2].contexts.is_empty());
    assert_eq!(scripted.requests.lock().unwrap().len(), 2);
    assert_eq!(support::model_script_requests(&snapshot).len(), 2);
    agent.shutdown().await;
}

#[tokio::test]
async fn accepting_and_taking_back_waiting_input_or_commands_does_not_capture_phantom_points() {
    let directory = tempfile::tempdir().unwrap();
    let scripted = Arc::new(ScriptedModel::new(vec![text("Only the first input ran")]));
    let model = Arc::new(GatedFirstRequest::new(scripted.clone()));
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let first = agent
        .submit(chat, "Hold the current request".into())
        .await
        .unwrap();
    tokio::time::timeout(TEST_DEADLINE, model.entered.notified())
        .await
        .unwrap();
    let before = checkpoints(&agent, chat).await;
    let queued = agent.submit(chat, "Take this back".into()).await.unwrap();
    let command = agent.queue_command(chat, "/compact".into()).await.unwrap();
    assert_eq!(checkpoints(&agent, chat).await, before);
    let taken = agent.take_queued_inputs(chat, true).await.unwrap();
    assert_eq!(
        taken.iter().map(|item| item.id).collect::<Vec<_>>(),
        vec![queued.run_id, tickloop_ids::Id::from(command)]
    );
    assert_eq!(checkpoints(&agent, chat).await, before);
    assert!(
        agent
            .snapshot(chat)
            .await
            .unwrap()
            .pending_inputs
            .is_empty()
    );
    model.release.notify_one();
    completion(&mut events, Some(first.run_id)).await;
    assert_eq!(checkpoints(&agent, chat).await, before);
    assert_eq!(scripted.requests.lock().unwrap().len(), 1);
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(snapshot.messages.len(), 2);
    assert_eq!(support::model_script_requests(&snapshot).len(), 1);
    agent.shutdown().await;
}

#[tokio::test]
async fn permission_allow_and_deny_comments_reach_history_without_becoming_prompts() {
    for allow in [true, false] {
        let directory = tempfile::tempdir().unwrap();
        let model = Arc::new(ScriptedModel::new(vec![
            call(
                "commented-shell",
                "Bash",
                json!({"command":"env sh -c 'printf approved > marker.txt'"}),
            ),
            text("The permission comment was received"),
        ]));
        let agent = Agent::new_embedded(config(), model.clone());
        agent.install_own_tickhost();
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        let mut events = agent.subscribe();
        let receipt = agent
            .submit(chat, "Propose a bounded shell action".into())
            .await
            .unwrap();
        let permission = tokio::time::timeout(TEST_DEADLINE, async {
            loop {
                match events.recv().await.unwrap().kind {
                    EventKind::PermissionRequested { request } => break request,
                    EventKind::Failed(error) => panic!("expected a permission request: {error}"),
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
        let before = checkpoints(&agent, chat).await;
        let comment = if allow {
            "允许此次操作；保留这条备注"
        } else {
            "拒绝此操作；请解释替代方案"
        };
        agent
            .answer_permission_with_comment(
                chat,
                permission.id,
                allow,
                PermissionScope::Once,
                Some(comment.into()),
            )
            .await
            .unwrap();
        completion(&mut events, Some(receipt.run_id)).await;
        let snapshot = agent.snapshot(chat).await.unwrap();
        assert_eq!(checkpoints(&agent, chat).await, before);
        assert_eq!(snapshot.rewind_points().len(), 1);
        assert!(snapshot.messages.iter().filter(|message| message.role == Role::User).flat_map(|message| &message.content).any(|content| matches!(content, Content::ToolResult { content, .. } if content.text().contains(comment))));
        assert!(
            !snapshot
                .rewind_points()
                .iter()
                .any(|entry| entry.prompt.contains(comment))
        );
        assert_eq!(directory.path().join("marker.txt").exists(), allow);
        assert_eq!(model.requests.lock().unwrap().len(), 2);
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn compaction_commands_and_direct_shell_do_not_capture_points_but_later_prompt_keeps_context()
{
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        text("First reply"),
        text("Public checkpoint summary"),
        text("Followup after host actions"),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    agent.install_own_tickhost();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let first = agent
        .submit(chat, "Initial user prompt".into())
        .await
        .unwrap();
    completion(&mut events, Some(first.run_id)).await;
    let original_points = checkpoints(&agent, chat).await;
    agent.compact(chat).await.unwrap();
    assert_eq!(checkpoints(&agent, chat).await, original_points);
    let command = agent.queue_command(chat, "/rules".into()).await.unwrap();
    let dispatched = agent.take_next_queued_command(chat).await.unwrap().unwrap();
    assert_eq!(dispatched.id, command);
    agent.finish_queued_command(chat, command).await.unwrap();
    assert_eq!(checkpoints(&agent, chat).await, original_points);
    let mut shell_events = agent.subscribe_user_shells();
    let shell = agent
        .submit_user_shell(chat, "printf direct-shell".into(), false)
        .await
        .unwrap();
    tokio::time::timeout(TEST_DEADLINE, async {
        loop {
            let record = shell_events.recv().await.unwrap();
            if record.id == shell.command_id
                && matches!(
                    record.state,
                    UserShellState::Completed | UserShellState::Failed
                )
            {
                assert_eq!(record.state, UserShellState::Completed);
                assert_eq!(record.exit_code, Some(0));
                break;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(checkpoints(&agent, chat).await, original_points);
    let before = agent.snapshot(chat).await.unwrap();
    assert_eq!(model.requests.lock().unwrap().len(), 2);
    let followup = agent
        .submit(chat, "Continue with the compacted context".into())
        .await
        .unwrap();
    completion(&mut events, Some(followup.run_id)).await;
    let snapshot = agent.snapshot(chat).await.unwrap();
    let points = snapshot.rewind_points();
    assert_eq!(points.len(), 2);
    assert_eq!(points[1].message_index, before.messages.len());
    assert_eq!(points[1].prompt, "Continue with the compacted context");
    assert_eq!(
        snapshot.message_runs[&points[1].message_index],
        followup.run_id
    );
    assert_eq!(
        snapshot.latest_compact_summary().as_deref(),
        Some("Public checkpoint summary")
    );
    assert_eq!(snapshot.user_shells.len(), 1);
    assert_eq!(model.requests.lock().unwrap().len(), 3);
    agent.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn trigger_deliveries_preserve_the_single_real_user_checkpoint() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        text("Initial observation"),
        text("Triggered observation"),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    let shell = ShellTick::new(Uuid::new_v4());
    shell.install(&agent).await;
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let first = agent
        .submit(chat, "Observe when triggered".into())
        .await
        .unwrap();
    completion(&mut events, Some(first.run_id)).await;
    let before = checkpoints(&agent, chat).await;
    shell.subscribe(&agent, chat).await;
    shell.update(1, "External observation");
    let completed = completion(&mut events, None).await;
    assert_ne!(completed.run_id, first.run_id);
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(checkpoints(&agent, chat).await, before);
    assert_eq!(snapshot.rewind_points().len(), 1);
    assert_eq!(snapshot.messages.len(), 4);
    assert_eq!(support::model_script_requests(&snapshot).len(), 2);
    assert_eq!(
        support::model_script_requests(&snapshot)[1].run_id,
        Some(completed.run_id)
    );
    assert!(snapshot.messages[2].content.iter().any(
        |content| matches!(content, Content::Text { text } if text.contains("External observation"))
    ));
    assert_eq!(model.requests.lock().unwrap().len(), 2);
    agent.shutdown().await;
}

#[tokio::test]
async fn restart_preserves_checkpoint_ids() {
    let directory = tempfile::tempdir().unwrap();
    let archive = directory.path().join("archive");
    let model = Arc::new(ScriptedModel::new(vec![
        text("First reply"),
        text("Second reply"),
        text("New point after legacy recovery"),
    ]));
    let agent = Agent::open_embedded(archive.clone(), config(), model.clone()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let first = agent.submit(chat, "First prompt".into()).await.unwrap();
    completion(&mut events, Some(first.run_id)).await;
    let second = agent.submit(chat, "Second prompt".into()).await.unwrap();
    completion(&mut events, Some(second.run_id)).await;
    let before = agent.snapshot(chat).await.unwrap();
    let original_points = checkpoints(&agent, chat).await;
    agent.shutdown().await;
    drop(agent);
    let reopened = Agent::open_embedded(archive.clone(), config(), model.clone()).unwrap();
    reopened.resume_chat(chat).await.unwrap();
    assert_eq!(checkpoints(&reopened, chat).await, original_points);
    assert_eq!(
        reopened.snapshot(chat).await.unwrap().messages,
        before.messages
    );
    assert_eq!(model.requests.lock().unwrap().len(), 2);
    reopened.shutdown().await;
}

#[tokio::test]
async fn more_than_one_hundred_prompts_stay_selectable_and_keep_raw_messages_usage_and_operations()
{
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("source.txt"), "source value").unwrap();
    let mut replies = vec![
        call("read", "Read", json!({"file_path":"source.txt"})),
        text("Source read"),
    ];
    replies.extend((1..101).map(|index| text(&format!("Reply {index}"))));
    let model = Arc::new(ScriptedModel::new(replies));
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let first = agent.submit(chat, "Prompt 0".into()).await.unwrap();
    completion(&mut events, Some(first.run_id)).await;
    let initial = agent.snapshot(chat).await.unwrap();
    let initial_point = initial.rewind_points()[0].clone();
    let initial_operation = serde_json::to_value(&initial.operations[0]).unwrap();
    for index in 1..101 {
        let receipt = agent.submit(chat, format!("Prompt {index}")).await.unwrap();
        completion(&mut events, Some(receipt.run_id)).await;
    }
    let snapshot = agent.snapshot(chat).await.unwrap();
    let points = snapshot.rewind_points();
    assert_eq!(points.len(), 101);
    assert_eq!(points[0], initial_point);
    assert_eq!(points[1].prompt, "Prompt 1");
    assert_eq!(points[1].message_index, 4);
    assert_eq!(points[100].prompt, "Prompt 100");
    assert_eq!(points[100].message_index, 202);
    assert_eq!(snapshot.messages.len(), 204);
    assert_eq!(snapshot.messages[0], initial.messages[0]);
    assert_eq!(support::model_script_requests(&snapshot).len(), 102);
    assert_eq!(
        support::model_script_requests(&snapshot)[0],
        support::model_script_requests(&initial)[0]
    );
    assert_eq!(snapshot.operations.len(), 1);
    assert_eq!(
        serde_json::to_value(&snapshot.operations[0]).unwrap(),
        initial_operation
    );
    assert_eq!(model.requests.lock().unwrap().len(), 102);
    agent.shutdown().await;
}

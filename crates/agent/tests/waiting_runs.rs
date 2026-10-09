mod support;

use serde_json::json;
use std::sync::Arc;
use support::*;
use tickloop_agent::{
    model::Content,
    permissions::{Decision, PermissionRule},
    runtime::{Agent, EventKind, LoopState, QuestionAnswer},
    tools::ASK_USER_QUESTION_TOOL_NAME,
};
use uuid::Uuid;

async fn require_workspace_write_approval(agent: &Agent, chat: Uuid) {
    agent
        .set_permission_rules(
            chat,
            vec![PermissionRule {
                id: Uuid::new_v4(),
                decision: Decision::Ask,
                tool: "Write".into(),
                specifier: None,
                input: None,
                loop_id: None,
                persistent: false,
            }],
        )
        .await
        .unwrap();
}

struct GatedSecondModel {
    model: Arc<ScriptedModel>,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    calls: std::sync::atomic::AtomicUsize,
    active: std::sync::atomic::AtomicUsize,
    maximum: std::sync::atomic::AtomicUsize,
}

#[tokio::test]
async fn queued_follow_up_preserves_the_running_goals_ability_to_ask_for_clarification() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(GatedSecondModel {
        model: Arc::new(ScriptedModel::new(vec![
            text("Original requirement noted."),
            call(
                "question",
                ASK_USER_QUESTION_TOOL_NAME,
                single_question("Which format?"),
            ),
            text("Clarification received."),
            text("Additional input applied."),
        ])),
        entered: Arc::new(tokio::sync::Notify::new()),
        release: Arc::new(tokio::sync::Notify::new()),
        calls: std::sync::atomic::AtomicUsize::new(0),
        active: std::sync::atomic::AtomicUsize::new(0),
        maximum: std::sync::atomic::AtomicUsize::new(0),
    });
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let first = agent
        .submit(chat, None, "Prepare a report.".into())
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
    let running = agent
        .submit(chat, Some(first.loop_id), "Clarify the format.".into())
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(3), model.entered.notified())
        .await
        .unwrap();
    let queued = agent
        .submit(
            chat,
            Some(first.loop_id),
            "Also keep the report brief.".into(),
        )
        .await
        .unwrap();
    assert_eq!(
        agent.snapshot(chat).await.unwrap().loops[0].state,
        LoopState::Running
    );
    model.release.notify_one();
    let mut completed = Vec::new();
    let mut merged = false;
    while completed.is_empty() || !merged {
        let event = next_event(&mut events).await;
        match event.kind {
            EventKind::QuestionAsked { request } => {
                assert_eq!(request.run_id, running.run_id);
                agent
                    .answer_question(chat, request.id, vec![QuestionAnswer::other("Markdown")])
                    .await
                    .unwrap();
            }
            EventKind::Completed { .. } => completed.push(event.run_id),
            EventKind::InputMerged { into_run_id } if event.run_id == queued.run_id => {
                assert_eq!(into_run_id, running.run_id);
                merged = true;
            }
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    assert_eq!(completed, vec![running.run_id]);
    agent.shutdown().await;
}

#[async_trait::async_trait]
impl tickloop_agent::model::ModelClient for GatedSecondModel {
    async fn generate_title(
        &self,
        request: tickloop_agent::model::ModelRequest,
        _: tokio::sync::mpsc::UnboundedSender<tickloop_agent::model::ModelDelta>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_agent::model::ModelOutput, tickloop_agent::model::ModelError> {
        Ok(support::title_output(request))
    }

    fn identity(&self) -> tickloop_agent::model::ModelIdentity {
        self.model.identity()
    }

    async fn complete(
        &self,
        request: tickloop_agent::model::ModelRequest,
        events: tokio::sync::mpsc::UnboundedSender<tickloop_agent::model::ModelDelta>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<tickloop_agent::model::ModelOutput, tickloop_agent::model::ModelError> {
        use std::sync::atomic::Ordering::SeqCst;
        let active = self.active.fetch_add(1, SeqCst) + 1;
        self.maximum.fetch_max(active, SeqCst);
        if self.calls.fetch_add(1, SeqCst) == 1 {
            self.entered.notify_one();
            tokio::select! {_=cancel.cancelled()=>{},_=self.release.notified()=>{}}
        }
        let result = self.model.complete(request, events, cancel).await;
        self.active.fetch_sub(1, SeqCst);
        result
    }
}

#[tokio::test]
async fn a_goal_waiting_for_permission_releases_the_chat_and_resumes_with_later_results() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "write",
            "Write",
            json!({"file_path":"draft.txt","content":"approved draft"}),
        ),
        text("The independent second goal is complete."),
        text("The first goal completed after approval."),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    require_workspace_write_approval(&agent, chat).await;
    let mut events = agent.subscribe();
    let first = agent
        .submit(chat, None, "Write the draft after approval.".into())
        .await
        .unwrap();
    let permission = loop {
        match next_event(&mut events).await.kind {
            EventKind::PermissionRequested { request } => break request,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    };
    let second = agent
        .submit(chat, None, "Complete an independent second goal.".into())
        .await
        .unwrap();
    loop {
        let event = next_event(&mut events).await;
        match event.kind {
            EventKind::Completed { .. } if event.run_id == second.run_id => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    assert!(!directory.path().join("draft.txt").exists());
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(
        snapshot
            .loops
            .iter()
            .find(|goal| goal.id == first.loop_id)
            .unwrap()
            .state,
        LoopState::WaitingPermission
    );
    agent
        .answer_permission(chat, permission.id, true)
        .await
        .unwrap();
    loop {
        let event = next_event(&mut events).await;
        match event.kind {
            EventKind::Completed { .. } if event.run_id == first.run_id => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    assert_eq!(
        std::fs::read_to_string(directory.path().join("draft.txt")).unwrap(),
        "approved draft"
    );
    let requests = model.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 3);
    assert!(requests[2].messages.iter().flat_map(|message| &message.content).any(|content| matches!(content,Content::Text{text} if text == "The independent second goal is complete.")));
    agent.shutdown().await;
}

#[tokio::test]
async fn answering_a_question_waits_for_the_active_goals_model_turn_before_resuming() {
    let directory = tempfile::tempdir().unwrap();
    let script = Arc::new(ScriptedModel::new(vec![
        call(
            "question",
            ASK_USER_QUESTION_TOOL_NAME,
            single_question("Which report?"),
        ),
        text("Second goal result."),
        text("First goal uses that result and the answer."),
    ]));
    let model = Arc::new(GatedSecondModel {
        model: script.clone(),
        entered: Arc::new(tokio::sync::Notify::new()),
        release: Arc::new(tokio::sync::Notify::new()),
        calls: std::sync::atomic::AtomicUsize::new(0),
        active: std::sync::atomic::AtomicUsize::new(0),
        maximum: std::sync::atomic::AtomicUsize::new(0),
    });
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let first = agent
        .submit(chat, None, "Clarify before modifying my report.".into())
        .await
        .unwrap();
    let question = loop {
        match next_event(&mut events).await.kind {
            EventKind::QuestionAsked { request } => break request,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    };
    let second = agent
        .submit(chat, None, "Work on a separate goal.".into())
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), model.entered.notified())
        .await
        .unwrap();
    agent
        .answer_question(
            chat,
            question.id,
            vec![QuestionAnswer::other("Weekly report.")],
        )
        .await
        .unwrap();
    tokio::task::yield_now().await;
    assert_eq!(model.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(
        agent
            .snapshot(chat)
            .await
            .unwrap()
            .loops
            .iter()
            .find(|goal| goal.id == first.loop_id)
            .unwrap()
            .state,
        LoopState::WaitingQuestion
    );
    model.release.notify_one();
    let mut completed = Vec::new();
    while completed.len() < 2 {
        let event = next_event(&mut events).await;
        match event.kind {
            EventKind::Completed { .. } => completed.push(event.loop_id),
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    assert_eq!(completed, [second.loop_id, first.loop_id]);
    assert_eq!(model.maximum.load(std::sync::atomic::Ordering::SeqCst), 1);
    let requests = script.requests.lock().unwrap().clone();
    assert!(
        requests[2]
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .any(|block| matches!(block,Content::Text{text} if text=="Second goal result."))
    );
    assert!(requests[2].messages.iter().flat_map(|message|&message.content).any(|block|matches!(block,Content::ToolResult{content,..} if content.text().contains("Weekly report."))));
    agent.shutdown().await;
}

#[tokio::test]
async fn stopping_an_approved_goal_waiting_for_the_slot_does_not_cancel_the_active_goal() {
    let directory = tempfile::tempdir().unwrap();
    let script = Arc::new(ScriptedModel::new(vec![
        call(
            "write",
            "Write",
            json!({"file_path":"draft.txt","content":"must not be written"}),
        ),
        text("Second goal remains active and completes."),
    ]));
    let model = Arc::new(GatedSecondModel {
        model: script,
        entered: Arc::new(tokio::sync::Notify::new()),
        release: Arc::new(tokio::sync::Notify::new()),
        calls: std::sync::atomic::AtomicUsize::new(0),
        active: std::sync::atomic::AtomicUsize::new(0),
        maximum: std::sync::atomic::AtomicUsize::new(0),
    });
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    require_workspace_write_approval(&agent, chat).await;
    let mut events = agent.subscribe();
    let first = agent
        .submit(chat, None, "Draft after approval.".into())
        .await
        .unwrap();
    let permission = loop {
        match next_event(&mut events).await.kind {
            EventKind::PermissionRequested { request } => break request,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    };
    let second = agent
        .submit(chat, None, "Keep this independent work running.".into())
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), model.entered.notified())
        .await
        .unwrap();
    agent
        .answer_permission(chat, permission.id, true)
        .await
        .unwrap();
    tokio::task::yield_now().await;
    agent.stop(chat, first.loop_id).await.unwrap();
    tokio::task::yield_now().await;
    assert!(!directory.path().join("draft.txt").exists());
    let snapshot = agent.snapshot(chat).await.unwrap();
    assert_eq!(
        snapshot
            .loops
            .iter()
            .find(|goal| goal.id == first.loop_id)
            .unwrap()
            .state,
        LoopState::Stopped
    );
    assert_eq!(
        snapshot
            .loops
            .iter()
            .find(|goal| goal.id == second.loop_id)
            .unwrap()
            .state,
        LoopState::Running
    );
    model.release.notify_one();
    loop {
        let event = next_event(&mut events).await;
        match event.kind {
            EventKind::Completed { .. } if event.run_id == second.run_id => break,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    assert_eq!(model.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert!(!directory.path().join("draft.txt").exists());
    assert!(
        agent
            .answer_permission(chat, permission.id, true)
            .await
            .is_err()
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn an_approved_file_change_waits_until_the_other_goals_real_shell_command_finishes() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "write",
            "Write",
            json!({"file_path":"draft.txt","content":"approved draft"}),
        ),
        call(
            "Bash",
            "Bash",
            json!({"command":"printf ready > command-ready; while [ ! -f command-release ]; do sleep 0.01; done; printf complete"}),
        ),
        text("The shell goal has completed."),
        text("The draft was written after the command."),
    ]));
    let agent = Agent::new_embedded(config(), model.clone());
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    require_workspace_write_approval(&agent, chat).await;
    let mut events = agent.subscribe();
    let first = agent
        .submit(chat, None, "Write after approval.".into())
        .await
        .unwrap();
    let write_permission = loop {
        match next_event(&mut events).await.kind {
            EventKind::PermissionRequested { request } => break request,
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    };
    let second = agent
        .submit(chat, None, "Run an independent shell command.".into())
        .await
        .unwrap();
    loop {
        match next_event(&mut events).await.kind {
            EventKind::PermissionRequested { request } => {
                assert_eq!(request.loop_id, second.loop_id);
                agent
                    .answer_permission(chat, request.id, true)
                    .await
                    .unwrap();
                break;
            }
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !directory.path().join("command-ready").exists() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    agent
        .answer_permission(chat, write_permission.id, true)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    assert!(!directory.path().join("draft.txt").exists());
    std::fs::write(directory.path().join("command-release"), "release").unwrap();
    let mut completed = Vec::new();
    while completed.len() < 2 {
        let event = next_event(&mut events).await;
        match event.kind {
            EventKind::Completed { .. } => completed.push(event.loop_id),
            EventKind::Failed(error) => panic!("{error}"),
            _ => {}
        }
    }
    assert_eq!(completed, [second.loop_id, first.loop_id]);
    assert_eq!(
        std::fs::read_to_string(directory.path().join("draft.txt")).unwrap(),
        "approved draft"
    );
    agent.shutdown().await;
}

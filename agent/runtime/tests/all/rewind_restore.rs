use std::{fs, path::Path, sync::Arc};

use serde_json::json;
use tickloop_agent_model::{ModelRequest, cards::model_for_persistent_selection};
use tickloop_agent_protocol::{
    BranchRequest, ChatSnapshot, Content, EventKind, ExecutionMode, InputContext, LoopState,
    ModelChange, Protocol, RewindMode, Trigger, TriggerPromptSource, TriggerSource,
};
use tickloop_agent_runtime::Agent;
use tickloop_agent_test_support::{ScriptedModel, call, config, next_event, text, transcript_file};
use uuid::Uuid;

async fn run_prompt(
    agent: &Agent,
    events: &mut tokio::sync::broadcast::Receiver<tickloop_agent_protocol::Event>,
    chat: Uuid,
    prompt: &str,
) -> tickloop_agent_protocol::Receipt {
    let receipt = agent
        .submit(chat, prompt.into())
        .await
        .expect("prompt accepted");
    loop {
        let event = next_event(events).await;
        if event.run_id != receipt.run_id {
            continue;
        }
        match event.kind {
            EventKind::PermissionRequested { request } => agent
                .answer_permission(chat, request.id, true)
                .await
                .expect("permission accepted"),
            EventKind::Completed { .. } => return receipt,
            EventKind::Failed(error) => panic!("prompt failed: {error}"),
            _ => {}
        }
    }
}

fn message_text(messages: &[tickloop_agent_protocol::Message]) -> String {
    messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|content| match content {
            Content::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn model_text(snapshot: &ChatSnapshot) -> String {
    message_text(&snapshot.conversation_messages().unwrap())
}

fn request_text(request: &ModelRequest) -> String {
    message_text(&request.messages)
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap()
}

#[tokio::test]
async fn code_rewind_restores_modified_created_and_deleted_files() {
    let directory = tempfile::tempdir().unwrap();
    let workdir = directory.path().join("work");
    fs::create_dir(&workdir).unwrap();
    let modified = workdir.join("modified.txt");
    let created = workdir.join("created.txt");
    let deleted = workdir.join("deleted.txt");
    fs::write(&modified, "original modified\n").unwrap();
    fs::write(&deleted, "original deleted\n").unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call("read-modified", "Read", json!({"file_path":"modified.txt"})),
        call(
            "edit",
            "Edit",
            json!({"file_path":"modified.txt","old_string":"original","new_string":"agent"}),
        ),
        call(
            "create",
            "Write",
            json!({"file_path":"created.txt","content":"agent created\n"}),
        ),
        call("read-deleted", "Read", json!({"file_path":"deleted.txt"})),
        call(
            "rewrite",
            "Write",
            json!({"file_path":"deleted.txt","content":"agent rewrote\n"}),
        ),
        call("remove", "Bash", json!({"command":"rm deleted.txt"})),
        text("changed three files"),
        text("second answer"),
    ]));
    let agent = Agent::open_embedded(directory.path().join("state"), config(), model).unwrap();
    agent.install_own_tickhost();
    let chat = agent.create_chat(workdir.clone()).await.unwrap();
    let mut events = agent.subscribe();
    run_prompt(&agent, &mut events, chat, "change three files").await;
    run_prompt(&agent, &mut events, chat, "say something else").await;
    assert_eq!(read(&modified), "agent modified\n");
    assert_eq!(read(&created), "agent created\n");
    assert!(!deleted.exists());

    let before = agent.snapshot(chat).await.unwrap();
    let points = before.rewind_points();
    assert_eq!(points.len(), 2);
    let mut expected_changes = vec![
        created.canonicalize().unwrap(),
        workdir.canonicalize().unwrap().join("deleted.txt"),
        modified.canonicalize().unwrap(),
    ];
    expected_changes.sort();
    let mut previewed = agent
        .rewind_file_changes(chat, points[0].message_id)
        .await
        .unwrap()
        .unwrap();
    previewed.sort();
    assert_eq!(previewed, expected_changes);

    let outcome = agent
        .rewind(chat, points[0].message_id, RewindMode::Code)
        .await
        .unwrap();
    let code = outcome.code.unwrap();
    assert!(code.error.is_none());
    assert!(code.failed_files.is_empty());
    let mut changed = code.changed_files;
    changed.sort();
    assert_eq!(changed, expected_changes);
    assert!(!outcome.conversation_restored);
    assert!(outcome.draft.is_none());
    assert_eq!(read(&modified), "original modified\n");
    assert!(!created.exists());
    assert_eq!(read(&deleted), "original deleted\n");
    assert_eq!(outcome.snapshot.messages, before.messages);
    agent.shutdown().await;
}

#[tokio::test]
async fn continuing_after_rewind_forks_the_transcript_and_restart_resumes_the_new_branch() {
    let directory = tempfile::tempdir().unwrap();
    let state = directory.path().join("state");
    let model = Arc::new(ScriptedModel::new(vec![
        text("first answer"),
        text("abandoned answer"),
        text("replacement answer"),
        text("answer after restart"),
    ]));
    let agent = Agent::open_embedded(state.clone(), config(), model.clone()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    run_prompt(&agent, &mut events, chat, "first prompt").await;
    run_prompt(&agent, &mut events, chat, "abandoned prompt").await;
    let points = agent.snapshot(chat).await.unwrap().rewind_points();

    let outcome = agent
        .rewind(chat, points[1].message_id, RewindMode::Conversation)
        .await
        .unwrap();
    assert!(outcome.conversation_restored);
    assert!(outcome.code.is_none());
    assert_eq!(outcome.snapshot.messages.len(), 2);
    assert!(!model_text(&outcome.snapshot).contains("abandoned"));
    let draft = outcome.draft.unwrap();
    assert_eq!(draft.prompt, "abandoned prompt");
    run_prompt(&agent, &mut events, chat, "replacement prompt").await;
    assert!(
        !request_text(model.requests.lock().unwrap().last().unwrap()).contains("abandoned answer")
    );
    let current_points = agent.snapshot(chat).await.unwrap().rewind_points();
    agent.shutdown().await;
    drop(agent);

    let transcript = read(&transcript_file(&state, chat));
    assert!(transcript.contains("abandoned answer"));
    assert!(transcript.contains("replacement answer"));

    let reopened = Agent::open_embedded(state, config(), model.clone()).unwrap();
    let restored = reopened.resume_chat(chat).await.unwrap();
    assert_eq!(
        message_text(&restored.messages),
        "first prompt\nfirst answer\nreplacement prompt\nreplacement answer"
    );
    assert_eq!(restored.rewind_points(), current_points);
    let mut events = reopened.subscribe();
    run_prompt(&reopened, &mut events, chat, "prompt after restart").await;
    let request = request_text(model.requests.lock().unwrap().last().unwrap());
    assert!(request.contains("replacement answer"));
    assert!(!request.contains("abandoned answer"));
    reopened.shutdown().await;
}

#[tokio::test]
async fn rewind_restores_the_execution_mode_of_the_target_prompt_even_after_restart() {
    let directory = tempfile::tempdir().unwrap();
    let state = directory.path().join("state");
    let model = Arc::new(ScriptedModel::new(vec![
        text("answer in accept edits"),
        text("answer in dont ask"),
    ]));
    let agent = Agent::open_embedded(state.clone(), config(), model.clone()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    agent
        .set_execution_mode(chat, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    run_prompt(&agent, &mut events, chat, "first prompt").await;
    agent
        .set_execution_mode(chat, ExecutionMode::Plan)
        .await
        .unwrap();
    run_prompt(&agent, &mut events, chat, "second prompt").await;
    agent.shutdown().await;
    drop(agent);

    let reopened = Agent::open_embedded(state, config(), model).unwrap();
    let restored = reopened.resume_chat(chat).await.unwrap();
    assert_eq!(restored.policy.mode, ExecutionMode::AcceptEdits);
    let points = restored.rewind_points();
    assert_eq!(
        points
            .iter()
            .map(|point| point.execution_mode)
            .collect::<Vec<_>>(),
        [ExecutionMode::AcceptEdits, ExecutionMode::Plan]
    );
    let second = reopened
        .rewind(chat, points[1].message_id, RewindMode::Conversation)
        .await
        .unwrap();
    assert_eq!(second.snapshot.policy.mode, ExecutionMode::Plan);
    let first = reopened
        .rewind(chat, points[0].message_id, RewindMode::Conversation)
        .await
        .unwrap();
    assert_eq!(first.snapshot.policy.mode, ExecutionMode::AcceptEdits);
    assert_eq!(
        reopened.snapshot(chat).await.unwrap().policy.mode,
        ExecutionMode::AcceptEdits
    );
    reopened.shutdown().await;
}

#[tokio::test]
async fn forked_and_branched_chats_restore_files_changed_before_they_split() {
    let directory = tempfile::tempdir().unwrap();
    let state = directory.path().join("state");
    let workdir = directory.path().join("work");
    fs::create_dir(&workdir).unwrap();
    let tracked = workdir.join("tracked.txt");
    fs::write(&tracked, "before any prompt\n").unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call("read", "Read", json!({"file_path":"tracked.txt"})),
        call(
            "write",
            "Write",
            json!({"file_path":"tracked.txt","content":"changed before the split\n"}),
        ),
        text("changed the file"),
        text("second answer"),
    ]));
    let agent = Agent::open_embedded(state.clone(), config(), model.clone()).unwrap();
    let source = agent.create_chat(workdir.clone()).await.unwrap();
    let mut events = agent.subscribe();
    run_prompt(&agent, &mut events, source, "change the file").await;
    run_prompt(&agent, &mut events, source, "keep talking").await;
    let source_points = agent.snapshot(source).await.unwrap().rewind_points();

    let branch = agent
        .branch_chat(BranchRequest {
            request_id: Uuid::new_v4(),
            source_chat_id: source,
            name: None,
        })
        .await
        .unwrap();
    let branch_points = branch.snapshot.rewind_points();
    assert_eq!(
        branch_points
            .iter()
            .map(|point| (point.message_id, point.prompt.clone()))
            .collect::<Vec<_>>(),
        source_points
            .iter()
            .map(|point| (point.message_id, point.prompt.clone()))
            .collect::<Vec<_>>()
    );
    agent
        .rewind(
            branch.child_chat_id,
            branch_points[0].message_id,
            RewindMode::Code,
        )
        .await
        .unwrap();
    assert_eq!(read(&tracked), "before any prompt\n");
    agent
        .rewind(
            branch.child_chat_id,
            branch_points[1].message_id,
            RewindMode::Code,
        )
        .await
        .unwrap();
    assert_eq!(read(&tracked), "changed before the split\n");

    let copy = agent.resume_as_new_chat(source).await.unwrap();
    assert_ne!(copy.id, source);
    fs::remove_dir_all(state.join("file-history").join(source.to_string())).unwrap();
    agent
        .rewind(copy.id, source_points[0].message_id, RewindMode::Code)
        .await
        .unwrap();
    assert_eq!(read(&tracked), "before any prompt\n");
    agent.shutdown().await;
    drop(agent);

    let reopened = Agent::open_embedded(state, config(), model).unwrap();
    let restored = reopened.resume_chat(copy.id).await.unwrap();
    let points = restored.rewind_points();
    assert_eq!(points.len(), 2);
    reopened
        .rewind(copy.id, points[1].message_id, RewindMode::Code)
        .await
        .unwrap();
    assert_eq!(read(&tracked), "changed before the split\n");
    reopened
        .rewind(
            copy.id,
            points[0].message_id,
            RewindMode::CodeAndConversation,
        )
        .await
        .unwrap();
    assert_eq!(read(&tracked), "before any prompt\n");
    let restored_branch = reopened.resume_chat(branch.child_chat_id).await.unwrap();
    let branch_points = restored_branch.rewind_points();
    assert_eq!(branch_points.len(), 2);
    let branch_rewind = reopened
        .rewind(
            branch.child_chat_id,
            branch_points[1].message_id,
            RewindMode::CodeAndConversation,
        )
        .await
        .unwrap();
    assert_eq!(read(&tracked), "changed before the split\n");
    assert!(branch_rewind.conversation_restored);
    assert_eq!(
        branch_rewind.snapshot.messages.len(),
        branch_points[1].message_index
    );
    assert!(branch_rewind.snapshot.public_conversation_items().is_ok());
    reopened.shutdown().await;
}

#[tokio::test]
async fn snapshots_beyond_one_hundred_drop_the_oldest_without_breaking_rewind() {
    const PROMPTS_AFTER_THE_EDIT: usize = 100;
    let directory = tempfile::tempdir().unwrap();
    let tracked = directory.path().join("tracked.txt");
    fs::write(&tracked, "v0\n").unwrap();
    let mut replies = vec![
        call("read", "Read", json!({"file_path":"tracked.txt"})),
        call(
            "write",
            "Write",
            json!({"file_path":"tracked.txt","content":"v1\n"}),
        ),
        text("wrote v1"),
    ];
    replies.extend((1..=PROMPTS_AFTER_THE_EDIT).map(|index| text(&format!("reply {index}"))));
    let agent = Agent::new_embedded(config(), Arc::new(ScriptedModel::new(replies)));
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    run_prompt(&agent, &mut events, chat, "prompt 0").await;
    for index in 1..=PROMPTS_AFTER_THE_EDIT {
        run_prompt(&agent, &mut events, chat, &format!("prompt {index}")).await;
    }
    let points = agent.snapshot(chat).await.unwrap().rewind_points();
    assert_eq!(points.len(), PROMPTS_AFTER_THE_EDIT + 1);

    let evicted = agent
        .rewind(chat, points[0].message_id, RewindMode::Code)
        .await
        .unwrap_err();
    assert!(evicted.to_string().contains("snapshot was not found"));
    assert_eq!(
        agent
            .rewind_file_changes(chat, points[0].message_id)
            .await
            .unwrap(),
        None
    );
    assert_eq!(read(&tracked), "v1\n");

    fs::write(&tracked, "manual change\n").unwrap();
    let retained = agent
        .rewind(chat, points[1].message_id, RewindMode::Code)
        .await
        .unwrap();
    assert!(retained.code.unwrap().error.is_none());
    assert_eq!(read(&tracked), "v1\n");

    let both = agent
        .rewind(chat, points[0].message_id, RewindMode::CodeAndConversation)
        .await
        .unwrap();
    assert!(both.code.unwrap().error.is_some());
    assert!(both.conversation_restored);
    assert!(both.snapshot.messages.is_empty());
    assert_eq!(read(&tracked), "v1\n");
    agent.shutdown().await;
}

#[tokio::test]
async fn conversation_rewind_truncates_messages_and_binds_the_draft_to_a_new_loop() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        text("first answer"),
        text("abandoned second answer"),
        text("replacement answer"),
    ]));
    let agent =
        Agent::open_embedded(directory.path().join("state"), config(), model.clone()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    run_prompt(&agent, &mut events, chat, "first prompt").await;
    let second = run_prompt(&agent, &mut events, chat, "second prompt").await;

    let before = agent.snapshot(chat).await.unwrap();
    let point = before.rewind_points()[1].clone();
    assert_eq!(point.message_index, 2);
    agent.stop(chat).await.unwrap();
    let result = agent
        .rewind(chat, point.message_id, RewindMode::Conversation)
        .await
        .unwrap();
    assert_eq!(result.snapshot.messages, before.messages[..2]);
    assert_eq!(result.snapshot.request_usage, before.request_usage);
    assert_eq!(result.snapshot.rewind_points(), before.rewind_points()[..1]);
    assert!(
        result
            .snapshot
            .events
            .iter()
            .all(|event| event.run_id != second.run_id)
    );
    let draft = result.draft.unwrap();
    assert_eq!(draft.prompt, "second prompt");
    // rewind 只截断对话，Chat 的 Loop 保留。
    assert_eq!(result.snapshot.chat_loop.id, second.loop_id);
    assert_eq!(result.snapshot.chat_loop.state, LoopState::Idle);

    run_prompt(&agent, &mut events, chat, &draft.prompt).await;
    let branched = agent.snapshot(chat).await.unwrap();
    assert!(!model_text(&branched).contains("abandoned second answer"));
    assert!(model_text(&branched).contains("replacement answer"));
    assert_eq!(branched.chat_loop.id, second.loop_id);
    let replacement_request = request_text(&model.requests.lock().unwrap()[2]);
    assert!(!replacement_request.contains("abandoned second answer"));
    assert!(!replacement_request.contains("\"rewinds\""));
    agent.shutdown().await;
}

#[tokio::test]
async fn conversation_rewind_keeps_the_loop_and_its_trigger() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        text("initial answer"),
        text("checked again"),
    ]));
    let agent = Agent::open_embedded(directory.path().join("state"), config(), model).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let receipt = run_prompt(&agent, &mut events, chat, "keep checking this").await;
    let point = agent.snapshot(chat).await.unwrap().rewind_points()[0].clone();
    let trigger_id = Uuid::new_v4();
    agent
        .configure_trigger(
            chat,
            Some(Trigger {
                id: trigger_id,
                prompt: "check again when requested".into(),
                prompt_source: TriggerPromptSource::Explicit,
                accepts_events: true,
                armed: true,
                subscriptions: Vec::new(),
            }),
        )
        .await
        .unwrap();

    let outcome = agent
        .rewind(chat, point.message_id, RewindMode::Conversation)
        .await
        .unwrap();
    let chat_loop = &outcome.snapshot.chat_loop;
    assert_eq!(chat_loop.id, receipt.loop_id);
    assert_eq!(chat_loop.state, LoopState::Idle);
    assert_eq!(chat_loop.trigger.as_ref().unwrap().id, trigger_id);
    assert!(outcome.draft.is_some());
    agent
        .trigger(
            chat,
            TriggerSource {
                source: "after rewind".into(),
                payload: serde_json::Value::Null,
            },
        )
        .await
        .unwrap();
    agent.shutdown().await;
}

#[tokio::test]
async fn code_rewind_moves_backward_and_forward_and_survives_restart() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call(
            "create-v1",
            "Write",
            json!({"file_path":"version.txt","content":"version one\r\nwithout final newline"}),
        ),
        text("created v1"),
        call(
            "write-v2",
            "Write",
            json!({"file_path":"version.txt","content":"version two\n"}),
        ),
        text("wrote v2"),
        text("recorded the current point"),
    ]));
    let state = directory.path().join("state");
    let agent = Agent::open_embedded(state.clone(), config(), model.clone()).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    run_prompt(&agent, &mut events, chat, "create version one").await;
    run_prompt(&agent, &mut events, chat, "write version two").await;
    run_prompt(&agent, &mut events, chat, "remember version two").await;
    let snapshot = agent.snapshot(chat).await.unwrap();
    let points = snapshot.rewind_points();
    let version = directory.path().join("version.txt");

    let backward = agent
        .rewind(chat, points[1].message_id, RewindMode::Code)
        .await
        .unwrap();
    assert_eq!(backward.code.unwrap().changed_files.len(), 1);
    assert_eq!(
        fs::read(&version).unwrap(),
        b"version one\r\nwithout final newline"
    );
    assert_eq!(backward.snapshot.messages, snapshot.messages);
    assert!(model_text(&backward.snapshot).contains("wrote v2"));

    agent
        .rewind(chat, points[2].message_id, RewindMode::Code)
        .await
        .unwrap();
    assert_eq!(fs::read(&version).unwrap(), b"version two\n");
    agent.shutdown().await;
    drop(agent);

    let reopened =
        Agent::open_embedded(state, config(), Arc::new(ScriptedModel::new(Vec::new()))).unwrap();
    reopened.resume_chat(chat).await.unwrap();
    assert_eq!(fs::read(&version).unwrap(), b"version two\n");
    reopened
        .rewind(chat, points[0].message_id, RewindMode::Code)
        .await
        .unwrap();
    assert!(!version.exists());
    reopened.shutdown().await;
}

#[tokio::test]
async fn conversation_restore_returns_the_original_saved_input_context() {
    let directory = tempfile::tempdir().unwrap();
    let referenced = directory.path().join("notes.md");
    fs::write(&referenced, "old reference\n").unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        text("used old reference"),
        text("later answer"),
    ]));
    let agent = Agent::open_embedded(directory.path().join("state"), config(), model).unwrap();
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    let first = agent
        .submit_with_file_references(chat, "review @notes.md".into(), vec![referenced.clone()])
        .await
        .unwrap();
    loop {
        let event = next_event(&mut events).await;
        if event.run_id == first.run_id && matches!(event.kind, EventKind::Completed { .. }) {
            break;
        }
    }
    run_prompt(&agent, &mut events, chat, "later prompt").await;
    fs::write(&referenced, "new disk content\n").unwrap();
    let point = agent.snapshot(chat).await.unwrap().rewind_points()[0].clone();
    let result = agent
        .rewind(chat, point.message_id, RewindMode::Conversation)
        .await
        .unwrap();
    let draft = result.draft.unwrap();
    assert_eq!(draft.prompt, "review @notes.md");
    assert_eq!(draft.contexts.len(), 1);
    assert!(matches!(
        &draft.contexts[0],
        InputContext::File(file) if file.content == "old reference\n"
    ));
    assert_eq!(read(&referenced), "new disk content\n");
    agent.shutdown().await;
}

#[tokio::test]
async fn rewind_across_a_compaction_boundary_drops_the_boundary_and_restores_raw_history() {
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        text("first answer"),
        text("compacted summary"),
        text("second answer"),
    ]));
    let agent = Agent::new_embedded(config(), model);
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    run_prompt(&agent, &mut events, chat, "first prompt").await;
    agent.compact(chat).await.unwrap();
    run_prompt(&agent, &mut events, chat, "second prompt").await;
    let points = agent.snapshot(chat).await.unwrap().rewind_points();

    let after_compaction = agent
        .rewind(chat, points[1].message_id, RewindMode::Conversation)
        .await
        .unwrap();
    assert_eq!(
        after_compaction
            .snapshot
            .latest_compact_summary()
            .as_deref(),
        Some("compacted summary")
    );
    assert!(
        !message_text(&after_compaction.snapshot.conversation_messages().unwrap())
            .contains("first answer")
    );
    // 照 Claude Code：截断越过边界时边界随之丢弃，模型重新看到边界之前的原始对话。
    let before_compaction = agent
        .rewind(chat, points[0].message_id, RewindMode::Conversation)
        .await
        .unwrap();
    assert!(before_compaction.snapshot.message_compactions.is_empty());
    assert!(
        before_compaction
            .snapshot
            .conversation_messages()
            .unwrap()
            .is_empty()
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn rewind_preserves_original_private_history_after_a_model_switch() {
    let directory = tempfile::tempdir().unwrap();
    let original = Arc::new(ScriptedModel::new(vec![
        vec![
            Content::Thinking {
                thinking: "private reasoning from the original model".into(),
                signature: Some("original-signature".into()),
            },
            Content::Text {
                text: "public first answer".into(),
            },
        ],
        text("public second answer"),
        text("public summary"),
    ]));
    let agent = Agent::new_embedded(config(), original);
    let chat = agent.create_chat(directory.path().into()).await.unwrap();
    let mut events = agent.subscribe();
    run_prompt(&agent, &mut events, chat, "first prompt").await;
    run_prompt(&agent, &mut events, chat, "second prompt").await;
    agent.compact(chat).await.unwrap();

    let mut selected = config();
    selected.name = "responses-replacement".into();
    selected.protocol = Protocol::Responses;
    selected.model = "replacement-model".into();
    assert_eq!(
        agent.set_model(chat, selected.clone()).await.unwrap(),
        ModelChange::Applied
    );
    let before = agent.snapshot(chat).await.unwrap();
    let points = before.rewind_points();
    agent
        .rewind(chat, points[1].message_id, RewindMode::Conversation)
        .await
        .unwrap();
    let after = agent.snapshot(chat).await.unwrap();
    assert_eq!(after.model, model_for_persistent_selection(selected));
    assert!(after.message_compactions.is_empty());
    assert!(after.messages.iter().flat_map(|message| &message.content).any(|content| matches!(content, Content::Thinking { signature: Some(signature), .. } if signature == "original-signature")));
    assert_eq!(
        after.message_sources.get(&1),
        before.message_sources.get(&1)
    );
    agent.shutdown().await;
}

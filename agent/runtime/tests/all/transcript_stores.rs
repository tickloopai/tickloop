//! The `TranscriptStore` contract, run against the local JSONL store and the in-memory store, and
//! what a Harness does with each store's answers.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tickloop_agent_protocol::{AgentHostOptions, ChatId, Content, EventKind, Message, Role};
use tickloop_agent_runtime::transcript::{
    AppendBatch, BatchId, ChatPlacement, JsonlTranscriptStore, LoadRange, StorePosition,
    TranscriptError, TranscriptStore,
};
use tickloop_agent_runtime::{
    Agent, AgentError, HarnessOptions, HarnessSeams, ToolPolicy, environment::LocalEnvironment,
};
use tickloop_agent_test_support::harness::ChosenModels;
use tickloop_agent_test_support::transcripts::{FaultyTranscriptStore, InMemoryTranscriptStore};
use tickloop_agent_test_support::{ScriptedModel, next_event, text};
use tickloop_agent_transcript::{ChainEntry, CustomTitleEntry, Line};
use uuid::Uuid;

/// A store under test, with what it needs to place a Chat.
struct StoreUnderTest {
    name: &'static str,
    store: Arc<dyn TranscriptStore>,
    /// Another handle on the same Transcripts, as a second recorder holds it.
    another_handle: Arc<dyn TranscriptStore>,
    folder: PathBuf,
    _directory: tempfile::TempDir,
}

fn stores() -> Vec<StoreUnderTest> {
    let jsonl_directory = tempfile::tempdir().unwrap();
    let jsonl_folder = jsonl_directory.path().join("folder");
    std::fs::create_dir_all(&jsonl_folder).unwrap();
    let jsonl = JsonlTranscriptStore::open(jsonl_directory.path().join("root")).unwrap();
    let another_jsonl = JsonlTranscriptStore::open(jsonl_directory.path().join("root")).unwrap();
    let memory = Arc::new(InMemoryTranscriptStore::with_position_gaps(7));
    let memory_directory = tempfile::tempdir().unwrap();
    let memory_folder = memory_directory.path().join("folder");
    std::fs::create_dir_all(&memory_folder).unwrap();
    vec![
        StoreUnderTest {
            name: "jsonl",
            store: Arc::new(jsonl),
            another_handle: Arc::new(another_jsonl),
            folder: jsonl_folder.canonicalize().unwrap(),
            _directory: jsonl_directory,
        },
        StoreUnderTest {
            name: "memory",
            store: memory.clone(),
            another_handle: memory,
            folder: memory_folder.canonicalize().unwrap(),
            _directory: memory_directory,
        },
    ]
}

fn entry(chat: ChatId, uuid: Uuid, parent: Option<Uuid>, millisecond: u64, role: Role) -> Line {
    let mut line = serde_json::json!({
        "type": match role { Role::User => "user", Role::Assistant => "assistant", Role::System => "system" },
        "uuid": uuid,
        "parentUuid": parent,
        "chatId": chat,
        "timestamp": tickloop_agent_base::timestamp::format_timestamp(millisecond),
        "version": "test",
        "message": Message { role, content: vec![Content::Text { text: format!("{uuid}") }] },
        "humanFamilyOrigin": null,
    });
    if role == Role::System {
        line["subtype"] = "compact_boundary".into();
        line["compactMetadata"] = serde_json::json!({"trigger": "manual", "preTokens": 1});
    }
    serde_json::from_value(line).unwrap()
}

fn title(chat: ChatId) -> Line {
    Line::CustomTitle(CustomTitleEntry {
        chat_id: chat,
        custom_title: "kept before the boundary".into(),
    })
}

fn batch(lines: Vec<Line>, expected: StorePosition) -> AppendBatch {
    AppendBatch {
        id: BatchId::new(),
        expected: Some(expected),
        lines,
        events_through: 0,
        display_leaf: None,
        durable: false,
    }
}

fn uuids(chain: &[ChainEntry]) -> Vec<Uuid> {
    chain.iter().map(|entry| entry.uuid).collect()
}

fn json(lines: &[Line]) -> Vec<String> {
    lines
        .iter()
        .map(|line| serde_json::to_string(line).unwrap())
        .collect()
}

/// Root prompt and answer, a compact boundary that starts the chain again, and a prompt after it.
struct History {
    chat: ChatId,
    prompt: Uuid,
    answer: Uuid,
    boundary: Uuid,
    after: Uuid,
    lines: Vec<Line>,
    position: StorePosition,
}

async fn write_history(under_test: &StoreUnderTest) -> History {
    let chat = Uuid::new_v4();
    let [prompt, answer, boundary, after] = [(); 4].map(|_| Uuid::new_v4());
    let store = &under_test.store;
    let registered = store
        .register(chat, &ChatPlacement::Folder(under_test.folder.clone()))
        .await
        .unwrap();
    let first = vec![
        title(chat),
        entry(chat, prompt, None, 1_000, Role::User),
        entry(chat, answer, Some(prompt), 2_000, Role::Assistant),
    ];
    let first_ack = store
        .append(chat, &batch(first.clone(), registered))
        .await
        .unwrap();
    let second = vec![
        entry(chat, boundary, None, 3_000, Role::System),
        entry(chat, after, Some(boundary), 4_000, Role::User),
    ];
    let second_ack = store
        .append(chat, &batch(second.clone(), first_ack.position))
        .await
        .unwrap();
    assert!(
        second_ack.position > first_ack.position,
        "{}",
        under_test.name
    );
    History {
        chat,
        prompt,
        answer,
        boundary,
        after,
        lines: first.into_iter().chain(second).collect(),
        position: second_ack.position,
    }
}

#[tokio::test]
async fn appended_lines_load_back_in_order_with_the_snapshot_position() {
    for under_test in stores() {
        let history = write_history(&under_test).await;
        let full = under_test
            .store
            .load(history.chat, LoadRange::Full)
            .await
            .unwrap()
            .unwrap();
        assert!(full.retained.is_empty(), "{}", under_test.name);
        assert_eq!(
            json(&full.lines),
            json(&history.lines),
            "{}",
            under_test.name
        );
        assert_eq!(full.position, history.position, "{}", under_test.name);
        if under_test.name == "jsonl" {
            let local = (under_test.store.clone() as Arc<dyn std::any::Any + Send + Sync>)
                .downcast::<JsonlTranscriptStore>()
                .unwrap();
            let written = std::fs::read_to_string(local.chat_path(history.chat).unwrap()).unwrap();
            assert_eq!(written.lines().collect::<Vec<_>>(), json(&history.lines));
        }
    }
}

#[tokio::test]
async fn a_tail_load_keeps_the_retained_records_and_the_chain_from_the_boundary() {
    for under_test in stores() {
        let history = write_history(&under_test).await;
        let tail = under_test
            .store
            .load(history.chat, LoadRange::FromLatestBoundary)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(tail.position, history.position, "{}", under_test.name);
        assert_eq!(
            uuids(&tail.chain_to(None).unwrap()),
            [history.boundary, history.after],
            "{}",
            under_test.name
        );
        let records = tail.retained.iter().chain(&tail.lines);
        assert!(
            records
                .filter(|line| line.is_retained())
                .any(|line| matches!(line, Line::CustomTitle(_))),
            "{}",
            under_test.name
        );
        if under_test.name == "memory" {
            assert_eq!(json(&tail.retained), json(&[title(history.chat)]));
            assert!(
                matches!(&tail.lines[0], Line::System(entry) if entry.uuid == history.boundary)
            );
        }
    }
}

#[tokio::test]
async fn a_leaf_before_the_latest_boundary_loads_from_the_full_range() {
    for under_test in stores() {
        let history = write_history(&under_test).await;
        let load = |range| under_test.store.load(history.chat, range);
        let tail = load(LoadRange::FromLatestBoundary).await.unwrap().unwrap();
        let chain = match tail.chain_to(Some(history.answer)) {
            Some(chain) => chain,
            None => load(LoadRange::Full)
                .await
                .unwrap()
                .unwrap()
                .chain_to(Some(history.answer))
                .unwrap(),
        };
        assert_eq!(
            uuids(&chain),
            [history.prompt, history.answer],
            "{}",
            under_test.name
        );
        assert!(tail.chain_to(Some(Uuid::new_v4())).is_none());
    }
}

#[tokio::test]
async fn retrying_a_committed_batch_commits_it_once() {
    for under_test in stores() {
        let chat = Uuid::new_v4();
        let store = &under_test.store;
        let registered = store
            .register(chat, &ChatPlacement::Folder(under_test.folder.clone()))
            .await
            .unwrap();
        let once = batch(
            vec![entry(chat, Uuid::new_v4(), None, 1, Role::User)],
            registered,
        );
        let first = store.append(chat, &once).await.unwrap();
        let retried = store.append_blocking(chat, &once).unwrap();
        assert_eq!(first, retried, "{}", under_test.name);
        let stored = store.load(chat, LoadRange::Full).await.unwrap().unwrap();
        assert_eq!(stored.lines.len(), 1, "{}", under_test.name);
        assert_eq!(stored.position, first.position, "{}", under_test.name);
    }
}

fn options(tools: ToolPolicy) -> HarnessOptions {
    HarnessOptions {
        model: tickloop_agent_test_support::config(),
        settings: None,
        host: AgentHostOptions::embedded_defaults(),
        tools,
        host_rules: Vec::new(),
    }
}

fn local_agent(store: Arc<dyn TranscriptStore>, replies: usize) -> Agent {
    let model = ScriptedModel::new(
        (0..replies)
            .map(|index| text(&format!("answer {index}")))
            .collect(),
    );
    Agent::with_seams(
        HarnessSeams {
            transcripts: Some(store),
            environment: Some(Arc::new(LocalEnvironment::bare())),
            models: ChosenModels::new(Arc::new(model)),
        },
        options(ToolPolicy::local()),
    )
    .unwrap()
}

async fn complete_turn(agent: &Agent, chat: ChatId, prompt: &str) {
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, None, prompt.into()).await.unwrap();
    loop {
        let event = next_event(&mut events).await;
        if event.run_id != receipt.run_id {
            continue;
        }
        match event.kind {
            EventKind::Completed { .. } => return,
            EventKind::Failed(error) => panic!("turn failed: {error}"),
            _ => {}
        }
    }
}

async fn prompts_stored(store: &dyn TranscriptStore, chat: ChatId, prompt: &str) -> usize {
    let stored = store.load(chat, LoadRange::Full).await.unwrap().unwrap();
    stored
        .lines
        .iter()
        .filter(|line| matches!(line, Line::User(entry) if entry.message.content.iter().any(|content| matches!(content, Content::Text { text } if text == prompt))))
        .count()
}

async fn chat_in(agent: &Agent, folder: &Path) -> ChatId {
    agent.create_chat(folder.to_path_buf()).await.unwrap()
}

#[tokio::test]
async fn a_lost_acknowledgement_is_retried_with_the_same_batch_and_commits_once() {
    for under_test in stores() {
        let faulty = Arc::new(FaultyTranscriptStore::new(under_test.store.clone()));
        let agent = local_agent(faulty.clone(), 3);
        let chat = chat_in(&agent, &under_test.folder).await;
        complete_turn(&agent, chat, "first").await;
        agent.sync().await.unwrap();
        faulty.lose_next_acknowledgement();
        complete_turn(&agent, chat, "second").await;
        agent.flush_transcripts().await;
        let positions = agent.sync().await.unwrap();
        assert_eq!(agent.transcript_write_error(), None, "{}", under_test.name);
        let batches = faulty.batches();
        assert!(
            batches
                .iter()
                .enumerate()
                .any(|(index, id)| batches[index + 1..].contains(id)),
            "the batch that lost its acknowledgement is sent again: {}",
            under_test.name
        );
        assert_eq!(
            prompts_stored(&*under_test.store, chat, "second").await,
            1,
            "{}",
            under_test.name
        );
        let stored = under_test
            .store
            .load(chat, LoadRange::Full)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(positions[&chat], stored.position, "{}", under_test.name);

        // A durable batch whose acknowledgement was lost is retried first; the Lines queued
        // after it still reach the store before sync succeeds.
        faulty.lose_next_acknowledgement();
        assert!(matches!(agent.sync().await, Err(AgentError::Storage(_))));
        complete_turn(&agent, chat, "third").await;
        agent.sync().await.unwrap();
        assert_eq!(
            prompts_stored(&*under_test.store, chat, "third").await,
            1,
            "{}",
            under_test.name
        );
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn a_permanent_storage_failure_makes_sync_fail_until_the_store_recovers() {
    for under_test in stores() {
        let faulty = Arc::new(FaultyTranscriptStore::new(under_test.store.clone()));
        let agent = local_agent(faulty.clone(), 1);
        let chat = chat_in(&agent, &under_test.folder).await;
        faulty.fail_appends();
        complete_turn(&agent, chat, "unsaved for now").await;
        assert!(
            matches!(agent.sync().await, Err(AgentError::Storage(_))),
            "{}",
            under_test.name
        );
        assert!(agent.transcript_write_error().is_some());
        faulty.heal();
        agent.sync().await.unwrap();
        assert_eq!(agent.transcript_write_error(), None);
        assert_eq!(
            prompts_stored(&*under_test.store, chat, "unsaved for now").await,
            1,
            "{}",
            under_test.name
        );
        agent.shutdown().await;
    }
}

#[tokio::test]
async fn after_a_fenced_append_the_harness_writes_nothing_more() {
    for under_test in stores() {
        let faulty = Arc::new(FaultyTranscriptStore::new(under_test.store.clone()));
        let agent = local_agent(faulty.clone(), 4);
        let chat = chat_in(&agent, &under_test.folder).await;
        let other = chat_in(&agent, &under_test.folder).await;
        complete_turn(&agent, chat, "kept").await;
        agent.sync().await.unwrap();
        faulty.fence();
        complete_turn(&agent, chat, "fenced").await;
        assert!(matches!(agent.sync().await, Err(AgentError::Fenced)));
        let appends = faulty.appends();
        complete_turn(&agent, chat, "after the fence").await;
        complete_turn(&agent, other, "in another Chat").await;
        assert!(matches!(
            agent.create_chat(under_test.folder.clone()).await,
            Err(AgentError::Fenced)
        ));
        agent.flush_transcripts().await;
        assert!(matches!(agent.sync().await, Err(AgentError::Fenced)));
        agent.shutdown().await;
        drop(agent);
        assert_eq!(faulty.appends(), appends, "{}", under_test.name);
        assert_eq!(prompts_stored(&*under_test.store, chat, "kept").await, 1);
        assert_eq!(prompts_stored(&*under_test.store, chat, "fenced").await, 0);
        assert_eq!(
            prompts_stored(&*under_test.store, chat, "after the fence").await,
            0
        );
        assert!(
            under_test
                .store
                .load(other, LoadRange::Full)
                .await
                .unwrap()
                .is_none_or(|stored| stored.lines.is_empty()),
            "{}",
            under_test.name
        );
    }
}

fn folderless_agent(store: Arc<dyn TranscriptStore>, model: Arc<ScriptedModel>) -> Agent {
    Agent::with_seams(
        HarnessSeams {
            transcripts: Some(store),
            environment: None,
            models: ChosenModels::new(model),
        },
        options(ToolPolicy::web_fetch_only()),
    )
    .unwrap()
}

/// A Chat without a Folder is recorded through an injected store, without a `cwd`. A new holder
/// loads its compacted history, whose positions have gaps, and appends at the loaded position.
#[tokio::test]
async fn a_new_holder_appends_compacted_history_at_the_loaded_position() {
    let store = Arc::new(InMemoryTranscriptStore::with_position_gaps(1_000));
    let first = folderless_agent(
        store.clone(),
        Arc::new(ScriptedModel::new(vec![
            text("first answer"),
            text("summary of the first turn"),
            text("second answer"),
        ])),
    );
    let chat = first.create_chat_without_folder().await.unwrap();
    complete_turn(&first, chat, "first prompt").await;
    first.compact(chat).await.unwrap();
    complete_turn(&first, chat, "second prompt").await;
    first.sync().await.unwrap();
    first.shutdown().await;
    drop(first);

    let lines = store.lines(chat);
    assert!(
        lines
            .iter()
            .filter_map(Line::chain_entry)
            .all(|entry| entry.cwd.is_none())
    );
    assert!(json(&lines).iter().all(|line| !line.contains("\"cwd\"")));
    let loaded_position = store.position(chat).unwrap();
    assert_ne!(
        loaded_position,
        StorePosition(lines.len() as u64),
        "positions have gaps"
    );
    let tail = store
        .load(chat, LoadRange::FromLatestBoundary)
        .await
        .unwrap()
        .unwrap();
    assert!(tail.lines.len() < lines.len(), "the history is compacted");
    let appends_before = store.received(chat).len();

    let model = Arc::new(ScriptedModel::new(vec![text("third answer")]));
    let next = folderless_agent(store.clone(), model.clone());
    let snapshot = next.resume_chat(chat).await.unwrap();
    assert_eq!(snapshot.folder, None);
    complete_turn(&next, chat, "third prompt").await;
    next.sync().await.unwrap();
    let received = store.received(chat);
    assert_eq!(received[appends_before].expected, Some(loaded_position));
    let request = serde_json::to_string(&model.requests.lock().unwrap()[0].messages).unwrap();
    assert!(request.contains("summary of the first turn"), "{request}");
    assert!(request.contains("second prompt"), "{request}");
    next.shutdown().await;
}

fn custom_titles(lines: &[Line]) -> Vec<String> {
    lines
        .iter()
        .filter_map(|line| match line {
            Line::CustomTitle(entry) => Some(entry.custom_title.clone()),
            _ => None,
        })
        .collect()
}

fn titled(chat: ChatId, title: &str) -> Line {
    Line::CustomTitle(CustomTitleEntry {
        chat_id: chat,
        custom_title: title.into(),
    })
}

#[tokio::test]
async fn a_batch_retried_after_other_batches_and_registrations_commits_once() {
    for under_test in stores() {
        let chat = Uuid::new_v4();
        let placement = ChatPlacement::Folder(under_test.folder.clone());
        let registered = under_test.store.register(chat, &placement).await.unwrap();
        let first = batch(vec![titled(chat, "A")], registered);
        let acknowledged = under_test.store.append(chat, &first).await.unwrap();
        // Another recorder registers the Chat again and appends before the first one retries.
        let position = under_test
            .another_handle
            .register(chat, &placement)
            .await
            .unwrap();
        under_test
            .another_handle
            .append(chat, &batch(vec![titled(chat, "B")], position))
            .await
            .unwrap();
        assert_eq!(
            under_test.store.append(chat, &first).await.unwrap(),
            acknowledged,
            "{}",
            under_test.name
        );
        assert_eq!(
            under_test
                .another_handle
                .append_blocking(chat, &first)
                .unwrap(),
            acknowledged,
            "{}",
            under_test.name
        );
        let stored = under_test
            .store
            .load(chat, LoadRange::Full)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            custom_titles(&stored.lines),
            ["A", "B"],
            "{}",
            under_test.name
        );
    }
}

/// One recorder loses an acknowledgement; another continues the Chat before the first retries.
/// The retried batch is not written again.
#[tokio::test]
async fn a_lost_acknowledgement_retried_after_another_recorder_wrote_commits_once() {
    for under_test in stores() {
        let faulty = Arc::new(FaultyTranscriptStore::new(under_test.store.clone()));
        let first = local_agent(faulty.clone(), 2);
        let chat = chat_in(&first, &under_test.folder).await;
        complete_turn(&first, chat, "first prompt").await;
        first.sync().await.unwrap();
        faulty.lose_next_acknowledgement();
        complete_turn(&first, chat, "lost acknowledgement").await;
        first.flush_transcripts().await;

        let second = local_agent(under_test.another_handle.clone(), 1);
        second.resume_chat(chat).await.unwrap();
        complete_turn(&second, chat, "another recorder").await;
        second.sync().await.unwrap();
        second.shutdown().await;

        // The in-memory store enforces expected positions, so the first recorder's next batch
        // after its retry conflicts there; the retried Lines are never duplicated either way.
        let synced = first.sync().await;
        assert!(
            synced.is_ok() || under_test.name == "memory",
            "{}: {synced:?}",
            under_test.name
        );
        for prompt in ["lost acknowledgement", "another recorder"] {
            assert_eq!(
                prompts_stored(&*under_test.store, chat, prompt).await,
                1,
                "{}: {prompt}",
                under_test.name
            );
        }
        first.shutdown().await;
    }
}

fn truncate(path: &Path) {
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .set_len(0)
        .unwrap();
}

#[tokio::test]
async fn a_truncated_jsonl_file_conflicts_instead_of_acknowledging_lost_lines() {
    let directory = tempfile::tempdir().unwrap();
    let store = JsonlTranscriptStore::open(directory.path().join("root")).unwrap();
    let chat = Uuid::new_v4();
    let registered = store
        .register(chat, &ChatPlacement::Folder(directory.path().into()))
        .await
        .unwrap();
    let first = batch(vec![titled(chat, "A")], registered);
    let acknowledged = store.append(chat, &first).await.unwrap();
    truncate(&store.chat_path(chat).unwrap());
    let durable = AppendBatch {
        durable: true,
        ..batch(Vec::new(), acknowledged.position)
    };
    for retried in [&durable, &first] {
        match store.append(chat, retried).await {
            Err(TranscriptError::Conflict(message)) => assert!(message.contains("truncated")),
            other => panic!("{other:?}"),
        }
    }

    // Growth from another writer is fine.
    let grown = Uuid::new_v4();
    let registered = store
        .register(grown, &ChatPlacement::Folder(directory.path().into()))
        .await
        .unwrap();
    store
        .append(grown, &batch(vec![titled(grown, "A")], registered))
        .await
        .unwrap();
    let path = store.chat_path(grown).unwrap();
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    std::io::Write::write_all(
        &mut file,
        format!("{}\n", serde_json::to_string(&titled(grown, "B")).unwrap()).as_bytes(),
    )
    .unwrap();
    store
        .append(
            grown,
            &AppendBatch {
                durable: true,
                ..batch(Vec::new(), registered)
            },
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn sync_fails_over_a_truncated_transcript_even_while_retrying_a_lost_acknowledgement() {
    for lose_acknowledgement in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let folder = directory.path().join("folder");
        std::fs::create_dir_all(&folder).unwrap();
        let jsonl = JsonlTranscriptStore::open(directory.path().join("root")).unwrap();
        let faulty = Arc::new(FaultyTranscriptStore::new(Arc::new(jsonl.clone())));
        let agent = local_agent(faulty.clone(), 2);
        let chat = chat_in(&agent, &folder).await;
        complete_turn(&agent, chat, "first").await;
        agent.sync().await.unwrap();
        if lose_acknowledgement {
            faulty.lose_next_acknowledgement();
            complete_turn(&agent, chat, "committed, acknowledgement lost").await;
            agent.flush_transcripts().await;
        }
        truncate(&jsonl.chat_path(chat).unwrap());
        match agent.sync().await {
            Err(AgentError::Storage(message)) => {
                assert!(message.contains("truncated"), "{message}")
            }
            other => panic!("{lose_acknowledgement}: {other:?}"),
        }
        let error = agent.transcript_write_error().unwrap();
        assert!(error.contains("truncated"), "{error}");
        agent.shutdown().await;
    }
}

/// A Fenced answer to a registration or a load stops every write of the Harness, as one to an
/// append does.
#[tokio::test]
async fn a_fence_seen_by_a_registration_or_a_load_stops_every_write() {
    for fenced_by_load in [false, true] {
        let memory = Arc::new(InMemoryTranscriptStore::new());
        let faulty = Arc::new(FaultyTranscriptStore::new(memory.clone()));
        let directory = tempfile::tempdir().unwrap();
        let agent = local_agent(faulty.clone(), 2);
        let chat = chat_in(&agent, directory.path()).await;
        complete_turn(&agent, chat, "before the fence").await;
        memory.fence();
        let seen = if fenced_by_load {
            agent.resume_chat(Uuid::new_v4()).await.map(|_| ())
        } else {
            agent.create_chat(directory.path().into()).await.map(|_| ())
        };
        assert!(matches!(seen, Err(AgentError::Fenced)), "{seen:?}");
        let appends = faulty.appends();
        let registrations = faulty.registrations();

        complete_turn(&agent, chat, "after the fence").await;
        agent.flush_transcripts().await;
        assert!(matches!(agent.sync().await, Err(AgentError::Fenced)));
        assert!(matches!(
            agent.create_chat(directory.path().into()).await,
            Err(AgentError::Fenced)
        ));
        agent.shutdown().await;
        drop(agent);
        assert_eq!(faulty.appends(), appends, "{fenced_by_load}");
        assert_eq!(faulty.registrations(), registrations, "{fenced_by_load}");
    }
}

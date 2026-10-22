//! The local JSONL a fixed sequence of recorder operations writes, line for line. The golden was
//! produced by the file writer that preceded `TranscriptStore`.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use tickloop_agent_protocol::{
    ChatSnapshot, CompactBoundary, CompactTrigger, CompactionMarker, Content, LoopSnapshot,
    LoopState, Message, ModelConfig, Role,
};
use uuid::Uuid;

use super::ChatRecorder;
use crate::ChatBookkeeping;
use crate::transcript::JsonlTranscriptStore;
use tickloop_agent_transcript::ContentReplacementRecord;

const GOLDEN: &str = include_str!("golden_transcript.jsonl");

trait Recording {
    fn sync(&self, snapshot: &ChatSnapshot, bookkeeping: &ChatBookkeeping);
    fn flush(&self);
    fn ai_title(&self, chat: Uuid, title: &str);
    fn replacements(&self, chat: Uuid, records: Vec<ContentReplacementRecord>);
    fn truncate(&self, snapshot: &ChatSnapshot, bookkeeping: &ChatBookkeeping, count: usize);
    fn close(&self, chat: Uuid);
}

fn message(role: Role, text: &str) -> Message {
    Message {
        role,
        content: vec![Content::Text { text: text.into() }],
    }
}

fn tool_result(id: &str) -> Content {
    Content::ToolResult {
        tool_use_id: id.into(),
        content: format!("result {id}").into(),
        is_error: false,
    }
}

fn run_scenario(recording: &dyn Recording, chat: Uuid, folder: &Path) {
    let run = Uuid::from_u128(0x100);
    let loop_id = Uuid::from_u128(0x200);
    let mut snapshot = crate::empty_snapshot(chat, Some(folder.into()), ModelConfig::unselected());
    let mut bookkeeping = ChatBookkeeping::default();
    snapshot.messages.push(message(Role::User, "first prompt"));
    snapshot.message_ids.insert(0, Uuid::from_u128(0x1));
    snapshot.message_runs.insert(0, run);
    snapshot.chat_loop = LoopSnapshot {
        state: LoopState::Running,
        run_id: Some(run),
        run_started_at: Some(1),
        state_since: Some(2),
        ..LoopSnapshot::new(loop_id)
    };
    bookkeeping.run_loops.insert(run, loop_id);
    recording.sync(&snapshot, &bookkeeping);
    recording.flush();

    snapshot.messages.push(Message {
        role: Role::Assistant,
        content: vec![
            Content::Text {
                text: "calling".into(),
            },
            Content::ToolUse {
                id: "call-1".into(),
                name: "Read".into(),
                input: serde_json::json!({"file_path": "a"}),
            },
            Content::ToolUse {
                id: "call-2".into(),
                name: "Read".into(),
                input: serde_json::json!({"file_path": "b"}),
            },
        ],
    });
    snapshot.message_ids.insert(1, Uuid::from_u128(0x2));
    snapshot.message_runs.insert(1, run);
    snapshot.messages.push(Message {
        role: Role::User,
        content: vec![tool_result("call-1")],
    });
    snapshot.message_ids.insert(2, Uuid::from_u128(0x3));
    snapshot.message_runs.insert(2, run);
    recording.sync(&snapshot, &bookkeeping);
    recording.flush();

    snapshot.messages[2].content.push(tool_result("call-2"));
    snapshot.messages.push(message(Role::Assistant, "done"));
    snapshot.message_ids.insert(3, Uuid::from_u128(0x4));
    snapshot.message_runs.insert(3, run);
    snapshot.chat_loop.state = LoopState::Idle;
    snapshot.chat_loop.state_since = Some(3);
    recording.sync(&snapshot, &bookkeeping);
    recording.ai_title(chat, "A golden Chat");
    recording.replacements(
        chat,
        vec![ContentReplacementRecord::ToolResult {
            tool_use_id: "call-1".into(),
            replacement: "replaced".into(),
        }],
    );
    recording.flush();

    recording.truncate(&snapshot, &bookkeeping, 1);
    snapshot.messages.truncate(1);
    snapshot
        .messages
        .push(message(Role::Assistant, "rewound answer"));
    snapshot.message_ids = BTreeMap::from([(0, Uuid::from_u128(0x1)), (1, Uuid::from_u128(0x5))]);
    snapshot.message_runs = BTreeMap::from([(0, run), (1, run)]);
    snapshot
        .messages
        .push(message(Role::System, "summary boundary"));
    snapshot.message_ids.insert(2, Uuid::from_u128(0x6));
    snapshot.message_compactions.insert(
        2,
        CompactionMarker::Boundary(CompactBoundary {
            trigger: CompactTrigger::Manual,
            pre_tokens: 42,
            user_context: None,
            messages_summarized: Some(2),
            preserved_by_later_summary: false,
            original_user_inputs: None,
            model: None,
        }),
    );
    snapshot
        .messages
        .push(message(Role::User, "after the boundary"));
    snapshot.message_ids.insert(3, Uuid::from_u128(0x7));
    recording.sync(&snapshot, &bookkeeping);
    recording.flush();
    recording.close(chat);
}

/// Uuids become their order of first appearance; timestamps, the version and the Folder become
/// placeholders.
fn normalized(text: &str, folder: &Path) -> String {
    let text = text.replace(&*folder.to_string_lossy(), "<FOLDER>");
    let text = replace_string_field(&text, "timestamp", "<TIME>");
    let text = replace_string_field(&text, "version", "<VERSION>");
    let mut seen: Vec<String> = Vec::new();
    let mut output = String::new();
    let mut rest = text.as_str();
    while !rest.is_empty() {
        let candidate = rest
            .get(..36)
            .filter(|slice| Uuid::parse_str(slice).is_ok());
        match candidate {
            Some(uuid) => {
                let index = seen
                    .iter()
                    .position(|known| known == uuid)
                    .unwrap_or_else(|| {
                        seen.push(uuid.to_owned());
                        seen.len() - 1
                    });
                output.push_str(&format!("<U{index}>"));
                rest = &rest[36..];
            }
            None => {
                let character = rest.chars().next().unwrap();
                output.push(character);
                rest = &rest[character.len_utf8()..];
            }
        }
    }
    output
}

fn replace_string_field(text: &str, key: &str, placeholder: &str) -> String {
    let pattern = format!("\"{key}\":\"");
    let mut output = String::new();
    let mut rest = text;
    while let Some(start) = rest.find(&pattern) {
        let value_start = start + pattern.len();
        output.push_str(&rest[..value_start]);
        output.push_str(placeholder);
        let value_end = rest[value_start..].find('"').unwrap() + value_start;
        rest = &rest[value_end..];
    }
    output.push_str(rest);
    output
}

fn assert_golden(written: &str, folder: &Path) {
    let normalized = normalized(written, folder);
    if std::env::var_os("TICKLOOP_UPDATE_GOLDEN").is_some() {
        std::fs::write(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("src/persistence/golden_transcript.jsonl"),
            &normalized,
        )
        .unwrap();
        return;
    }
    let expected = GOLDEN.lines().collect::<Vec<_>>();
    let actual = normalized.lines().collect::<Vec<_>>();
    for (index, (expected, actual)) in expected.iter().zip(&actual).enumerate() {
        assert_eq!(actual, expected, "line {}", index + 1);
    }
    assert_eq!(actual.len(), expected.len(), "{normalized}");
}

struct StoreRecording {
    recorder: ChatRecorder,
    runtime: tokio::runtime::Runtime,
}

impl Recording for StoreRecording {
    fn sync(&self, snapshot: &ChatSnapshot, bookkeeping: &ChatBookkeeping) {
        self.recorder.enqueue_sync(snapshot, bookkeeping);
    }

    fn flush(&self) {
        self.recorder.flush_blocking();
    }

    fn ai_title(&self, chat: Uuid, title: &str) {
        self.recorder.record_ai_title(chat, title.into());
    }

    fn replacements(&self, chat: Uuid, records: Vec<ContentReplacementRecord>) {
        self.recorder.enqueue_content_replacements(chat, records);
    }

    fn truncate(&self, snapshot: &ChatSnapshot, bookkeeping: &ChatBookkeeping, count: usize) {
        self.recorder.truncate(snapshot, bookkeeping, count);
    }

    fn close(&self, chat: Uuid) {
        self.runtime.block_on(async {
            self.recorder.queue_metadata_again(chat).await;
            self.recorder.flush().await;
        });
    }
}

#[test]
fn the_local_store_writes_the_golden_jsonl() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let folder = directory.path().join("folder");
    std::fs::create_dir_all(&folder).unwrap();
    let folder = folder.canonicalize().unwrap();
    let chat = Uuid::from_u128(0xC0);
    let store = JsonlTranscriptStore::open(root).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let recorder = ChatRecorder::new(Arc::new(store.clone()));
    runtime
        .block_on(recorder.register_chat(chat, Some(&folder)))
        .unwrap();
    let recording = StoreRecording { recorder, runtime };
    run_scenario(&recording, chat, &folder);
    let path = store.chat_path(chat).unwrap();
    assert_golden(&std::fs::read_to_string(path).unwrap(), &folder);
}

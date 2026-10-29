use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::json;
use tickloop_harness_model::ModelClient;
use tickloop_harness_protocol::{AgentHostOptions, ExecutionMode, PromptPolicy};
use tickloop_harness_test_support::routed::{RoutedModel, has_tool_result, state};
use tickloop_harness_test_support::{ScriptedModel, call, config, harness::ChosenModels, text};

use super::*;
use crate::transcript::JsonlTranscriptStore;

/// How ralph-loop's Stop hook reads the last assistant text from `transcript_path`, verbatim.
const RALPH_LOOP_EXTRACTION: &str = r#"set -euo pipefail
grep -q '"role":"assistant"' "$1"
grep '"role":"assistant"' "$1" | tail -n 100 | jq -rs '
  map(.message.content[]? | select(.type == "text") | .text) | last // ""
'"#;

const ANSWER: &str = "Read them. <promise>DONE</promise>";

fn ralph_loop_last_assistant_text(transcript: &Path) -> String {
    let output = std::process::Command::new("bash")
        .args(["-c", RALPH_LOOP_EXTRACTION, "ralph-loop"])
        .arg(transcript)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .unwrap()
        .trim_end_matches('\n')
        .to_owned()
}

fn agent(store: Arc<JsonlTranscriptStore>, model: Arc<dyn ModelClient>) -> Agent {
    Agent::with_seams(
        crate::HarnessSeams {
            search: None,
            transcripts: Some(store),
            environment: Some(Arc::new(crate::environment::FolderEnvironment::bare())),
            models: ChosenModels::new(model),
        },
        crate::local_options(
            config(),
            None,
            AgentHostOptions {
                prompt_policy: PromptPolicy::DenyOnPrompt,
                ..AgentHostOptions::embedded_defaults()
            },
        ),
    )
    .unwrap()
}

struct Fixture {
    directory: tempfile::TempDir,
    store: Arc<JsonlTranscriptStore>,
    model: Arc<ScriptedModel>,
    agent: Agent,
    chat: ChatId,
}

impl Fixture {
    fn store_root(&self) -> PathBuf {
        self.directory.path().join("state")
    }

    async fn ask(&self, prompt: &str, answer: Vec<tickloop_harness_protocol::Content>) {
        self.model.enqueue(answer);
        self.agent.submit(self.chat, prompt.into()).await.unwrap();
        self.agent.quiesce(self.chat).await;
    }

    async fn transcript(&self) -> (PathBuf, String) {
        let path = self
            .agent
            .hook_transcript_path(self.chat)
            .await
            .unwrap()
            .expect("a Chat with a Folder has a hook transcript");
        let content = std::fs::read_to_string(&path).unwrap();
        (path, content)
    }
}

/// A Chat that read a file and answered.
async fn answered() -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let workdir = directory.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    std::fs::write(workdir.join("notes.txt"), "remember the tests\n").unwrap();
    let store = Arc::new(JsonlTranscriptStore::open(directory.path().join("state")).unwrap());
    let model = Arc::new(ScriptedModel::new(vec![call(
        "read-notes",
        "Read",
        json!({"file_path": "notes.txt"}),
    )]));
    let agent = agent(store.clone(), model.clone());
    let chat = agent.create_chat(workdir).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    let fixture = Fixture {
        directory,
        store,
        model,
        agent,
        chat,
    };
    fixture.ask("Read the notes", text(ANSWER)).await;
    fixture
}

fn uuids(content: &str) -> Vec<String> {
    content
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap()["uuid"].to_string())
        .collect()
}

fn assert_unique_lines(content: &str) {
    let mut ids = uuids(content);
    let count = ids.len();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), count, "a line was written twice:\n{content}");
}

/// Beside the Chat's own Transcript in the store, as Claude Code keeps its session file.
fn expected_path(fixture: &Fixture) -> PathBuf {
    let stored = fixture.store.chat_path(fixture.chat).unwrap();
    stored
        .parent()
        .unwrap()
        .join(fixture.chat.uuid().to_string())
        .join(HOOK_TRANSCRIPT_FILE)
}

/// A name swapped for a hard link to a file elsewhere is never written through: the transcript
/// is written again as a new file of its own, and the other file stays as it was.
#[tokio::test]
async fn a_hard_link_put_in_its_place_is_never_written_through() {
    let fixture = answered().await;
    let (path, before) = fixture.transcript().await;
    let outside = fixture.directory.path().join("outside.txt");
    // As long as the transcript, so its length alone cannot tell them apart.
    let outside_content = format!("{}\n", "o".repeat(before.len() - 1));
    std::fs::write(&outside, &outside_content).unwrap();
    std::fs::remove_file(&path).unwrap();
    std::fs::hard_link(&outside, &path).unwrap();

    fixture.ask("More", text("More done.")).await;
    let (_, after) = fixture.transcript().await;
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), outside_content);
    assert!(after.starts_with(&before), "{after}");
    assert_eq!(std::fs::metadata(&path).unwrap().nlink(), 1);
    assert_eq!(ralph_loop_last_assistant_text(&path), "More done.");
    fixture.agent.shutdown().await;
    assert!(
        outside.exists(),
        "deleting the transcript leaves other files alone"
    );
}

/// A second link to the transcript would see every later append; it keeps the old content and
/// the transcript goes on as a new file.
#[tokio::test]
async fn a_second_link_to_the_transcript_gets_nothing_more() {
    let fixture = answered().await;
    let (path, before) = fixture.transcript().await;
    let copy = fixture.directory.path().join("linked.jsonl");
    std::fs::hard_link(&path, &copy).unwrap();

    fixture.ask("More", text("More done.")).await;
    let (_, after) = fixture.transcript().await;
    assert_eq!(std::fs::read_to_string(&copy).unwrap(), before);
    assert!(after.starts_with(&before) && after.len() > before.len());
    assert_eq!(std::fs::metadata(&path).unwrap().nlink(), 1);
    fixture.agent.shutdown().await;
}

/// A rewrite that keeps the file's length is still caught: the next request writes the
/// transcript again from the stored Lines.
#[tokio::test]
async fn a_same_length_rewrite_is_written_again() {
    let fixture = answered().await;
    let (path, before) = fixture.transcript().await;
    let tampered = before.replace(ANSWER, &"x".repeat(ANSWER.len()));
    assert_eq!(tampered.len(), before.len());
    // File times may come from a coarse clock (Linux ticks); a rewrite is noticed once the
    // clock moved on since the last write.
    std::thread::sleep(std::time::Duration::from_millis(50));
    std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .write_all(tampered.as_bytes())
        .unwrap();

    let (_, after) = fixture.transcript().await;
    assert_eq!(after, before);
    fixture.agent.shutdown().await;
}

#[tokio::test]
async fn the_transcript_is_written_only_when_asked_for_and_reads_like_claude_codes() {
    let fixture = answered().await;
    let expected = expected_path(&fixture);
    assert!(!expected.exists(), "nothing is written before a hook asks");

    let (path, content) = fixture.transcript().await;
    assert_eq!(path, expected);
    let family_temp = {
        let chats = fixture.agent.inner.chats.lock().await;
        chats[&fixture.chat]
            .tools
            .family_temp_directory()
            .unwrap()
            .to_path_buf()
    };
    assert!(
        !path.starts_with(&family_temp),
        "tools write the family temp directory; the transcript lives in the Transcript store"
    );
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(ralph_loop_last_assistant_text(&path), ANSWER);
    let lines: Vec<serde_json::Value> = content
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let blocks: Vec<(String, serde_json::Value)> = lines
        .iter()
        .map(|line| {
            assert_eq!(line["type"], line["message"]["role"]);
            assert_eq!(line["sessionId"], json!(fixture.chat.uuid()));
            let [block] = line["message"]["content"].as_array().unwrap().as_slice() else {
                panic!("one block per line: {line}");
            };
            (
                line["type"].as_str().unwrap().to_owned(),
                block["type"].clone(),
            )
        })
        .collect();
    assert_eq!(
        blocks,
        [
            ("user".to_owned(), json!("text")),
            ("assistant".to_owned(), json!("tool_use")),
            ("user".to_owned(), json!("tool_result")),
            ("assistant".to_owned(), json!("text")),
        ]
    );
    assert_eq!(lines[0]["message"]["content"][0]["text"], "Read the notes");
    assert!(
        lines[2]["message"]["content"][0]["content"]
            .as_str()
            .unwrap()
            .contains("remember the tests")
    );

    fixture.agent.discard_hook_transcript(fixture.chat);
    assert!(!path.exists(), "the Chat's end deletes its transcript");
    fixture.agent.shutdown().await;
}

#[tokio::test]
async fn asking_again_appends_only_what_is_new() {
    let fixture = answered().await;
    let (_, first) = fixture.transcript().await;
    assert_eq!(fixture.transcript().await.1, first);

    fixture.ask("And the tests?", text("Second answer.")).await;
    let (path, second) = fixture.transcript().await;
    assert!(second.starts_with(&first), "{second}");
    let added: Vec<&str> = second[first.len()..].lines().collect();
    assert_eq!(added.len(), 2, "{second}");
    assert!(added[0].contains("And the tests?"));
    assert_unique_lines(&second);
    assert_eq!(ralph_loop_last_assistant_text(&path), "Second answer.");
    fixture.agent.shutdown().await;
}

#[tokio::test]
async fn a_compaction_keeps_what_was_written_and_adds_its_summary() {
    let fixture = answered().await;
    let (_, before) = fixture.transcript().await;
    fixture.model.enqueue(text("The notes were read."));
    fixture.agent.compact(fixture.chat).await.unwrap();
    fixture.ask("Continue", text("After compaction.")).await;

    let (path, after) = fixture.transcript().await;
    assert!(after.starts_with(&before), "{after}");
    assert_unique_lines(&after);
    assert_eq!(after.matches("remember the tests").count(), 1, "{after}");
    let added = &after[before.len()..];
    assert!(added.contains("The notes were read."), "{added}");
    assert!(added.contains("Continue"), "{added}");
    assert_eq!(ralph_loop_last_assistant_text(&path), "After compaction.");
    fixture.agent.shutdown().await;
}

#[tokio::test]
async fn a_reload_writes_the_whole_transcript_again() {
    let fixture = answered().await;
    let (path, before) = fixture.transcript().await;
    fixture.agent.shutdown().await;
    assert!(!path.exists(), "the Worker's exit deletes the transcript");

    let restored = agent(
        Arc::new(JsonlTranscriptStore::open(fixture.store_root()).unwrap()),
        Arc::new(ScriptedModel::new(Vec::new())),
    );
    restored.resume_chat(fixture.chat).await.unwrap();
    // A file an earlier process left behind is not taken for this one's.
    std::fs::write(&path, "{\"left\":\"behind\"}\n").unwrap();
    let reloaded = restored
        .hook_transcript_path(fixture.chat)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reloaded, path);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    assert_eq!(ralph_loop_last_assistant_text(&path), ANSWER);
    restored.shutdown().await;
    drop(fixture.store);
}

#[tokio::test]
async fn a_subagent_has_its_own_transcript() {
    let directory = tempfile::tempdir().unwrap();
    let workdir = directory.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    let store = Arc::new(JsonlTranscriptStore::open(directory.path().join("state")).unwrap());
    let model = Arc::new(RoutedModel::new(|request| {
        if !state(request)["delegation"].is_null() {
            return text("child done");
        }
        if !has_tool_result(request, "child") {
            return call(
                "child",
                "Subagent",
                json!({"name": "worker", "task": "Look around"}),
            );
        }
        text("root done")
    }));
    let agent = agent(store, model);
    let chat = agent.create_chat(workdir).await.unwrap();
    agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    agent.submit(chat, "Delegate".into()).await.unwrap();
    agent.quiesce(chat).await;
    let child = agent.subagents(chat).await.unwrap()[0].child_chat_id;
    agent.quiesce(child).await;
    agent.quiesce(chat).await;

    let root_path = agent.hook_transcript_path(chat).await.unwrap().unwrap();
    let child_path = agent.hook_transcript_path(child).await.unwrap().unwrap();
    assert_ne!(child_path, root_path);
    assert_eq!(
        child_path.parent().unwrap().file_name().unwrap(),
        child.uuid().to_string().as_str()
    );
    assert_eq!(ralph_loop_last_assistant_text(&root_path), "root done");
    assert_eq!(ralph_loop_last_assistant_text(&child_path), "child done");
    assert!(
        std::fs::read_to_string(&child_path)
            .unwrap()
            .contains("Look around")
    );
    agent.shutdown().await;
    assert!(!root_path.exists() && !child_path.exists());
}

#[tokio::test]
async fn a_chat_without_a_local_transcript_has_none() {
    let agent = Agent::with_seams(
        crate::HarnessSeams {
            search: None,
            transcripts: None,
            environment: None,
            models: ChosenModels::new(Arc::new(ScriptedModel::new(Vec::new()))),
        },
        crate::HarnessOptions {
            tools: crate::ToolPolicy::web_fetch_only(),
            waits: crate::WaitHosting::Durable,
            ..crate::local_options(
                config(),
                None,
                AgentHostOptions {
                    prompt_policy: PromptPolicy::DenyOnPrompt,
                    ..AgentHostOptions::product_defaults()
                },
            )
        },
    )
    .unwrap();
    let chat = agent.create_chat_without_folder().await.unwrap();
    assert_eq!(agent.hook_transcript_path(chat).await.unwrap(), None);
    agent.shutdown().await;
}

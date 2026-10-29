//! The file tools of a Chat reach its Folder and Additional Directories only through its
//! Execution Environment Port: here a memory environment whose files exist nowhere on disk.

use std::{path::PathBuf, sync::Arc};

use serde_json::{Value, json};
use tickloop_harness_model::ModelRequest;
use tickloop_harness_protocol::{ChatId, Content, EventKind, ExecutionMode, ToolResult};
use tickloop_harness_runtime::{
    Agent, HarnessOptions, HarnessSeams,
    environment::{FileAccess, memory::MemoryEnvironment},
};
use tickloop_harness_test_support::{ScriptedModel, call, config, harness::ChosenModels, text};

const FOLDER: &str = "folder";
const NOTES: &str = "notes";

struct Directory {
    _temporary: tempfile::TempDir,
    path: PathBuf,
}

impl Directory {
    fn new() -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().canonicalize().unwrap();
        Self {
            _temporary: temporary,
            path,
        }
    }

    fn display(&self) -> String {
        self.path.display().to_string()
    }

    fn file(&self, name: &str) -> String {
        self.path.join(name).display().to_string()
    }
}

/// A Chat on a memory environment with a writable Folder and an Additional Directory of
/// `notes_access`; the local directories stay empty.
struct Fixture {
    agent: Agent,
    model: Arc<ScriptedModel>,
    environment: MemoryEnvironment,
    folder: Directory,
    notes: Directory,
    chat: ChatId,
}

/// What one turn did: each tool call's result and how many approvals it asked for.
struct Turn {
    results: Vec<(String, ToolResult)>,
    permissions: usize,
}

impl Turn {
    fn result(&self, call_id: &str) -> &ToolResult {
        let mut results = self.results.iter().filter(|(id, _)| id == call_id);
        let (_, result) = results.next().expect("the call completed");
        assert!(results.next().is_none(), "{call_id} completed twice");
        result
    }
}

impl Fixture {
    async fn new(notes_access: FileAccess) -> Self {
        let folder = Directory::new();
        let notes = Directory::new();
        let environment = MemoryEnvironment::new();
        environment.define_root(FOLDER, &folder.display(), FileAccess::ReadWrite);
        environment.define_root(NOTES, &notes.display(), notes_access);
        let model = Arc::new(ScriptedModel::new(Vec::new()));
        let agent = Agent::with_seams(
            HarnessSeams {
                search: None,
                transcripts: None,
                environment: Some(Arc::new(environment.clone())),
                models: ChosenModels::new(model.clone()),
            },
            HarnessOptions::local(config()),
        )
        .unwrap();
        let chat = agent.create_chat(folder.path.clone()).await.unwrap();
        agent
            .set_directories(chat, folder.path.clone(), vec![notes.path.clone()])
            .await
            .unwrap();
        Self {
            agent,
            model,
            environment,
            folder,
            notes,
            chat,
        }
    }

    /// A turn in which the model makes `calls` one after another and then answers; every
    /// approval asked for is given.
    async fn turn(&self, calls: &[(&str, &str, Value)]) -> Turn {
        for (id, name, input) in calls {
            self.model.enqueue(call(id, name, input.clone()));
        }
        self.model.enqueue(text("done"));
        let mut events = self.agent.subscribe();
        self.agent
            .submit(self.chat, "Work on the files.".into())
            .await
            .unwrap();
        let mut turn = Turn {
            results: Vec::new(),
            permissions: 0,
        };
        loop {
            let event = tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
                .await
                .expect("event timed out")
                .unwrap();
            match event.kind {
                EventKind::PermissionRequested { request } => {
                    turn.permissions += 1;
                    self.agent
                        .answer_permission(self.chat, request.id, true)
                        .await
                        .unwrap();
                }
                EventKind::ToolCompleted {
                    call_id, result, ..
                } => turn.results.push((call_id, result)),
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
        turn
    }

    fn last_request(&self) -> ModelRequest {
        self.model.requests.lock().unwrap().last().unwrap().clone()
    }

    fn file(&self, root: &str, path: &str) -> Option<String> {
        self.environment
            .file(Some(root), path)
            .map(|content| String::from_utf8(content).unwrap())
    }
}

fn tool_results(request: &ModelRequest, call_id: &str) -> usize {
    request
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter(|content| matches!(content, Content::ToolResult { tool_use_id, .. } if tool_use_id == call_id))
        .count()
}

#[tokio::test]
async fn an_additional_directory_is_read_by_its_display_path_and_relative_to_the_folder() {
    let fixture = Fixture::new(FileAccess::Read).await;
    fixture
        .environment
        .put_file(Some(NOTES), "plan.md", "first step\nsecond step\n");
    let relative = format!(
        "../{}/plan.md",
        fixture.notes.path.file_name().unwrap().to_str().unwrap()
    );
    let turn = fixture
        .turn(&[
            (
                "absolute",
                "Read",
                json!({"file_path": fixture.notes.file("plan.md")}),
            ),
            ("relative", "Read", json!({"file_path": relative})),
        ])
        .await;
    for call_id in ["absolute", "relative"] {
        let result = turn.result(call_id);
        assert!(!result.is_error, "{call_id}: {}", result.content);
        assert_eq!(
            result.content, "     1\u{2192}first step\n     2\u{2192}second step\n     3\u{2192}",
            "{call_id}"
        );
    }
    assert_eq!(turn.permissions, 0);
    assert!(
        !fixture.notes.path.join("plan.md").exists(),
        "the file exists only in the environment"
    );
}

#[tokio::test]
async fn a_path_that_climbs_out_of_the_folder_is_refused() {
    let fixture = Fixture::new(FileAccess::Read).await;
    fixture
        .environment
        .put_file(None, "/etc/hosts", "127.0.0.1 localhost\n");
    let depth = fixture.folder.path.components().count() - 1;
    let climbing = format!("{}etc/hosts", "../".repeat(depth));
    let turn = fixture
        .turn(&[
            ("climb", "Read", json!({"file_path": climbing})),
            ("absolute", "Read", json!({"file_path": "/etc/hosts"})),
        ])
        .await;
    for call_id in ["climb", "absolute"] {
        let result = turn.result(call_id);
        assert!(result.is_error, "{call_id}: {}", result.content);
        assert_eq!(
            result.content, "/etc/hosts is outside the directories this Chat can read.",
            "{call_id}"
        );
    }
    assert_eq!(turn.permissions, 0);
}

#[tokio::test]
async fn a_write_into_a_read_only_directory_is_refused_before_approval() {
    let fixture = Fixture::new(FileAccess::Read).await;
    fixture
        .agent
        .set_execution_mode(fixture.chat, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    let target = fixture.notes.file("summary.md");
    let turn = fixture
        .turn(&[(
            "write",
            "Write",
            json!({"file_path": target, "content": "summary"}),
        )])
        .await;
    let result = turn.result("write");
    assert!(result.is_error);
    assert_eq!(
        result.content,
        format!("{target} is in a directory this Chat can only read, so Write cannot change it.")
    );
    assert_eq!(turn.permissions, 0, "no approval was asked for");
    assert_eq!(fixture.file(NOTES, "summary.md"), None);
}

#[tokio::test]
async fn a_write_whose_answer_is_lost_is_reported_once_and_never_sent_again() {
    let fixture = Fixture::new(FileAccess::Read).await;
    fixture.environment.lose_next_reply();
    let turn = fixture
        .turn(&[(
            "write",
            "Write",
            json!({"file_path": "report.md", "content": "final figures\n"}),
        )])
        .await;
    let result = turn.result("write");
    assert!(result.is_error);
    assert!(
        result.content.starts_with(&format!(
            "The change to {} was sent, but its answer was lost",
            fixture.folder.file("report.md")
        )),
        "{}",
        result.content
    );
    assert!(
        result
            .content
            .ends_with("it was not sent again. Read the file before deciding what to do next."),
        "{}",
        result.content
    );
    assert_eq!(tool_results(&fixture.last_request(), "write"), 1);
    assert_eq!(
        fixture.file(FOLDER, "report.md").as_deref(),
        Some("final figures\n"),
        "the one dispatched write took effect"
    );
    assert!(!fixture.folder.path.join("report.md").exists());
}

#[tokio::test]
async fn an_edit_after_an_outside_change_must_read_the_file_again() {
    let fixture = Fixture::new(FileAccess::Read).await;
    fixture
        .environment
        .put_file(Some(FOLDER), "report.md", "Delivery Monday\n");
    let turn = fixture
        .turn(&[("read", "Read", json!({"file_path": "report.md"}))])
        .await;
    assert!(!turn.result("read").is_error);
    fixture
        .environment
        .put_file(Some(FOLDER), "report.md", "Delivery Tuesday\n");
    let turn = fixture
        .turn(&[(
            "edit",
            "Edit",
            json!({"file_path": "report.md", "old_string": "Tuesday", "new_string": "Friday"}),
        )])
        .await;
    let result = turn.result("edit");
    assert!(result.is_error);
    assert_eq!(
        result.content,
        "File has been modified since read, either by the user or by a linter. Read it again before attempting to write it."
    );
    assert_eq!(
        fixture.file(FOLDER, "report.md").as_deref(),
        Some("Delivery Tuesday\n")
    );

    let turn = fixture
        .turn(&[
            ("reread", "Read", json!({"file_path": "report.md"})),
            (
                "edit",
                "Edit",
                json!({"file_path": "report.md", "old_string": "Tuesday", "new_string": "Friday"}),
            ),
        ])
        .await;
    assert_eq!(
        turn.result("edit").content,
        format!(
            "The file {} has been updated successfully.",
            fixture.folder.file("report.md")
        )
    );
    assert_eq!(
        fixture.file(FOLDER, "report.md").as_deref(),
        Some("Delivery Friday\n")
    );
}

#[tokio::test]
async fn auto_reads_and_edits_the_workspace_directly_and_still_asks_for_sensitive_files() {
    let fixture = Fixture::new(FileAccess::ReadWrite).await;
    fixture
        .environment
        .put_file(Some(NOTES), "todo.md", "- ship\n");
    let turn = fixture
        .turn(&[
            ("read", "Read", json!({"file_path": fixture.notes.file("todo.md")})),
            (
                "edit",
                "Edit",
                json!({"file_path": fixture.notes.file("todo.md"), "old_string": "ship", "new_string": "ship it"}),
            ),
            ("write", "Write", json!({"file_path": "src/lib.rs", "content": "fn main() {}\n"})),
            (
                "search",
                "Grep",
                json!({"pattern": "ship", "path": fixture.notes.display()}),
            ),
        ])
        .await;
    for call_id in ["read", "edit", "write"] {
        assert!(
            !turn.result(call_id).is_error,
            "{call_id}: {}",
            turn.result(call_id).content
        );
    }
    assert_eq!(
        turn.result("search").content,
        format!("Found 1 file\n{}", fixture.notes.file("todo.md"))
    );
    assert_eq!(turn.permissions, 0);
    assert_eq!(
        fixture.file(NOTES, "todo.md").as_deref(),
        Some("- ship it\n")
    );
    assert_eq!(
        fixture.file(FOLDER, "src/lib.rs").as_deref(),
        Some("fn main() {}\n")
    );

    fixture
        .agent
        .set_execution_mode(fixture.chat, ExecutionMode::AcceptEdits)
        .await
        .unwrap();
    let turn = fixture
        .turn(&[(
            "hook",
            "Write",
            json!({"file_path": ".git/hooks/pre-commit", "content": "exit 0\n"}),
        )])
        .await;
    assert_eq!(turn.permissions, 1, "a sensitive file still needs approval");
    assert!(!turn.result("hook").is_error);
}

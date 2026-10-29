//! File history, Rewind and `@` mentions reach a Chat's Folder and Additional Directories only
//! through its Execution Environment Port: here a memory environment whose display paths name
//! directories that also exist, with other files, on this machine.

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use serde_json::{Value, json};
use tickloop_harness_protocol::{
    ChatId, CodeRewind, Content, EventKind, InputContext, Receipt, RewindMode, TextContext,
};
use tickloop_harness_runtime::{
    Agent, HarnessOptions, HarnessSeams,
    environment::{
        ExecutionAdmission, ExecutionAuthority, ExecutionEnvironment, ExecutionError,
        ExecutionInspection, ExecutionIntent, ExecutionObservation, ExecutionOutcome,
        ExecutionPort, ExecutionQuery, ExecutionScope, FileAccess, ObserveRequest,
        PreparedExecution, memory::MemoryEnvironment,
    },
};
use tickloop_harness_test_support::{ScriptedModel, call, config, harness::ChosenModels, text};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const FOLDER: &str = "folder";
const NOTES: &str = "notes";
const OUTSIDE_CHANGE: &str = "changed from outside\n";

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

    fn file(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

/// Someone changes the Folder's `report.md` from outside right before the next `WriteFile` is
/// prepared once armed: between a restore reading the file and writing it.
struct ChangedBeforeWrite {
    memory: MemoryEnvironment,
    armed: Arc<AtomicBool>,
}

impl ExecutionEnvironment for ChangedBeforeWrite {
    fn bind(
        &self,
        scope: ExecutionScope,
        authority: ExecutionAuthority,
    ) -> Result<Arc<dyn ExecutionPort>, ExecutionError> {
        Ok(Arc::new(ChangingPort {
            inner: self.memory.bind(scope, authority)?,
            memory: self.memory.clone(),
            armed: self.armed.clone(),
        }))
    }
}

struct ChangingPort {
    inner: Arc<dyn ExecutionPort>,
    memory: MemoryEnvironment,
    armed: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl ExecutionPort for ChangingPort {
    async fn inspect(&self, query: ExecutionQuery) -> Result<ExecutionInspection, ExecutionError> {
        self.inner.inspect(query).await
    }

    async fn prepare(&self, intent: ExecutionIntent) -> Result<PreparedExecution, ExecutionError> {
        if matches!(intent, ExecutionIntent::WriteFile { .. })
            && self.armed.swap(false, Ordering::SeqCst)
        {
            self.memory
                .put_file(Some(FOLDER), "report.md", OUTSIDE_CHANGE);
        }
        self.inner.prepare(intent).await
    }

    async fn execute(
        &self,
        prepared: PreparedExecution,
        admission: ExecutionAdmission,
        cancel: CancellationToken,
    ) -> ExecutionOutcome {
        self.inner.execute(prepared, admission, cancel).await
    }

    async fn observe(
        &self,
        request: ObserveRequest,
    ) -> Result<ExecutionObservation, ExecutionError> {
        self.inner.observe(request).await
    }
}

/// A Chat on a memory environment with a writable Folder and an Additional Directory of
/// `notes_access`; the local directories hold files of their own.
struct Fixture {
    agent: Agent,
    model: Arc<ScriptedModel>,
    memory: MemoryEnvironment,
    folder: Directory,
    notes: Directory,
    chat: ChatId,
}

impl Fixture {
    async fn new(notes_access: FileAccess) -> Self {
        let memory = MemoryEnvironment::new();
        Self::on(memory.clone(), Arc::new(memory), notes_access).await
    }

    async fn on(
        memory: MemoryEnvironment,
        environment: Arc<dyn ExecutionEnvironment>,
        notes_access: FileAccess,
    ) -> Self {
        let folder = Directory::new();
        let notes = Directory::new();
        memory.define_root(FOLDER, &folder.display(), FileAccess::ReadWrite);
        memory.define_root(NOTES, &notes.display(), notes_access);
        let model = Arc::new(ScriptedModel::new(Vec::new()));
        let agent = Agent::with_seams(
            HarnessSeams {
                search: None,
                transcripts: None,
                environment: Some(environment),
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
            memory,
            folder,
            notes,
            chat,
        }
    }

    /// A turn in which the model makes `calls` one after another and then answers; every
    /// approval asked for is given, and every call must succeed.
    async fn turn(&self, calls: &[(&str, &str, Value)]) {
        for (id, name, input) in calls {
            self.model.enqueue(call(id, name, input.clone()));
        }
        self.model.enqueue(text("done"));
        let mut events = self.agent.subscribe();
        let receipt = self
            .agent
            .submit(self.chat, "Work on the files.".into())
            .await
            .unwrap();
        self.finish(&mut events, &receipt).await;
    }

    async fn finish(
        &self,
        events: &mut tokio::sync::broadcast::Receiver<tickloop_harness_protocol::Event>,
        receipt: &Receipt,
    ) {
        loop {
            let event = tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
                .await
                .expect("event timed out")
                .unwrap();
            if event.run_id != receipt.run_id {
                continue;
            }
            match event.kind {
                EventKind::PermissionRequested { request } => self
                    .agent
                    .answer_permission(self.chat, request.id, true)
                    .await
                    .unwrap(),
                EventKind::ToolCompleted { result, .. } => {
                    assert!(!result.is_error, "{}", result.content);
                }
                EventKind::Completed { .. } => return,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
    }

    async fn first_prompt(&self) -> Uuid {
        self.agent
            .snapshot(self.chat)
            .await
            .unwrap()
            .rewind_points()[0]
            .message_id
    }

    async fn rewind_code(&self) -> CodeRewind {
        let point = self.first_prompt().await;
        let outcome = self
            .agent
            .rewind(self.chat, point, RewindMode::Code)
            .await
            .unwrap();
        outcome.code.unwrap()
    }

    fn file(&self, root: &str, path: &str) -> Option<String> {
        self.memory
            .file(Some(root), path)
            .map(|content| String::from_utf8(content).unwrap())
    }

    /// Mentions `paths` with `@`; the run's contexts on success.
    async fn mention(&self, paths: Vec<PathBuf>) -> Result<Vec<InputContext>, String> {
        self.model.enqueue(text("noted"));
        let mut events = self.agent.subscribe();
        let receipt = self
            .agent
            .submit_with_input_contexts(self.chat, "Look at these.".into(), Vec::new(), paths)
            .await
            .map_err(|error| error.to_string())?;
        self.finish(&mut events, &receipt).await;
        let snapshot = self.agent.snapshot(self.chat).await.unwrap();
        Ok(snapshot.message_contexts.values().last().unwrap().clone())
    }
}

fn sorted(mut paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths.sort();
    paths
}

#[tokio::test]
async fn rewind_restores_the_environment_files_and_never_touches_the_same_paths_on_this_machine() {
    let fixture = Fixture::new(FileAccess::ReadWrite).await;
    fixture
        .memory
        .put_file(Some(FOLDER), "report.md", "Delivery Monday\n");
    fixture.memory.put_file(Some(NOTES), "todo.md", "- ship\n");
    let local_report = fixture.folder.file("report.md");
    let local_todo = fixture.notes.file("todo.md");
    std::fs::write(&local_report, "local report\n").unwrap();
    std::fs::write(&local_todo, "local notes\n").unwrap();
    fixture
        .turn(&[
            ("read", "Read", json!({"file_path": "report.md"})),
            (
                "edit",
                "Edit",
                json!({"file_path": "report.md", "old_string": "Monday", "new_string": "Friday"}),
            ),
            ("read notes", "Read", json!({"file_path": local_todo})),
            (
                "edit notes",
                "Edit",
                json!({"file_path": local_todo, "old_string": "ship", "new_string": "ship it"}),
            ),
            (
                "create",
                "Write",
                json!({"file_path": "summary.md", "content": "all done\n"}),
            ),
        ])
        .await;
    assert_eq!(
        fixture.file(FOLDER, "report.md").as_deref(),
        Some("Delivery Friday\n")
    );

    let changed = [
        local_report.clone(),
        local_todo.clone(),
        fixture.folder.file("summary.md"),
    ];
    let point = fixture.first_prompt().await;
    let preview = fixture
        .agent
        .rewind_file_changes(fixture.chat, point)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(sorted(preview), sorted(changed.to_vec()));
    let code = fixture.rewind_code().await;
    assert!(code.failed_files.is_empty(), "{:?}", code.failed_files);
    assert_eq!(sorted(code.changed_files), sorted(changed.to_vec()));
    assert_eq!(
        fixture.file(FOLDER, "report.md").as_deref(),
        Some("Delivery Monday\n")
    );
    assert_eq!(fixture.file(NOTES, "todo.md").as_deref(), Some("- ship\n"));
    assert_eq!(fixture.file(FOLDER, "summary.md"), None);
    assert_eq!(
        std::fs::read_to_string(&local_report).unwrap(),
        "local report\n"
    );
    assert_eq!(
        std::fs::read_to_string(&local_todo).unwrap(),
        "local notes\n"
    );
    assert!(!fixture.folder.file("summary.md").exists());
}

#[tokio::test]
async fn a_file_changed_from_outside_during_the_restore_is_reported_and_kept() {
    let memory = MemoryEnvironment::new();
    let armed = Arc::new(AtomicBool::new(false));
    let environment = Arc::new(ChangedBeforeWrite {
        memory: memory.clone(),
        armed: armed.clone(),
    });
    let fixture = Fixture::on(memory, environment, FileAccess::ReadWrite).await;
    fixture
        .memory
        .put_file(Some(FOLDER), "report.md", "Delivery Monday\n");
    fixture
        .turn(&[
            ("read", "Read", json!({"file_path": "report.md"})),
            (
                "edit",
                "Edit",
                json!({"file_path": "report.md", "old_string": "Monday", "new_string": "Friday"}),
            ),
        ])
        .await;

    armed.store(true, Ordering::SeqCst);
    let code = fixture.rewind_code().await;
    assert!(code.error.is_none());
    assert!(code.changed_files.is_empty());
    let failed = code
        .failed_files
        .iter()
        .map(|(path, _)| path.clone())
        .collect::<Vec<_>>();
    assert_eq!(failed, [fixture.folder.file("report.md")]);
    assert!(
        !armed.load(Ordering::SeqCst),
        "the restore prepared a write"
    );
    assert_eq!(
        fixture.file(FOLDER, "report.md").as_deref(),
        Some(OUTSIDE_CHANGE)
    );
}

#[tokio::test]
async fn a_mention_reads_files_and_listings_from_the_environment() {
    let fixture = Fixture::new(FileAccess::Read).await;
    fixture
        .memory
        .put_file(Some(NOTES), "plan.md", "first step\n");
    fixture.memory.put_file(Some(FOLDER), "src/main.rs", "");
    fixture.memory.put_file(Some(FOLDER), "src/lib/mod.rs", "");
    std::fs::write(fixture.notes.file("plan.md"), "local plan\n").unwrap();

    let contexts = fixture
        .mention(vec![
            fixture.notes.file("plan.md"),
            fixture.folder.file("src"),
        ])
        .await
        .unwrap();
    assert_eq!(
        contexts,
        [
            InputContext::File(TextContext {
                path: fixture.notes.file("plan.md"),
                content: "first step\n".into(),
                truncated: false,
            }),
            InputContext::Directory(TextContext {
                path: PathBuf::from("src"),
                content: "lib/\nmain.rs".into(),
                truncated: false,
            }),
        ]
    );
    let request = fixture
        .model
        .requests
        .lock()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    let sent = request
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .any(|content| matches!(content, Content::Text { text } if text.contains("first step")));
    assert!(sent, "the mentioned file reached the model");
}

#[tokio::test]
async fn a_mention_outside_the_readable_directories_is_refused_before_a_run_starts() {
    let fixture = Fixture::new(FileAccess::None).await;
    fixture
        .memory
        .put_file(Some(NOTES), "secret.md", "hidden\n");
    fixture
        .memory
        .put_file(None, "/etc/hosts", "127.0.0.1 localhost\n");

    for path in [fixture.notes.file("secret.md"), PathBuf::from("/etc/hosts")] {
        let error = fixture.mention(vec![path.clone()]).await.unwrap_err();
        assert!(
            error.contains("referenced path is outside the configured workspace"),
            "{}: {error}",
            path.display()
        );
    }
    assert!(fixture.model.requests.lock().unwrap().is_empty());
    assert!(
        fixture
            .agent
            .snapshot(fixture.chat)
            .await
            .unwrap()
            .messages
            .is_empty()
    );
}

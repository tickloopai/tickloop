//! A Folder that exists only in the Execution Environment: the memory environment reports its
//! roots without a path on this machine, so the Harness neither resolves nor trusts them here,
//! reaches their files only through the Port, and keeps no local scratchpad, plans, Skills or
//! shell for them. A Chat can also gain such a Folder and lose it again.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use serde_json::{Value, json};
use tickloop_harness_model::ModelRequest;
use tickloop_harness_policy::settings::{
    MemorySettingsStore, PermissionChanges, SettingsSnapshot, SettingsStore, SettingsStoreError,
    SettingsUpdate,
};
use tickloop_harness_protocol::{
    AgentHostOptions, ChatId, Content, EventKind, ExecutionMode, Role, ToolResult,
};
use tickloop_harness_runtime::{
    Agent, HarnessOptions, HarnessSeams,
    environment::{FileAccess, memory::MemoryEnvironment},
};
use tickloop_harness_test_support::{
    ScriptedModel, call, config, harness::ChosenModels, reminded_plan_file, text,
    transcripts::InMemoryTranscriptStore,
};
use tickloop_harness_tools::{
    BASH_TOOL_NAME, EDIT_TOOL_NAME, GLOB_TOOL_NAME, GREP_TOOL_NAME, READ_TOOL_NAME,
    TASK_STOP_TOOL_NAME, WRITE_TOOL_NAME,
};
use tickloop_harness_transcript::Line;

/// Paths no machine running these tests has.
const APP: &str = "/tickloop-environment-only/app";
const DESIGN: &str = "/tickloop-environment-only/design";
const FILE_TOOLS: [&str; 5] = [
    READ_TOOL_NAME,
    GLOB_TOOL_NAME,
    GREP_TOOL_NAME,
    WRITE_TOOL_NAME,
    EDIT_TOOL_NAME,
];
const SHELL_TOOLS: [&str; 2] = [BASH_TOOL_NAME, TASK_STOP_TOOL_NAME];

/// A Settings Store that remembers each directory it was asked to read Skills or instruction
/// files of.
#[derive(Default)]
struct RecordingSettings {
    inner: MemorySettingsStore,
    folders: Mutex<Vec<Option<PathBuf>>>,
    instruction_text: Mutex<String>,
}

impl RecordingSettings {
    fn record(&self, folder: Option<&Path>) {
        self.folders
            .lock()
            .unwrap()
            .push(folder.map(Path::to_path_buf));
    }

    fn asked_about(&self, path: &str) -> bool {
        self.folders
            .lock()
            .unwrap()
            .iter()
            .flatten()
            .any(|folder| folder.starts_with(path))
    }
}

impl SettingsStore for RecordingSettings {
    fn read(&self, project: Option<&Path>) -> SettingsSnapshot {
        self.record(project);
        self.inner.read(project)
    }

    fn write(&self, change: SettingsUpdate) -> Result<(), SettingsStoreError> {
        self.inner.write(change)
    }

    fn skills(&self, folder: Option<&Path>) -> tickloop_harness_policy::skills::SkillRegistry {
        self.record(folder);
        self.inner.skills(folder)
    }

    fn instruction_files(
        &self,
        folder: Option<&Path>,
    ) -> Vec<tickloop_harness_policy::instructions::InstructionFile> {
        self.record(folder);
        let content = self.instruction_text.lock().unwrap().clone();
        if let Some(folder) = folder.filter(|_| !content.is_empty()) {
            vec![tickloop_harness_policy::instructions::InstructionFile {
                path: folder.join("AGENTS.md"),
                scope: tickloop_harness_policy::instructions::InstructionScope::Project,
                content,
            }]
        } else {
            self.inner.instruction_files(folder)
        }
    }

    fn write_permissions(
        &self,
        changes: &PermissionChanges,
        expected: Option<(&Path, &str)>,
    ) -> Result<(), SettingsStoreError> {
        self.inner.write_permissions(changes, expected)
    }
}

struct Fixture {
    agent: Agent,
    model: Arc<ScriptedModel>,
    environment: MemoryEnvironment,
    store: Arc<InMemoryTranscriptStore>,
}

impl Fixture {
    /// An Agent that requires trusted directories, on a memory environment whose `app` and
    /// `design` roots exist nowhere on this machine.
    fn new() -> Self {
        assert!(!Path::new(APP).exists() && !Path::new(DESIGN).exists());
        let environment = MemoryEnvironment::new();
        environment.define_root("app", APP, FileAccess::ReadWrite);
        environment.define_root("design", DESIGN, FileAccess::Read);
        Self::on(environment, Arc::new(InMemoryTranscriptStore::new()), None)
    }

    fn on(
        environment: MemoryEnvironment,
        store: Arc<InMemoryTranscriptStore>,
        settings: Option<Arc<RecordingSettings>>,
    ) -> Self {
        let model = Arc::new(ScriptedModel::new(Vec::new()));
        let agent = Agent::with_seams(
            HarnessSeams {
                search: None,
                transcripts: Some(store.clone()),
                environment: Some(Arc::new(environment.clone())),
                models: ChosenModels::new(model.clone()),
            },
            HarnessOptions {
                host: AgentHostOptions::product_defaults(),
                settings: settings.map(|settings| settings as Arc<dyn SettingsStore>),
                ..HarnessOptions::local(config())
            },
        )
        .unwrap();
        Self {
            agent,
            model,
            environment,
            store,
        }
    }

    /// The same environment and Transcripts in a new Agent, as a later process.
    async fn reopen(self) -> Self {
        self.agent.sync().await.unwrap();
        self.agent.shutdown().await;
        Self::on(self.environment, self.store, None)
    }

    /// One turn in which the model makes `calls` and then answers; every approval asked for is
    /// given. Returns the turn's last model request and each call's result.
    async fn turn(
        &self,
        chat: ChatId,
        calls: &[(&str, &str, Value)],
    ) -> (ModelRequest, Vec<(String, ToolResult)>) {
        let replies = calls
            .iter()
            .map(|(id, name, input)| call(id, name, input.clone()))
            .collect();
        self.turn_with_replies(chat, replies).await
    }

    /// [`Self::turn`] in which the model gives `replies` in order, each with any number of calls.
    async fn turn_with_replies(
        &self,
        chat: ChatId,
        replies: Vec<Vec<Content>>,
    ) -> (ModelRequest, Vec<(String, ToolResult)>) {
        for reply in replies {
            self.model.enqueue(reply);
        }
        self.model.enqueue(text("done"));
        let mut events = self.agent.subscribe();
        let receipt = self.agent.submit(chat, "Go on.".into()).await.unwrap();
        let mut results = Vec::new();
        loop {
            let event = tokio::time::timeout(std::time::Duration::from_secs(10), events.recv())
                .await
                .expect("event timed out")
                .unwrap();
            match event.kind {
                EventKind::PermissionRequested { request } if event.chat_id == chat => {
                    self.agent
                        .answer_permission(chat, request.id, true)
                        .await
                        .unwrap();
                }
                EventKind::ToolCompleted {
                    call_id, result, ..
                } if event.chat_id == chat => results.push((call_id, result)),
                EventKind::Completed { .. } if event.run_id == receipt.run_id => break,
                EventKind::Failed(error) if event.run_id == receipt.run_id => panic!("{error}"),
                _ => {}
            }
        }
        let request = self.model.requests.lock().unwrap().last().unwrap().clone();
        (request, results)
    }
}

fn tool_names(request: &ModelRequest) -> Vec<&str> {
    request
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect()
}

fn system_text(request: &ModelRequest) -> String {
    request
        .messages
        .iter()
        .filter(|message| message.role == Role::System)
        .flat_map(|message| &message.content)
        .filter_map(|content| match content {
            Content::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn result<'a>(results: &'a [(String, ToolResult)], call_id: &str) -> &'a ToolResult {
    &results
        .iter()
        .find(|(id, _)| id == call_id)
        .unwrap_or_else(|| panic!("{call_id} did not complete"))
        .1
}

/// The Folder each chain entry of `chat` names, in order, without repeats.
fn recorded_folders(store: &InMemoryTranscriptStore, chat: ChatId) -> Vec<Option<PathBuf>> {
    let mut folders: Vec<Option<PathBuf>> = Vec::new();
    for entry in store.lines(chat).iter().filter_map(Line::chain_entry) {
        if folders.last() != Some(&entry.cwd) {
            folders.push(entry.cwd.clone());
        }
    }
    folders
}

#[tokio::test]
async fn a_folder_only_in_the_environment_is_created_recovered_and_reached_through_the_port() {
    let fixture = Fixture::new();
    fixture
        .environment
        .put_file(Some("app"), "notes.md", "the plan\n");
    let chat = fixture.agent.create_chat(PathBuf::from(APP)).await.unwrap();
    fixture
        .agent
        .set_directories(chat, PathBuf::from(APP), vec![PathBuf::from(DESIGN)])
        .await
        .unwrap();
    let snapshot = fixture.agent.snapshot(chat).await.unwrap();
    assert_eq!(snapshot.folder, Some(PathBuf::from(APP)));
    assert_eq!(snapshot.additional_directories, vec![PathBuf::from(DESIGN)]);

    let (request, results) = fixture
        .turn(
            chat,
            &[
                (
                    "read",
                    "Read",
                    json!({"file_path": format!("{APP}/notes.md")}),
                ),
                (
                    "write",
                    "Write",
                    json!({"file_path": format!("{APP}/answer.md"), "content": "yes\n"}),
                ),
                (
                    "refused",
                    "Write",
                    json!({"file_path": format!("{DESIGN}/answer.md"), "content": "no\n"}),
                ),
            ],
        )
        .await;
    for tool in FILE_TOOLS {
        assert!(tool_names(&request).contains(&tool), "{tool} missing");
    }
    let read = result(&results, "read");
    assert!(
        !read.is_error && read.content.contains("the plan"),
        "{read:?}"
    );
    assert!(!result(&results, "write").is_error);
    assert_eq!(
        fixture
            .environment
            .file(Some("app"), "answer.md")
            .as_deref(),
        Some(&b"yes\n"[..])
    );
    assert!(result(&results, "refused").is_error);
    assert_eq!(fixture.environment.file(Some("design"), "answer.md"), None);
    assert!(!Path::new(APP).exists() && !Path::new(DESIGN).exists());

    let fixture = fixture.reopen().await;
    let snapshot = fixture.agent.resume_chat(chat).await.unwrap();
    assert_eq!(snapshot.folder, Some(PathBuf::from(APP)));
    let (_, results) = fixture
        .turn(
            chat,
            &[(
                "again",
                "Read",
                json!({"file_path": format!("{APP}/answer.md")}),
            )],
        )
        .await;
    let again = result(&results, "again");
    assert!(
        !again.is_error && again.content.contains("yes"),
        "{again:?}"
    );
    fixture.agent.shutdown().await;
}

#[tokio::test]
async fn a_chat_without_a_folder_gains_one_loses_it_and_recovers_each_way() {
    let fixture = Fixture::new();
    fixture
        .environment
        .put_file(Some("app"), "notes.md", "the plan\n");
    let chat = fixture.agent.create_chat_without_folder().await.unwrap();
    let (folderless, _) = fixture.turn(chat, &[]).await;
    let folderless_prompt = system_text(&folderless);
    for tool in FILE_TOOLS.into_iter().chain(SHELL_TOOLS) {
        assert!(!tool_names(&folderless).contains(&tool), "{tool} listed");
    }

    fixture
        .agent
        .set_working_directories(chat, Some(PathBuf::from(APP)), Vec::new())
        .await
        .unwrap();
    let (with_folder, results) = fixture
        .turn(
            chat,
            &[(
                "read",
                "Read",
                json!({"file_path": format!("{APP}/notes.md")}),
            )],
        )
        .await;
    assert!(tool_names(&with_folder).contains(&READ_TOOL_NAME));
    assert!(result(&results, "read").content.contains("the plan"));
    let prompt = system_text(&with_folder);
    assert!(
        prompt.contains(&format!(" - Primary working directory: {APP}\n")),
        "{prompt}"
    );

    let fixture = fixture.reopen().await;
    let snapshot = fixture.agent.resume_chat(chat).await.unwrap();
    assert_eq!(snapshot.folder, Some(PathBuf::from(APP)));
    fixture
        .agent
        .set_working_directories(chat, None, Vec::new())
        .await
        .unwrap();
    let (without_folder, _) = fixture.turn(chat, &[]).await;
    for tool in FILE_TOOLS.into_iter().chain(SHELL_TOOLS) {
        assert!(
            !tool_names(&without_folder).contains(&tool),
            "{tool} listed"
        );
    }
    assert_eq!(system_text(&without_folder), folderless_prompt);
    assert!(
        fixture
            .agent
            .set_working_directories(chat, None, vec![PathBuf::from(DESIGN)])
            .await
            .is_err()
    );

    let fixture = fixture.reopen().await;
    let snapshot = fixture.agent.resume_chat(chat).await.unwrap();
    assert_eq!(snapshot.folder, None);
    assert_eq!(
        recorded_folders(&fixture.store, chat),
        vec![None, Some(PathBuf::from(APP)), None]
    );
    fixture.agent.shutdown().await;
}

#[tokio::test]
async fn an_environment_without_shells_offers_no_shell_and_runs_none_here() {
    let fixture = Fixture::new();
    let with_shells = fixture.agent.create_chat(PathBuf::from(APP)).await.unwrap();
    let (request, _) = fixture.turn(with_shells, &[]).await;
    for tool in SHELL_TOOLS {
        assert!(tool_names(&request).contains(&tool), "{tool} missing");
    }

    fixture.environment.set_shells(false);
    let chat = fixture.agent.create_chat(PathBuf::from(APP)).await.unwrap();
    let marker = tempfile::tempdir().unwrap();
    let marker = marker.path().join("ran-here");
    let (request, _) = fixture.turn(chat, &[]).await;
    for tool in SHELL_TOOLS {
        assert!(!tool_names(&request).contains(&tool), "{tool} listed");
    }
    assert!(
        fixture
            .agent
            .submit_user_shell(chat, format!("touch {}", marker.display()), false)
            .await
            .is_err()
    );
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(!marker.exists());
    fixture.agent.shutdown().await;
}

#[tokio::test]
async fn a_folder_in_the_environment_fetches_like_a_folderless_chat_and_keeps_nothing_here() {
    let settings = Arc::new(RecordingSettings::default());
    let environment = MemoryEnvironment::new();
    environment.define_root("app", APP, FileAccess::ReadWrite);
    let fixture = Fixture::on(
        environment,
        Arc::new(InMemoryTranscriptStore::new()),
        Some(settings.clone()),
    );
    let chat = fixture.agent.create_chat(PathBuf::from(APP)).await.unwrap();
    let folderless = fixture.agent.create_chat_without_folder().await.unwrap();
    let fetch = || {
        [(
            "fetch",
            "WebFetch",
            json!({"url": "http://localhost:9/status", "prompt": "summarize"}),
        )]
    };
    let (request, results) = fixture.turn(chat, &fetch()).await;
    let (_, folderless_results) = fixture.turn(folderless, &fetch()).await;
    let fetched = result(&results, "fetch");
    assert!(
        fetched.is_error && fetched.content.contains("which is not a public address"),
        "{fetched:?}"
    );
    assert_eq!(
        fetched.content,
        result(&folderless_results, "fetch").content
    );

    let prompt = system_text(&request);
    for absent in [
        "# Scratchpad Directory",
        "\"scratchpad\":\"",
        "# Doing tasks",
    ] {
        assert!(!prompt.contains(absent), "{absent} in {prompt}");
    }
    assert!(
        prompt.contains(&format!(" - Primary working directory: {APP}\n")),
        "{prompt}"
    );
    let scratch = tempfile::tempdir().unwrap();
    let (_, results) = fixture
        .turn(
            chat,
            &[(
                "local",
                "Write",
                json!({"file_path": scratch.path().join("note.md"), "content": "x"}),
            )],
        )
        .await;
    assert!(result(&results, "local").is_error);
    assert!(!scratch.path().join("note.md").exists());

    fixture
        .agent
        .set_execution_mode(chat, ExecutionMode::Plan)
        .await
        .unwrap();
    let (planning, _) = fixture.turn(chat, &[]).await;
    assert_eq!(reminded_plan_file(&planning), None);
    assert_eq!(fixture.agent.plan_file(chat).await.unwrap(), None);
    assert!(
        !settings.asked_about(APP),
        "{:?}",
        settings.folders.lock().unwrap()
    );
    fixture.agent.shutdown().await;
}

#[tokio::test]
async fn detach_is_durable_without_another_turn() {
    let fixture = Fixture::new();
    let chat = fixture.agent.create_chat(PathBuf::from(APP)).await.unwrap();
    fixture.turn(chat, &[]).await;
    fixture.agent.sync().await.unwrap();
    fixture
        .agent
        .set_working_directories(chat, None, vec![])
        .await
        .unwrap();
    let fixture = fixture.reopen().await;
    let restored = fixture.agent.resume_chat(chat).await.unwrap();
    fixture.agent.shutdown().await;
    assert_eq!(
        restored.folder, None,
        "detached Folder must remain detached after restart"
    );
}
#[tokio::test]
async fn remote_shell_uses_environment_cwd() {
    let fixture = Fixture::new();
    fixture.environment.define_shell(
        "pwd",
        std::time::Duration::ZERO,
        format!("{APP}\n").into_bytes(),
        0,
    );
    let chat = fixture.agent.create_chat(PathBuf::from(APP)).await.unwrap();
    let (_, results) = fixture
        .turn(chat, &[("remote-shell", "Bash", json!({"command": "pwd"}))])
        .await;
    fixture.agent.shutdown().await;
    assert!(
        !result(&results, "remote-shell").is_error,
        "{:?}",
        result(&results, "remote-shell")
    );
}
#[tokio::test]
async fn remote_transcript_does_not_read_local_git() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join(".git")).unwrap();
    std::fs::write(
        root.path().join(".git/HEAD"),
        "ref: refs/heads/HARNESS_LOCAL_ONLY\n",
    )
    .unwrap();
    let environment = MemoryEnvironment::new();
    environment.define_root("app", root.path().to_str().unwrap(), FileAccess::ReadWrite);
    let fixture = Fixture::on(environment, Arc::new(InMemoryTranscriptStore::new()), None);
    let chat = fixture
        .agent
        .create_chat(root.path().to_path_buf())
        .await
        .unwrap();
    fixture.turn(chat, &[]).await;
    fixture.agent.sync().await.unwrap();
    let branches: Vec<_> = fixture
        .store
        .lines(chat)
        .iter()
        .filter_map(Line::chain_entry)
        .filter_map(|entry| entry.git_branch.clone())
        .collect();
    fixture.agent.shutdown().await;
    assert!(
        branches.is_empty(),
        "remote Transcript contains local git branches: {branches:?}"
    );
}

#[tokio::test]
async fn reattach_keeps_reporting_environment_edits_on_rewind() {
    check_reattached_history(false).await;
}

#[tokio::test]
async fn restarting_while_detached_keeps_reporting_environment_edits_on_rewind() {
    check_reattached_history(true).await;
}

async fn check_reattached_history(restart_detached: bool) {
    let fixture = Fixture::new();
    fixture
        .environment
        .put_file(Some("app"), "notes.md", "original\n");
    let chat = fixture.agent.create_chat(PathBuf::from(APP)).await.unwrap();
    let (_, first) = fixture.turn(chat, &[
        ("read1", "Read", json!({"file_path": "notes.md"})),
        ("edit1", "Edit", json!({"file_path": "notes.md", "old_string": "original", "new_string": "intermediate"}))
    ]).await;
    assert!(!result(&first, "edit1").is_error);
    let snapshot = fixture.agent.snapshot(chat).await.unwrap();
    let point = *snapshot.message_ids.values().next().unwrap();
    fixture
        .agent
        .set_working_directories(chat, None, vec![])
        .await
        .unwrap();
    let fixture = if restart_detached {
        let fixture = fixture.reopen().await;
        assert!(
            fixture
                .agent
                .resume_chat(chat)
                .await
                .unwrap()
                .folder
                .is_none()
        );
        fixture
    } else {
        fixture
    };
    fixture
        .agent
        .set_working_directories(chat, Some(PathBuf::from(APP)), vec![])
        .await
        .unwrap();
    let (_, second) = fixture.turn(chat, &[
        ("read2", "Read", json!({"file_path": "notes.md"})),
        ("edit2", "Edit", json!({"file_path": "notes.md", "old_string": "intermediate", "new_string": "final"}))
    ]).await;
    assert!(!result(&second, "edit2").is_error);
    let fixture = fixture.reopen().await;
    fixture.agent.resume_chat(chat).await.unwrap();
    let code = fixture
        .agent
        .rewind(chat, point, tickloop_harness_protocol::RewindMode::Code)
        .await
        .unwrap()
        .code
        .unwrap();
    let actual = fixture.environment.file(Some("app"), "notes.md");
    fixture.agent.shutdown().await;
    assert!(code.changed_files.is_empty(), "{code:?}");
    assert_only_in_environment(&code.failed_files, &[format!("{APP}/notes.md")]);
    assert_eq!(
        actual.as_deref(),
        Some(&b"final\n"[..]),
        "a file only in the environment has no backup to restore"
    );
}

/// Asserts that rewind reported exactly `paths` as not restored because file history keeps no
/// copy of files only in the environment.
fn assert_only_in_environment(failed: &[(PathBuf, String)], paths: &[String]) {
    let mut reported = failed
        .iter()
        .map(|(path, reason)| {
            assert!(
                reason.contains("only in the execution environment"),
                "{}: {reason}",
                path.display()
            );
            path.display().to_string()
        })
        .collect::<Vec<_>>();
    reported.sort();
    let mut expected = paths.to_vec();
    expected.sort();
    assert_eq!(reported, expected);
}

/// The `file_change` each Write or Edit call of `chat` was prepared with, by call id.
async fn prepared_file_changes(fixture: &Fixture, chat: ChatId) -> Vec<(String, bool)> {
    fixture
        .agent
        .snapshot(chat)
        .await
        .unwrap()
        .operations
        .iter()
        .filter(|operation| [WRITE_TOOL_NAME, EDIT_TOOL_NAME].contains(&operation.name.as_str()))
        .map(|operation| (operation.call_id.clone(), operation.file_change.is_some()))
        .collect()
}

const HOST_TEMP_CHILD: &str = "TICKLOOP_HOST_TEMP_CHILD";

/// In the test process, runs `test` again in a child whose `TICKLOOP_TMPDIR` is first a directory
/// nobody may write and then a writable one, checks that both runs passed and left the directory
/// empty, and returns `false`; in the child, returns `true` so the caller runs the test body.
fn in_child_with_host_temp(test: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    if std::env::var_os(HOST_TEMP_CHILD).is_some() {
        return true;
    }
    for mode in [0o500, 0o700] {
        let base = tempfile::tempdir().unwrap();
        std::fs::set_permissions(base.path(), std::fs::Permissions::from_mode(mode)).unwrap();
        let probe = std::fs::write(base.path().join("probe"), "x");
        assert_eq!(probe.is_err(), mode == 0o500, "{probe:?}");
        let _ = std::fs::remove_file(base.path().join("probe"));
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture"])
            .env(HOST_TEMP_CHILD, "1")
            .env("TICKLOOP_TMPDIR", base.path())
            .output()
            .unwrap();
        let left = std::fs::read_dir(base.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        std::fs::set_permissions(base.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "mode {mode:o}:\n{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(left.is_empty(), "mode {mode:o} left {left:?}");
    }
    false
}

/// `lines` lines of `width` characters each, every one containing `delivery`.
fn text_of(lines: usize, width: usize) -> String {
    (0..lines)
        .map(|index| format!("{:<width$}\n", format!("delivery {index}")))
        .collect()
}

fn utf16_length(text: &str) -> usize {
    text.encode_utf16().count()
}

/// The text of each tool result `request` sends, by call id.
fn sent_results(request: &ModelRequest) -> Vec<(String, String)> {
    request
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|content| match content {
            Content::ToolResult {
                tool_use_id,
                content,
                ..
            } => Some((tool_use_id.clone(), content.text().into_owned())),
            _ => None,
        })
        .collect()
}

const NOT_KEPT_NOTE: &str = "the rest is not kept anywhere, because this Chat's Folder is only in its execution environment";

#[tokio::test]
async fn a_remote_folder_chat_keeps_nothing_on_the_host() {
    if !in_child_with_host_temp(&tickloop_harness_test_support::test_path!(
        a_remote_folder_chat_keeps_nothing_on_the_host
    )) {
        return;
    }
    let fixture = Fixture::new();
    fixture
        .environment
        .put_file(Some("app"), "notes.md", "original\n");
    fixture
        .environment
        .put_file(Some("app"), "log.txt", text_of(600, 150));
    for part in 0..4 {
        fixture
            .environment
            .put_file(Some("app"), &format!("part-{part}.txt"), text_of(1500, 30));
    }
    let chat = fixture.agent.create_chat(PathBuf::from(APP)).await.unwrap();

    let (_, results) = fixture
        .turn(
            chat,
            &[
                ("read", "Read", json!({"file_path": "notes.md"})),
                (
                    "edit",
                    "Edit",
                    json!({"file_path": "notes.md", "old_string": "original", "new_string": "changed"}),
                ),
                (
                    "write",
                    "Write",
                    json!({"file_path": format!("{APP}/answer.md"), "content": "yes\n"}),
                ),
            ],
        )
        .await;
    for id in ["read", "edit", "write"] {
        assert!(
            !result(&results, id).is_error,
            "{id}: {:?}",
            result(&results, id)
        );
    }
    assert_eq!(
        fixture.environment.file(Some("app"), "notes.md").as_deref(),
        Some(&b"changed\n"[..])
    );
    assert_eq!(
        fixture
            .environment
            .file(Some("app"), "answer.md")
            .as_deref(),
        Some(&b"yes\n"[..])
    );
    assert_eq!(
        prepared_file_changes(&fixture, chat).await,
        vec![("edit".to_owned(), false), ("write".to_owned(), false)]
    );
    let point = *fixture
        .agent
        .snapshot(chat)
        .await
        .unwrap()
        .message_ids
        .values()
        .next()
        .unwrap();

    let (_, results) = fixture
        .turn(
            chat,
            &[(
                "grep",
                "Grep",
                json!({"pattern": "delivery", "path": "log.txt", "output_mode": "content", "head_limit": 0}),
            )],
        )
        .await;
    let grep = result(&results, "grep");
    assert!(!grep.is_error, "{grep:?}");
    assert!(
        grep.content.contains(NOT_KEPT_NOTE),
        "{}",
        &grep.content[grep.content.len() - 400..]
    );
    assert!(grep.content.contains("delivery 0"));
    assert!(!grep.content.contains("delivery 599"));
    assert!(utf16_length(&grep.content) < 21_000);
    assert_eq!(grep.output_ref, None);

    let reads = (0..4)
        .map(|part| {
            (
                format!("part-{part}"),
                json!({"file_path": format!("part-{part}.txt")}),
            )
        })
        .collect::<Vec<_>>();
    let one_round = reads
        .iter()
        .flat_map(|(id, input)| call(id, READ_TOOL_NAME, input.clone()))
        .collect();
    let (request, results) = fixture.turn_with_replies(chat, vec![one_round]).await;
    let total = reads
        .iter()
        .map(|(id, _)| {
            let read = result(&results, id);
            assert!(
                !read.is_error && !read.content.contains(NOT_KEPT_NOTE),
                "{id}"
            );
            utf16_length(&read.content)
        })
        .sum::<usize>();
    assert!(total > 200_000, "{total}");
    let sent = sent_results(&request);
    let round = reads
        .iter()
        .map(|(id, _)| &sent.iter().find(|(sent_id, _)| sent_id == id).unwrap().1)
        .collect::<Vec<_>>();
    assert!(round.iter().any(|text| text.contains(NOT_KEPT_NOTE)));
    assert!(
        round.iter().map(|text| utf16_length(text)).sum::<usize>() <= 200_000 + 4 * 400,
        "the round was not cut to the budget"
    );

    let fixture = fixture.reopen().await;
    fixture.agent.resume_chat(chat).await.unwrap();
    assert_eq!(
        fixture
            .agent
            .rewind_file_changes(chat, point)
            .await
            .unwrap(),
        Some(Vec::new())
    );
    let outcome = fixture
        .agent
        .rewind(chat, point, tickloop_harness_protocol::RewindMode::Code)
        .await
        .unwrap();
    let code = outcome.code.unwrap();
    assert!(
        code.changed_files.is_empty() && code.error.is_none(),
        "{code:?}"
    );
    assert_only_in_environment(
        &code.failed_files,
        &[format!("{APP}/notes.md"), format!("{APP}/answer.md")],
    );
    assert_eq!(
        fixture.environment.file(Some("app"), "notes.md").as_deref(),
        Some(&b"changed\n"[..])
    );
    fixture.agent.shutdown().await;
}

#[tokio::test]
async fn mixed_roots_keep_records_only_for_files_on_this_machine() {
    let (fixture, chat, root, _) = local_fixture().await;
    let folder = root.path().canonicalize().unwrap();
    fixture
        .environment
        .define_root("design", DESIGN, FileAccess::ReadWrite);
    fixture
        .environment
        .put_file(Some("app"), "local.md", "local original\n");
    fixture
        .environment
        .put_file(Some("design"), "remote.md", "remote original\n");
    fixture
        .agent
        .set_directories(chat, folder.clone(), vec![DESIGN.into()])
        .await
        .unwrap();
    let remote = format!("{DESIGN}/remote.md");
    let (_, results) = fixture
        .turn(
            chat,
            &[
                ("read-local", "Read", json!({"file_path": "local.md"})),
                (
                    "edit-local",
                    "Edit",
                    json!({"file_path": "local.md", "old_string": "original", "new_string": "changed"}),
                ),
                ("read-remote", "Read", json!({"file_path": remote})),
                (
                    "edit-remote",
                    "Edit",
                    json!({"file_path": remote, "old_string": "original", "new_string": "changed"}),
                ),
            ],
        )
        .await;
    for id in ["edit-local", "edit-remote"] {
        assert!(
            !result(&results, id).is_error,
            "{id}: {:?}",
            result(&results, id)
        );
    }
    assert_eq!(
        prepared_file_changes(&fixture, chat).await,
        vec![
            ("edit-local".to_owned(), true),
            ("edit-remote".to_owned(), false)
        ]
    );
    let point = *fixture
        .agent
        .snapshot(chat)
        .await
        .unwrap()
        .message_ids
        .values()
        .next()
        .unwrap();
    fixture.agent.sync().await.unwrap();
    let history = fixture
        .store
        .lines(chat)
        .iter()
        .find_map(|line| match line {
            Line::FileHistorySnapshot(entry) => entry
                .snapshot
                .get("backupDirectory")
                .and_then(Value::as_str)
                .map(PathBuf::from),
            _ => None,
        })
        .unwrap();
    let backups = std::fs::read_dir(&history)
        .unwrap()
        .map(|entry| std::fs::read(entry.unwrap().path()).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(backups, vec![b"local original\n".to_vec()]);

    let code = fixture
        .agent
        .rewind(chat, point, tickloop_harness_protocol::RewindMode::Code)
        .await
        .unwrap()
        .code
        .unwrap();
    assert_eq!(code.changed_files, vec![folder.join("local.md")]);
    assert_only_in_environment(&code.failed_files, std::slice::from_ref(&remote));
    assert_eq!(
        fixture.environment.file(Some("app"), "local.md").as_deref(),
        Some(&b"local original\n"[..])
    );
    assert_eq!(
        fixture
            .environment
            .file(Some("design"), "remote.md")
            .as_deref(),
        Some(&b"remote changed\n"[..])
    );
    fixture.agent.shutdown().await;
}
#[tokio::test]
async fn permissions_view_avoids_remote_project_partition() {
    let settings = Arc::new(RecordingSettings::default());
    let environment = MemoryEnvironment::new();
    environment.define_root("app", APP, FileAccess::ReadWrite);
    let fixture = Fixture::on(
        environment,
        Arc::new(InMemoryTranscriptStore::new()),
        Some(settings.clone()),
    );
    let chat = fixture.agent.create_chat(PathBuf::from(APP)).await.unwrap();
    assert!(!settings.asked_about(APP));
    fixture.agent.permissions_view(chat).await.unwrap();
    fixture.agent.shutdown().await;
    assert!(
        !settings.asked_about(APP),
        "remote permissions view queried local project partition {APP}"
    );
}

#[tokio::test]
async fn remote_directory_controls_and_branch_use_environment_admission() {
    use tickloop_harness_protocol::{BranchRequest, PermissionChange};
    let fixture = Fixture::new();
    let chat = fixture.agent.create_chat(APP.into()).await.unwrap();
    fixture
        .agent
        .add_launch_directories(chat, vec![DESIGN.into()])
        .await
        .unwrap();
    let view = fixture.agent.permissions_view(chat).await.unwrap();
    assert_eq!(view.additional_directories, vec![PathBuf::from(DESIGN)]);
    let view = fixture
        .agent
        .apply_permission_change(
            chat,
            PermissionChange::RemoveDirectory {
                path: DESIGN.into(),
            },
            view.revision,
        )
        .await
        .unwrap();
    assert!(view.additional_directories.is_empty());
    fixture
        .agent
        .apply_permission_change(
            chat,
            PermissionChange::AddDirectory {
                path: DESIGN.into(),
            },
            view.revision,
        )
        .await
        .unwrap();
    fixture
        .environment
        .put_file(Some("app"), "notes.md", "remote branch\n");
    fixture.turn(chat, &[]).await;
    let branch = fixture
        .agent
        .branch_chat(BranchRequest {
            request_id: uuid::Uuid::new_v4(),
            source_chat_id: chat,
            name: None,
        })
        .await
        .unwrap();
    assert_eq!(branch.snapshot.folder, Some(APP.into()));
    let (_, results) = fixture
        .turn(
            branch.child_chat_id,
            &[("branch-read", "Read", json!({"file_path":"notes.md"}))],
        )
        .await;
    assert!(
        result(&results, "branch-read")
            .content
            .contains("remote branch")
    );
    fixture.agent.shutdown().await;
}

#[tokio::test]
async fn remote_skill_preflight_and_persistent_rules_never_use_local_partition() {
    use tickloop_harness_protocol::{Decision, PermissionRule};
    let settings = Arc::new(RecordingSettings::default());
    let environment = MemoryEnvironment::new();
    environment.define_root("app", APP, FileAccess::ReadWrite);
    let fixture = Fixture::on(
        environment,
        Arc::new(InMemoryTranscriptStore::new()),
        Some(settings.clone()),
    );
    assert!(
        fixture
            .agent
            .check_user_skill(Path::new(APP), "local-only", "")
            .await
            .is_err()
    );
    let chat = fixture.agent.create_chat(APP.into()).await.unwrap();
    let rule = PermissionRule {
        id: uuid::Uuid::new_v4(),
        decision: Decision::Allow,
        tool: "Bash".into(),
        specifier: None,
        input: None,
        loop_id: None,
        persistent: true,
    };
    assert!(
        fixture
            .agent
            .set_permission_rules(chat, vec![rule])
            .await
            .is_err()
    );
    assert!(!settings.asked_about(APP));
    fixture.agent.shutdown().await;
}

#[tokio::test]
async fn mixed_roots_recover_and_same_path_placement_changes_refresh_authority() {
    let local = tempfile::tempdir().unwrap();
    let local = local.path().canonicalize().unwrap();
    let settings = Arc::new(RecordingSettings::default());
    settings
        .write(SettingsUpdate::TrustDirectory(local.clone()))
        .unwrap();
    let environment = MemoryEnvironment::new();
    environment.define_root("app", local.to_str().unwrap(), FileAccess::ReadWrite);
    environment.set_root_local_path("app", Some(local.clone()));
    environment.define_root("design", DESIGN, FileAccess::Read);
    let fixture = Fixture::on(
        environment,
        Arc::new(InMemoryTranscriptStore::new()),
        Some(settings.clone()),
    );
    let chat = fixture.agent.create_chat(local.clone()).await.unwrap();
    fixture
        .agent
        .set_directories(chat, local.clone(), vec![DESIGN.into()])
        .await
        .unwrap();
    let (request, _) = fixture.turn(chat, &[]).await;
    assert!(system_text(&request).contains("# Scratchpad Directory"));
    fixture.agent.sync().await.unwrap();
    fixture.agent.shutdown().await;
    let fixture = Fixture::on(fixture.environment, fixture.store, Some(settings.clone()));
    fixture.agent.resume_chat(chat).await.unwrap();
    fixture.turn(chat, &[]).await;
    fixture.environment.set_root_local_path("app", None);
    settings.folders.lock().unwrap().clear();
    fixture
        .agent
        .set_directories(chat, local.clone(), vec![DESIGN.into()])
        .await
        .unwrap();
    let (request, _) = fixture.turn(chat, &[]).await;
    assert!(!system_text(&request).contains("# Scratchpad Directory"));
    fixture.agent.permissions_view(chat).await.unwrap();
    assert!(!settings.asked_about(local.to_str().unwrap()));
    fixture.agent.shutdown().await;
}

#[tokio::test]
async fn queued_and_live_user_shells_block_detaching_until_settled() {
    use tickloop_harness_protocol::UserShellState;
    let fixture = Fixture::new();
    fixture.environment.define_shell(
        "hold",
        std::time::Duration::from_secs(60),
        b"finished\n".to_vec(),
        0,
    );
    let chat = fixture.agent.create_chat(APP.into()).await.unwrap();
    fixture
        .agent
        .set_execution_mode(chat, ExecutionMode::FullAccess)
        .await
        .unwrap();
    let receipt = fixture
        .agent
        .submit_user_shell(chat, "hold".into(), true)
        .await
        .unwrap();
    assert!(matches!(
        fixture
            .agent
            .set_working_directories(chat, None, vec![])
            .await,
        Err(tickloop_harness_runtime::AgentError::Busy)
    ));
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if fixture
                .agent
                .snapshot(chat)
                .await
                .unwrap()
                .user_shells
                .iter()
                .any(|shell| {
                    shell.id == receipt.command_id && shell.state == UserShellState::Background
                })
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(matches!(
        fixture
            .agent
            .set_working_directories(chat, None, vec![])
            .await,
        Err(tickloop_harness_runtime::AgentError::Busy)
    ));
    fixture
        .agent
        .stop_user_shell(chat, receipt.command_id)
        .await
        .unwrap();
    fixture.agent.shutdown().await;
}

struct SingleBindEnvironment {
    memory: MemoryEnvironment,
    binds: std::sync::atomic::AtomicUsize,
}

impl tickloop_harness_runtime::environment::ExecutionEnvironment for SingleBindEnvironment {
    fn bind(
        &self,
        scope: tickloop_harness_runtime::environment::ExecutionScope,
        authority: tickloop_harness_runtime::environment::ExecutionAuthority,
    ) -> Result<
        Arc<dyn tickloop_harness_runtime::environment::ExecutionPort>,
        tickloop_harness_runtime::environment::ExecutionError,
    > {
        if self.binds.fetch_add(1, std::sync::atomic::Ordering::SeqCst) != 0 {
            return Err(tickloop_harness_runtime::environment::ExecutionError::new(
                "Unavailable",
                "binding unavailable",
            ));
        }
        self.memory.bind(scope, authority)
    }
}

#[tokio::test]
async fn admission_uses_one_binding_and_failed_replacement_keeps_remote_authority() {
    let memory = MemoryEnvironment::new();
    memory.define_root("app", APP, FileAccess::ReadWrite);
    memory.put_file(Some("app"), "notes.md", "still remote");
    let environment = Arc::new(SingleBindEnvironment {
        memory: memory.clone(),
        binds: Default::default(),
    });
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = Arc::new(ScriptedModel::new(Vec::new()));
    let agent = Agent::with_seams(
        HarnessSeams {
            search: None,
            transcripts: Some(store.clone()),
            environment: Some(environment.clone()),
            models: ChosenModels::new(model.clone()),
        },
        HarnessOptions {
            host: AgentHostOptions::product_defaults(),
            ..HarnessOptions::local(config())
        },
    )
    .unwrap();
    let fixture = Fixture {
        agent,
        model,
        store,
        environment: memory,
    };
    let chat = fixture.agent.create_chat(APP.into()).await.unwrap();
    assert_eq!(
        environment.binds.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    assert!(
        fixture
            .agent
            .set_directories(chat, APP.into(), vec![])
            .await
            .is_err()
    );
    let (request, results) = fixture
        .turn(chat, &[("read", "Read", json!({"file_path":"notes.md"}))])
        .await;
    assert!(result(&results, "read").content.contains("still remote"));
    assert!(!system_text(&request).contains("# Scratchpad Directory"));
    assert!(fixture.agent.create_chat(APP.into()).await.is_err());
    fixture.agent.shutdown().await;
}

#[tokio::test]
async fn remote_shell_stop_and_outside_root_refusal_use_the_port() {
    let fixture = Fixture::new();
    fixture
        .environment
        .define_shell("hold", std::time::Duration::from_secs(60), vec![], 0);
    let chat = fixture.agent.create_chat(APP.into()).await.unwrap();
    let (_, results) = fixture
        .turn(
            chat,
            &[(
                "start",
                "Bash",
                json!({"command":"hold", "run_in_background":true}),
            )],
        )
        .await;
    assert!(!result(&results, "start").is_error);
    let command = fixture.agent.snapshot(chat).await.unwrap().commands[0].operation_id;
    let (_, results) = fixture
        .turn(
            chat,
            &[
                ("stop", "TaskStop", json!({"task_id":command.to_string()})),
                (
                    "outside",
                    "Bash",
                    json!({"command":"hold", "cwd":"/outside"}),
                ),
            ],
        )
        .await;
    assert!(
        !result(&results, "stop").is_error,
        "{:?}",
        result(&results, "stop")
    );
    assert!(result(&results, "outside").is_error);
    fixture.agent.shutdown().await;
}

#[tokio::test]
async fn local_persistent_rules_do_not_sync_into_remote_chat_at_same_path() {
    use tickloop_harness_protocol::{Decision, PermissionChange, PermissionRule};
    let directory = tempfile::tempdir().unwrap();
    let folder = directory.path().canonicalize().unwrap();
    let settings = Arc::new(RecordingSettings::default());
    settings
        .write(SettingsUpdate::TrustDirectory(folder.clone()))
        .unwrap();
    let environment = MemoryEnvironment::new();
    environment.define_root("app", folder.to_str().unwrap(), FileAccess::ReadWrite);
    environment.set_root_local_path("app", Some(folder.clone()));
    let fixture = Fixture::on(
        environment,
        Arc::new(InMemoryTranscriptStore::new()),
        Some(settings.clone()),
    );
    let local = fixture.agent.create_chat(folder.clone()).await.unwrap();
    fixture.environment.set_root_local_path("app", None);
    let remote = fixture.agent.create_chat(folder.clone()).await.unwrap();
    let rule = PermissionRule {
        id: uuid::Uuid::new_v4(),
        decision: Decision::Allow,
        tool: "Bash".into(),
        specifier: None,
        input: None,
        loop_id: None,
        persistent: true,
    };
    fixture
        .agent
        .set_permission_rules(local, vec![rule.clone()])
        .await
        .unwrap();
    assert!(
        fixture
            .agent
            .snapshot(remote)
            .await
            .unwrap()
            .policy
            .rules
            .is_empty()
    );
    settings.folders.lock().unwrap().clear();
    let view = fixture.agent.permissions_view(remote).await.unwrap();
    assert!(
        fixture
            .agent
            .apply_permission_change(remote, PermissionChange::UpsertRule { rule }, view.revision)
            .await
            .is_err()
    );
    assert!(!settings.asked_about(folder.to_str().unwrap()));
    fixture.agent.shutdown().await;
}

async fn local_fixture() -> (Fixture, ChatId, tempfile::TempDir, Arc<RecordingSettings>) {
    let root = tempfile::tempdir_in(tickloop_harness_test_support::SHORT_TEMP_ROOT).unwrap();
    let folder = root.path().canonicalize().unwrap();
    let settings = Arc::new(RecordingSettings::default());
    settings
        .write(SettingsUpdate::TrustDirectory(folder.clone()))
        .unwrap();
    let environment = MemoryEnvironment::new();
    environment.define_root("app", folder.to_str().unwrap(), FileAccess::ReadWrite);
    environment.set_root_local_path("app", Some(folder.clone()));
    let fixture = Fixture::on(
        environment,
        Arc::new(InMemoryTranscriptStore::new()),
        Some(settings.clone()),
    );
    let chat = fixture.agent.create_chat(folder).await.unwrap();
    (fixture, chat, root, settings)
}
#[tokio::test]
async fn mixed_roots_can_actually_read_remote_additional_directory() {
    let (fixture, chat, root, _) = local_fixture().await;
    fixture
        .environment
        .define_root("design", DESIGN, FileAccess::Read);
    fixture
        .environment
        .put_file(Some("design"), "notes.md", "remote content\n");
    fixture
        .agent
        .set_directories(chat, root.path().to_path_buf(), vec![DESIGN.into()])
        .await
        .unwrap();
    let (_, results) = fixture
        .turn(
            chat,
            &[
                (
                    "read",
                    "Read",
                    json!({"file_path":format!("{DESIGN}/notes.md")}),
                ),
                (
                    "write",
                    "Write",
                    json!({"file_path":"local.md", "content":"local content"}),
                ),
                (
                    "remote-write",
                    "Write",
                    json!({"file_path":format!("{DESIGN}/forbidden.md"), "content":"no"}),
                ),
            ],
        )
        .await;
    assert!(!result(&results, "write").is_error);
    assert!(result(&results, "remote-write").is_error);
    assert_eq!(
        fixture.environment.file(Some("app"), "local.md").as_deref(),
        Some(&b"local content"[..])
    );
    fixture.agent.shutdown().await;
    let read = result(&results, "read");
    assert!(
        !read.is_error && read.content.contains("remote content"),
        "{read:?}"
    );
}
#[tokio::test]
async fn detached_chat_survives_worker_upgrade() {
    let environment = MemoryEnvironment::new();
    environment.define_root("app", APP, FileAccess::ReadWrite);
    let make_agent = || {
        Agent::with_seams(
            HarnessSeams {
                search: None,
                transcripts: None,
                environment: Some(Arc::new(environment.clone())),
                models: ChosenModels::new(Arc::new(ScriptedModel::new(Vec::new()))),
            },
            HarnessOptions {
                host: AgentHostOptions::product_defaults(),
                ..HarnessOptions::local(config())
            },
        )
        .unwrap()
    };
    let original = make_agent();
    let chat = original.create_chat(APP.into()).await.unwrap();
    original
        .set_working_directories(chat, None, vec![])
        .await
        .unwrap();
    let checkpoint = original
        .prepare_upgrade()
        .await
        .unwrap()
        .expect("idle Chat checkpoint");
    let replacement = make_agent();
    let outcome = replacement.restore_upgrade(checkpoint).await;
    replacement.cancel_upgrade().await;
    original.cancel_upgrade().await;
    replacement.shutdown().await;
    original.shutdown().await;
    assert!(outcome.is_ok(), "detached Chat restore failed: {outcome:?}");
}

#[tokio::test]
async fn adding_local_directory_preserves_instruction_prefix() {
    let (fixture, chat, root, settings) = local_fixture().await;
    *settings.instruction_text.lock().unwrap() = "CACHED_INSTRUCTIONS_A".into();
    let (before, _) = fixture.turn(chat, &[]).await;
    let before = before
        .messages
        .iter()
        .find(|m| m.role == Role::User)
        .unwrap()
        .clone();
    *settings.instruction_text.lock().unwrap() = "EDITED_INSTRUCTIONS_B".into();
    let extra = root.path().join("extra");
    std::fs::create_dir(&extra).unwrap();
    fixture
        .environment
        .define_root("extra", extra.to_str().unwrap(), FileAccess::ReadWrite);
    fixture
        .environment
        .set_root_local_path("extra", Some(extra.clone()));
    fixture
        .agent
        .add_launch_directories(chat, vec![extra])
        .await
        .unwrap();
    let (after, _) = fixture.turn(chat, &[]).await;
    let after = after
        .messages
        .iter()
        .find(|m| m.role == Role::User)
        .unwrap()
        .clone();
    fixture.agent.shutdown().await;
    assert_eq!(
        serde_json::to_value(before).unwrap(),
        serde_json::to_value(after).unwrap(),
        "adding local directory must preserve cached instruction prefix"
    );
}
#[tokio::test]
async fn noop_local_setter_keeps_pending_write_approval_valid() {
    use tickloop_harness_protocol::{Decision, PermissionRule};
    let (fixture, chat, root, _) = local_fixture().await;
    fixture
        .agent
        .set_permission_rules(
            chat,
            vec![PermissionRule {
                id: uuid::Uuid::new_v4(),
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
    fixture.model.enqueue(call(
        "write",
        "Write",
        json!({"file_path":"new.md", "content":"new data"}),
    ));
    fixture.model.enqueue(text("done"));
    let mut events = fixture.agent.subscribe();
    let receipt = fixture
        .agent
        .submit(chat, "Write new.md".into())
        .await
        .unwrap();
    let mut outcome = None;
    let mut answered = false;
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(10), events.recv())
            .await
            .unwrap()
            .unwrap();
        match event.kind {
            EventKind::PermissionRequested { request } if event.chat_id == chat => {
                fixture
                    .agent
                    .set_directories(chat, root.path().to_path_buf(), vec![])
                    .await
                    .unwrap();
                let answer = fixture
                    .agent
                    .answer_permission(chat, request.id, true)
                    .await;
                if let Err(error) = answer {
                    outcome = Some(Err(format!("approval rejected after no-op: {error}")));
                    break;
                }
                answered = true;
            }
            EventKind::ToolCompleted {
                call_id, result, ..
            } if call_id == "write" => {
                outcome = Some(if result.is_error {
                    Err(result.content)
                } else {
                    Ok(())
                })
            }
            EventKind::Completed { .. } if event.run_id == receipt.run_id => break,
            EventKind::Failed(error) if event.run_id == receipt.run_id => {
                outcome = Some(Err(error.to_string()));
                break;
            }
            _ => {}
        }
    }
    fixture.agent.shutdown().await;
    assert!(
        answered && matches!(outcome, Some(Ok(()))),
        "write after no-op update: {outcome:?}"
    );
}
#[tokio::test]
async fn local_file_tools_cannot_corrupt_rewind_backups() {
    let root = tempfile::tempdir_in(tickloop_harness_test_support::SHORT_TEMP_ROOT).unwrap();
    let folder = root.path().canonicalize().unwrap();
    let settings = Arc::new(RecordingSettings::default());
    settings
        .write(SettingsUpdate::TrustDirectory(folder.clone()))
        .unwrap();
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = Arc::new(ScriptedModel::new(Vec::new()));
    let agent = Agent::with_seams(
        HarnessSeams {
            search: None,
            transcripts: Some(store.clone()),
            environment: Some(Arc::new(
                tickloop_harness_runtime::environment::FolderEnvironment::bare(),
            )),
            models: ChosenModels::new(model.clone()),
        },
        HarnessOptions {
            host: AgentHostOptions::product_defaults(),
            settings: Some(settings),
            ..HarnessOptions::local(config())
        },
    )
    .unwrap();
    let fixture = Fixture {
        agent,
        model,
        store,
        environment: MemoryEnvironment::new(),
    };
    let chat = fixture.agent.create_chat(folder.clone()).await.unwrap();
    std::fs::write(folder.join("notes.md"), "original\n").unwrap();
    let (request, results) = fixture.turn(chat, &[
        ("read", "Read", json!({"file_path":"notes.md"})),
        ("edit", "Edit", json!({"file_path":"notes.md", "old_string":"original", "new_string":"changed"})),
    ]).await;
    assert!(!result(&results, "edit").is_error);
    let point = *fixture
        .agent
        .snapshot(chat)
        .await
        .unwrap()
        .message_ids
        .values()
        .next()
        .unwrap();
    fixture.agent.sync().await.unwrap();
    let history = fixture
        .store
        .lines(chat)
        .iter()
        .find_map(|line| match line {
            Line::FileHistorySnapshot(entry) => entry
                .snapshot
                .get("backupDirectory")
                .and_then(Value::as_str)
                .map(PathBuf::from),
            _ => None,
        })
        .unwrap();
    let prompt = system_text(&request);
    let host: Value =
        serde_json::from_str(prompt.split("Current host state: ").last().unwrap()).unwrap();
    assert!(!history.starts_with(host["family_temp_directory"].as_str().unwrap()));
    let backup = std::fs::read_dir(&history)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.file_name().unwrap().to_string_lossy().ends_with("@v1"))
        .unwrap();
    std::os::unix::fs::symlink(&backup, folder.join("backup-link")).unwrap();
    fixture
        .agent
        .add_launch_directories(chat, vec![history.clone()])
        .await
        .unwrap();
    let (_, results) = fixture
        .turn(
            chat,
            &[
                ("read-backup", "Read", json!({"file_path":backup})),
                (
                    "write-backup",
                    "Write",
                    json!({"file_path":backup,"content":"tampered\n"}),
                ),
                ("linked-backup", "Read", json!({"file_path":"backup-link"})),
                (
                    "search-backup",
                    "Grep",
                    json!({"path":history.parent(),"pattern":"original","output_mode":"content"}),
                ),
                (
                    "glob-backup",
                    "Glob",
                    json!({"path":history.parent(),"pattern":"**/*@v1"}),
                ),
            ],
        )
        .await;
    for id in ["read-backup", "write-backup", "linked-backup"] {
        assert!(
            result(&results, id).is_error,
            "{id}: {:?}",
            result(&results, id)
        );
    }
    assert!(
        !result(&results, "search-backup")
            .content
            .contains("original")
    );
    assert!(
        !result(&results, "glob-backup")
            .content
            .contains(backup.file_name().unwrap().to_str().unwrap())
    );
    fixture
        .agent
        .rewind(chat, point, tickloop_harness_protocol::RewindMode::Code)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(folder.join("notes.md")).unwrap(),
        "original\n"
    );
    fixture.agent.shutdown().await;
}

fn upgrade_fixture(environment: MemoryEnvironment, settings: Arc<RecordingSettings>) -> Fixture {
    let model = Arc::new(ScriptedModel::new(Vec::new()));
    let agent = Agent::with_seams(
        HarnessSeams {
            search: None,
            transcripts: None,
            environment: Some(Arc::new(environment.clone())),
            models: ChosenModels::new(model.clone()),
        },
        HarnessOptions {
            host: AgentHostOptions::product_defaults(),
            settings: Some(settings),
            ..HarnessOptions::local(config())
        },
    )
    .unwrap();
    Fixture {
        agent,
        model,
        environment,
        store: Arc::new(InMemoryTranscriptStore::new()),
    }
}

#[tokio::test]
async fn detached_upgrade_retains_backups_from_former_local_folder() {
    check_local_history_upgrade(true, false).await;
}

#[tokio::test]
async fn legacy_local_checkpoint_finds_existing_backups() {
    check_local_history_upgrade(false, true).await;
}

async fn check_local_history_upgrade(detach: bool, legacy: bool) {
    let root = tempfile::tempdir_in(tickloop_harness_test_support::SHORT_TEMP_ROOT).unwrap();
    let folder = root.path().canonicalize().unwrap();
    let settings = Arc::new(RecordingSettings::default());
    settings
        .write(SettingsUpdate::TrustDirectory(folder.clone()))
        .unwrap();
    let environment = MemoryEnvironment::new();
    environment.define_root("app", folder.to_str().unwrap(), FileAccess::ReadWrite);
    environment.set_root_local_path("app", Some(folder.clone()));
    environment.put_file(Some("app"), "notes.md", "original\n");
    let fixture = upgrade_fixture(environment.clone(), settings.clone());
    let chat = fixture.agent.create_chat(folder.clone()).await.unwrap();
    let (request, results) = fixture.turn(chat, &[
        ("read", "Read", json!({"file_path":"notes.md"})),
        ("edit", "Edit", json!({"file_path":"notes.md", "old_string":"original", "new_string":"changed"})),
    ]).await;
    assert!(!result(&results, "edit").is_error);
    let point = *fixture
        .agent
        .snapshot(chat)
        .await
        .unwrap()
        .message_ids
        .values()
        .next()
        .unwrap();
    if detach {
        fixture
            .agent
            .set_working_directories(chat, None, vec![])
            .await
            .unwrap();
    }
    let checkpoint = fixture.agent.prepare_upgrade().await.unwrap().unwrap();
    let mut encoded = serde_json::to_value(checkpoint).unwrap();
    let prompt = system_text(&request);
    let host: Value =
        serde_json::from_str(prompt.split("Current host state: ").last().unwrap()).unwrap();
    let family = PathBuf::from(host["family_temp_directory"].as_str().unwrap());
    let project: String = folder
        .to_str()
        .unwrap()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let legacy_directory = family
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join(project)
        .join("file-history")
        .join(chat.uuid().to_string());
    assert_eq!(
        encoded["chats"][0]["file_history"]["backup_directory"],
        json!(legacy_directory)
    );
    assert!(
        std::fs::read_dir(&legacy_directory)
            .unwrap()
            .next()
            .is_some()
    );
    if legacy {
        let history = &mut encoded["chats"][0]["file_history"];
        history.as_object_mut().unwrap().remove("backup_directory");
        for snapshot in history["snapshots"].as_array_mut().unwrap() {
            snapshot.as_object_mut().unwrap().remove("backupDirectory");
        }
    }
    let replacement = upgrade_fixture(environment, settings);
    replacement
        .agent
        .restore_upgrade(serde_json::from_value(encoded).unwrap())
        .await
        .unwrap();
    replacement.agent.cancel_upgrade().await;
    fixture.agent.cancel_upgrade().await;
    if detach {
        assert!(
            replacement
                .agent
                .snapshot(chat)
                .await
                .unwrap()
                .folder
                .is_none()
        );
        replacement
            .agent
            .set_working_directories(chat, Some(folder), vec![])
            .await
            .unwrap();
    }
    replacement
        .agent
        .rewind(chat, point, tickloop_harness_protocol::RewindMode::Code)
        .await
        .unwrap();
    assert_eq!(
        replacement
            .environment
            .file(Some("app"), "notes.md")
            .as_deref(),
        Some(&b"original\n"[..])
    );
    replacement.agent.shutdown().await;
    fixture.agent.shutdown().await;
}

#[tokio::test]
async fn normalized_noop_keeps_revision_but_changed_shell_capabilities_refresh() {
    let fixture = Fixture::new();
    let chat = fixture.agent.create_chat(APP.into()).await.unwrap();
    fixture
        .agent
        .set_directories(chat, APP.into(), vec![DESIGN.into()])
        .await
        .unwrap();
    let before = fixture.agent.snapshot(chat).await.unwrap().policy.revision;
    fixture
        .agent
        .set_working_directories(chat, Some(APP.into()), vec![DESIGN.into(), DESIGN.into()])
        .await
        .unwrap();
    assert_eq!(
        fixture.agent.snapshot(chat).await.unwrap().policy.revision,
        before
    );
    fixture.environment.set_shells(false);
    fixture
        .agent
        .set_directories(chat, APP.into(), vec![DESIGN.into()])
        .await
        .unwrap();
    assert!(fixture.agent.snapshot(chat).await.unwrap().policy.revision > before);
    let (request, _) = fixture.turn(chat, &[]).await;
    for tool in SHELL_TOOLS {
        assert!(!tool_names(&request).contains(&tool));
    }
    fixture.agent.shutdown().await;
}

#[tokio::test]
async fn a_remote_root_named_like_private_history_still_reads_only_its_environment() {
    let (fixture, chat, _root, _) = local_fixture().await;
    let (request, _) = fixture.turn(chat, &[]).await;
    let prompt = system_text(&request);
    let host: Value =
        serde_json::from_str(prompt.split("Current host state: ").last().unwrap()).unwrap();
    let family = PathBuf::from(host["family_temp_directory"].as_str().unwrap());
    let displayed = family
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("file-history");
    fixture
        .environment
        .define_root("remote", displayed.to_str().unwrap(), FileAccess::ReadWrite);
    fixture
        .environment
        .put_file(Some("remote"), "notes.md", "environment content");
    let remote = fixture.agent.create_chat(displayed).await.unwrap();
    let (_, results) = fixture
        .turn(remote, &[("read", "Read", json!({"file_path":"notes.md"}))])
        .await;
    assert!(!result(&results, "read").is_error);
    assert!(
        result(&results, "read")
            .content
            .contains("environment content")
    );
    fixture.agent.shutdown().await;
}

async fn protected_search_fixture() -> (Fixture, ChatId, tempfile::TempDir, PathBuf, PathBuf) {
    let root = tempfile::tempdir_in(tickloop_harness_test_support::SHORT_TEMP_ROOT).unwrap();
    let folder = root.path().canonicalize().unwrap();
    let settings = Arc::new(RecordingSettings::default());
    settings
        .write(SettingsUpdate::TrustDirectory(folder.clone()))
        .unwrap();
    let store = Arc::new(InMemoryTranscriptStore::new());
    let model = Arc::new(ScriptedModel::new(Vec::new()));
    let agent = Agent::with_seams(
        HarnessSeams {
            search: None,
            transcripts: Some(store.clone()),
            environment: Some(Arc::new(
                tickloop_harness_runtime::environment::FolderEnvironment::bare(),
            )),
            models: ChosenModels::new(model.clone()),
        },
        HarnessOptions {
            host: AgentHostOptions::product_defaults(),
            settings: Some(settings),
            ..HarnessOptions::local(config())
        },
    )
    .unwrap();
    let fixture = Fixture {
        agent,
        model,
        store,
        environment: MemoryEnvironment::new(),
    };
    let chat = fixture.agent.create_chat(folder.clone()).await.unwrap();
    std::fs::write(folder.join("notes.md"), "original\n").unwrap();
    let (request, results) = fixture.turn(chat, &[
        ("read", "Read", json!({"file_path":"notes.md"})),
        ("edit", "Edit", json!({"file_path":"notes.md", "old_string":"original", "new_string":"changed"})),
    ]).await;
    assert!(!result(&results, "edit").is_error);
    fixture.agent.sync().await.unwrap();
    let history = fixture
        .store
        .lines(chat)
        .iter()
        .find_map(|line| match line {
            Line::FileHistorySnapshot(entry) => entry
                .snapshot
                .get("backupDirectory")
                .and_then(Value::as_str)
                .map(PathBuf::from),
            _ => None,
        })
        .unwrap();
    let prompt = system_text(&request);
    let host: Value =
        serde_json::from_str(prompt.split("Current host state: ").last().unwrap()).unwrap();
    assert!(!history.starts_with(host["family_temp_directory"].as_str().unwrap()));
    let backup = std::fs::read_dir(&history)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.file_name().unwrap().to_string_lossy().ends_with("@v1"))
        .unwrap();
    (fixture, chat, root, history, backup)
}

#[tokio::test]
async fn directory_attachment_uses_protected_listing() {
    let (fixture, chat, _root, history, _backup) = protected_search_fixture().await;
    let ancestor = history.parent().unwrap().parent().unwrap().to_path_buf();
    fixture
        .agent
        .add_launch_directories(chat, vec![ancestor.clone()])
        .await
        .unwrap();
    fixture.model.enqueue(text("done"));
    let mut events = fixture.agent.subscribe();
    let receipt = fixture
        .agent
        .submit_with_input_contexts(chat, "inspect directory".into(), Vec::new(), vec![ancestor])
        .await
        .unwrap();
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(10), events.recv())
            .await
            .unwrap()
            .unwrap();
        match event.kind {
            EventKind::Completed { .. } if event.run_id == receipt.run_id => break,
            EventKind::Failed(error) if event.run_id == receipt.run_id => panic!("{error}"),
            _ => {}
        }
    }
    let snapshot = fixture.agent.snapshot(chat).await.unwrap();
    let contexts = serde_json::to_string(&snapshot.message_contexts).unwrap();
    fixture.agent.shutdown().await;
    assert!(
        !contexts.contains("file-history/"),
        "protected child leaked into attachment: {contexts}"
    );
}

#[tokio::test]
async fn protected_matches_do_not_consume_glob_and_grep_pages() {
    use std::io::Write;
    let (fixture, chat, _root, history, _backup) = protected_search_fixture().await;
    let ancestor = history.parent().unwrap().parent().unwrap();
    let hidden = tempfile::tempdir_in(&history).unwrap();
    for index in 0..105 {
        std::fs::write(
            hidden.path().join(format!("hidden-{index:03}.reviewprobe")),
            "needle HIDDEN\n",
        )
        .unwrap();
    }
    let mut visible = tempfile::Builder::new()
        .prefix("z-visible-")
        .suffix(".reviewprobe")
        .tempfile_in(ancestor)
        .unwrap();
    visible
        .write_all(b"needle VISIBLE_FIRST\nneedle VISIBLE_SECOND\nneedle VISIBLE_THIRD\n")
        .unwrap();
    let name = visible.path().file_name().unwrap().to_str().unwrap();
    let (_, results) = fixture.turn(chat, &[
        ("glob", "Glob", json!({"path":ancestor,"pattern":"**/*.reviewprobe"})),
        ("grep-files", "Grep", json!({"path":ancestor,"pattern":"needle","glob":"**/*.reviewprobe","head_limit":1})),
        ("grep-content", "Grep", json!({"path":ancestor,"pattern":"needle","glob":"**/*.reviewprobe","output_mode":"content","head_limit":1,"offset":1})),
        ("grep-count", "Grep", json!({"path":ancestor,"pattern":"needle","glob":"**/*.reviewprobe","output_mode":"count","head_limit":1})),
    ]).await;
    fixture.agent.shutdown().await;
    for id in ["glob", "grep-files", "grep-count"] {
        let found = result(&results, id);
        assert!(
            !found.is_error && found.content.contains(name),
            "{id}: {found:?}"
        );
        assert!(!found.content.contains("hidden-"), "{id}: {found:?}");
    }
    let page = result(&results, "grep-content");
    assert!(
        !page.is_error && page.content.contains("VISIBLE_SECOND"),
        "{page:?}"
    );
    for absent in ["HIDDEN", "VISIBLE_FIRST", "VISIBLE_THIRD"] {
        assert!(!page.content.contains(absent), "{page:?}");
    }
}

#[tokio::test]
async fn memory_queries_exclude_subtrees_before_limits_and_offsets() {
    use tickloop_harness_runtime::environment::{
        ExecutionAuthority, ExecutionEnvironment, ExecutionInspection, ExecutionQuery,
        ExecutionScope, FileInspection, FilePath, FileQuery, RootName,
    };
    use tickloop_harness_tools::file_search::{GlobQuery, GrepOutput, GrepOutputMode, GrepQuery};
    let environment = MemoryEnvironment::new();
    environment.define_root("app", APP, FileAccess::ReadWrite);
    let name = RootName::new("app").unwrap();
    let mut excluded = Vec::new();
    for index in 0..105 {
        let folder = format!("a-private-{index:03}");
        environment.put_file(
            Some("app"),
            &format!("{folder}/hidden.txt"),
            "needle HIDDEN\n",
        );
        excluded.push(FilePath::in_root(name.clone(), &folder).unwrap());
    }
    environment.put_file(
        Some("app"),
        "z-visible.txt",
        "needle FIRST\nneedle SECOND\nneedle THIRD\n",
    );
    let chat: ChatId = tickloop_ids::generate().into();
    let port = environment
        .bind(
            ExecutionScope {
                chat,
                folder: Some(APP.into()),
                additional_directories: vec![],
            },
            ExecutionAuthority::new(chat),
        )
        .unwrap();
    let path = FilePath::in_root(name, "").unwrap();
    let ExecutionInspection::File(FileInspection::List(listing)) = port
        .inspect(ExecutionQuery::File(
            FileQuery::List {
                path: path.clone(),
                limit: 100,
            }
            .excluding(excluded.clone()),
        ))
        .await
        .unwrap()
    else {
        panic!("expected listing");
    };
    assert_eq!(
        listing
            .entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>(),
        ["z-visible.txt"]
    );
    assert!(!listing.truncated);
    let ExecutionInspection::File(FileInspection::Glob(found)) = port
        .inspect(ExecutionQuery::File(
            FileQuery::Glob {
                path: path.clone(),
                query: GlobQuery {
                    pattern: "**/*.txt".into(),
                    limit: 100,
                },
            }
            .excluding(excluded.clone()),
        ))
        .await
        .unwrap()
    else {
        panic!("expected glob");
    };
    assert_eq!(
        found
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>(),
        ["z-visible.txt"]
    );
    assert!(!found.truncated);
    let ExecutionInspection::File(FileInspection::Grep(found)) = port
        .inspect(ExecutionQuery::File(
            FileQuery::Grep {
                path,
                query: GrepQuery {
                    pattern: "needle".into(),
                    globs: vec![],
                    file_type: None,
                    output_mode: GrepOutputMode::Content,
                    lines_before: 0,
                    lines_after: 0,
                    line_numbers: false,
                    case_insensitive: false,
                    multiline: false,
                    head_limit: Some(1),
                    offset: 1,
                },
            }
            .excluding(excluded),
        ))
        .await
        .unwrap()
    else {
        panic!("expected grep");
    };
    let GrepOutput::Content(lines) = found.output else {
        panic!("expected content");
    };
    assert_eq!(lines.len(), 1);
    let tickloop_harness_tools::file_search::GrepLine::Match(line) = &lines[0] else {
        panic!("expected match");
    };
    assert_eq!(line.text.as_deref(), Some("needle SECOND"));
}

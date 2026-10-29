//! What a Chat offers the model follows the file roots its Execution Environment Port reports:
//! which file tools it lists, how its system prompt names the Folder and Additional Directories,
//! and whether it reads git information, bound again when the directories change.

use std::{path::PathBuf, process::Command, sync::Arc};

use tickloop_harness_model::ModelRequest;
use tickloop_harness_protocol::{ChatId, Content, EventKind, Role};
use tickloop_harness_runtime::{
    Agent, HarnessOptions, HarnessSeams,
    environment::{FileAccess, memory::MemoryEnvironment},
};
use tickloop_harness_test_support::{
    ScriptedModel, config, harness::ChosenModels, is_user_context, next_event, text,
};
use tickloop_harness_tools::{
    BASH_TOOL_NAME, EDIT_TOOL_NAME, GLOB_TOOL_NAME, GREP_TOOL_NAME, READ_TOOL_NAME,
    TASK_STOP_TOOL_NAME, WRITE_TOOL_NAME,
};

const READING_TOOLS: [&str; 3] = [READ_TOOL_NAME, GLOB_TOOL_NAME, GREP_TOOL_NAME];
const WRITING_TOOLS: [&str; 2] = [WRITE_TOOL_NAME, EDIT_TOOL_NAME];

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
}

struct Fixture {
    agent: Agent,
    model: Arc<ScriptedModel>,
    environment: MemoryEnvironment,
    folder: Directory,
}

impl Fixture {
    /// An Agent on a memory environment whose Folder root has `access`.
    fn new(access: FileAccess) -> Self {
        let folder = Directory::new();
        let environment = MemoryEnvironment::new();
        environment.define_root("folder", &folder.display(), access);
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
        Self {
            agent,
            model,
            environment,
            folder,
        }
    }

    async fn chat(&self) -> ChatId {
        self.agent
            .create_chat(self.folder.path.clone())
            .await
            .unwrap()
    }

    /// The conversation request of one turn that answers at once.
    async fn request(&self, chat: ChatId) -> ModelRequest {
        self.model.enqueue(text("done"));
        let mut events = self.agent.subscribe();
        let receipt = self
            .agent
            .submit(chat, "Look around.".into())
            .await
            .unwrap();
        loop {
            let event = next_event(&mut events).await;
            match event.kind {
                EventKind::Completed { .. } if event.run_id == receipt.run_id => break,
                EventKind::Failed(error) => panic!("{error}"),
                _ => {}
            }
        }
        self.model.requests.lock().unwrap().last().unwrap().clone()
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
    message_text(request, |message| message.role == Role::System)
}

fn user_context_text(request: &ModelRequest) -> String {
    message_text(request, is_user_context)
}

fn message_text(
    request: &ModelRequest,
    selected: impl Fn(&tickloop_harness_protocol::Message) -> bool,
) -> String {
    request
        .messages
        .iter()
        .filter(|message| selected(message))
        .flat_map(|message| &message.content)
        .filter_map(|content| match content {
            Content::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_read_only_folder_offers_only_the_reading_file_tools() {
    let writable = Fixture::new(FileAccess::ReadWrite);
    let writable_request = writable.request(writable.chat().await).await;
    let fixture = Fixture::new(FileAccess::Read);
    let request = fixture.request(fixture.chat().await).await;
    let names = tool_names(&request);
    for tool in READING_TOOLS
        .into_iter()
        .chain([BASH_TOOL_NAME, TASK_STOP_TOOL_NAME])
    {
        assert!(names.contains(&tool), "{tool} missing from {names:?}");
    }
    let mut expected = tool_names(&writable_request);
    for tool in WRITING_TOOLS {
        assert!(expected.contains(&tool), "{tool} missing from {expected:?}");
    }
    expected.retain(|tool| !WRITING_TOOLS.contains(tool));
    assert_eq!(names, expected);
}

#[tokio::test]
async fn closed_roots_offer_no_file_tools() {
    let fixture = Fixture::new(FileAccess::None);
    let notes = Directory::new();
    fixture
        .environment
        .define_root("notes", &notes.display(), FileAccess::None);
    let chat = fixture.chat().await;
    fixture
        .agent
        .set_directories(chat, fixture.folder.path.clone(), vec![notes.path.clone()])
        .await
        .unwrap();
    let request = fixture.request(chat).await;
    let names = tool_names(&request);
    for tool in READING_TOOLS.into_iter().chain(WRITING_TOOLS) {
        assert!(!names.contains(&tool), "{tool} listed in {names:?}");
    }
    for tool in [BASH_TOOL_NAME, TASK_STOP_TOOL_NAME] {
        assert!(names.contains(&tool), "{tool} missing from {names:?}");
    }
}

#[tokio::test]
async fn the_system_prompt_names_each_directory_by_display_path_and_marks_its_access() {
    let fixture = Fixture::new(FileAccess::Read);
    let drafts = Directory::new();
    let reference = Directory::new();
    let sealed = Directory::new();
    fixture
        .environment
        .define_root("a-drafts", &drafts.display(), FileAccess::ReadWrite);
    fixture
        .environment
        .define_root("b-reference", &reference.display(), FileAccess::Read);
    fixture
        .environment
        .define_root("c-sealed", &sealed.display(), FileAccess::None);
    let chat = fixture.chat().await;
    fixture
        .agent
        .set_directories(
            chat,
            fixture.folder.path.clone(),
            vec![
                drafts.path.clone(),
                reference.path.clone(),
                sealed.path.clone(),
            ],
        )
        .await
        .unwrap();
    let request = fixture.request(chat).await;
    let system = system_text(&request);
    assert!(
        system.contains(&format!(
            " - Primary working directory: {} (read-only)\n",
            fixture.folder.display()
        )),
        "{system}"
    );
    assert!(
        system.contains(&format!(
            " - Additional working directories: {}, {} (read-only), {} (no file access)\n",
            drafts.display(),
            reference.display(),
            sealed.display()
        )),
        "{system}"
    );
    assert!(
        system.contains(&format!(r#""workdir":"{}""#, fixture.folder.display())),
        "{system}"
    );
    let names = tool_names(&request);
    for tool in READING_TOOLS.into_iter().chain(WRITING_TOOLS) {
        assert!(names.contains(&tool), "{tool} missing from {names:?}");
    }
}

#[tokio::test]
async fn a_folder_elsewhere_reports_no_git_information() {
    let fixture = Fixture::new(FileAccess::ReadWrite);
    let git = |args: &[&str]| {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(&fixture.folder.path)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .status()
                .unwrap()
                .success()
        );
    };
    git(&["init", "-q", "-b", "trunk"]);
    git(&[
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=f@example.test",
        "commit",
        "-q",
        "--allow-empty",
        "-m",
        "first commit",
    ]);
    let chat = fixture.chat().await;
    let request = fixture.request(chat).await;
    let system = system_text(&request);
    assert!(
        system.contains(&format!(
            " - Primary working directory: {}\n",
            fixture.folder.display()
        )),
        "{system}"
    );
    for absent in ["Is a git repository", "OS Version", "Additional working"] {
        assert!(!system.contains(absent), "{absent} in {system}");
    }
    let context = user_context_text(&request);
    assert!(!context.is_empty());
    for absent in ["git status", "Current branch", "first commit"] {
        assert!(!context.contains(absent), "{absent} in {context}");
    }
}

#[tokio::test]
async fn changing_directories_binds_a_port_for_the_new_ones() {
    let fixture = Fixture::new(FileAccess::Read);
    let drafts = Directory::new();
    fixture
        .environment
        .define_root("drafts", &drafts.display(), FileAccess::ReadWrite);
    let chat = fixture.chat().await;
    let listed = |request: &ModelRequest| {
        let names = tool_names(request);
        WRITING_TOOLS.map(|tool| names.contains(&tool))
    };
    let before = fixture.request(chat).await;
    assert_eq!(listed(&before), [false, false]);
    assert!(!system_text(&before).contains("Additional working directories"));

    fixture
        .agent
        .set_directories(chat, fixture.folder.path.clone(), vec![drafts.path.clone()])
        .await
        .unwrap();
    let added = fixture.request(chat).await;
    assert_eq!(listed(&added), [true, true]);
    assert!(
        system_text(&added).contains(&format!(
            " - Additional working directories: {}\n",
            drafts.display()
        )),
        "{}",
        system_text(&added)
    );

    fixture
        .agent
        .set_directories(chat, fixture.folder.path.clone(), Vec::new())
        .await
        .unwrap();
    let removed = fixture.request(chat).await;
    assert_eq!(listed(&removed), [false, false]);
    assert!(!system_text(&removed).contains("Additional working directories"));
}

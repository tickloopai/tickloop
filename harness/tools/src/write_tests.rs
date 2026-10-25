use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::{ToolError, Tools, Workspace};
use tickloop_harness_protocol::{FileChangeAction, ToolResult};

const NOT_READ: &str = "File has not been read yet. Read it first before writing to it.";
const MODIFIED_SINCE_READ: &str = "File has been modified since read, either by the user or by a linter. Read it again before attempting to write it.";

struct Chat {
    directory: tempfile::TempDir,
    output: tempfile::TempDir,
    tools: Tools,
}

impl Chat {
    fn new() -> Self {
        let output = tempfile::tempdir().unwrap();
        let tools = Tools::new(output.path().join("chat-output"));
        Self {
            directory: tempfile::tempdir().unwrap(),
            output,
            tools,
        }
    }

    fn workspace(&self) -> Workspace {
        Workspace {
            folder: Some(self.directory.path().to_path_buf()),
            additional_directories: Vec::new(),
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.directory.path().canonicalize().unwrap().join(name)
    }

    fn file(&self, name: &str, content: &str) -> PathBuf {
        let path = self.path(name);
        std::fs::write(&path, content).unwrap();
        path
    }

    fn content(&self, name: &str) -> String {
        std::fs::read_to_string(self.path(name)).unwrap()
    }

    async fn run(&self, name: &str, input: Value) -> Result<ToolResult, ToolError> {
        let prepared = self
            .tools
            .prepare(
                &tickloop_ids::IdContext::new(),
                &self.workspace(),
                name,
                &input,
            )
            .await?;
        self.tools.execute(prepared, CancellationToken::new()).await
    }

    async fn read(&self, input: Value) {
        let result = self.run("Read", input).await.unwrap();
        assert!(!result.is_error, "{}", result.content);
    }

    async fn write(&self, name: &str, content: &str) -> Result<ToolResult, ToolError> {
        self.run("Write", json!({"file_path": name, "content": content}))
            .await
    }

    async fn edit(&self, input: Value) -> Result<ToolResult, ToolError> {
        self.run("Edit", input).await
    }

    fn has_reviews(&self) -> bool {
        self.output.path().join("chat-output/file-changes").exists()
    }
}

fn rejection(result: Result<ToolResult, ToolError>) -> String {
    match result {
        Err(ToolError::Rejected(message)) => message,
        other => panic!("expected a Claude Code rejection, got {other:?}"),
    }
}

/// Moves the file's modification time forward without touching its content, like a formatter or
/// sync tool that rewrites identical bytes.
fn touch_later(path: &Path) {
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(SystemTime::now() + Duration::from_secs(30))
        .unwrap();
}

#[tokio::test]
async fn write_and_edit_are_the_only_file_changing_tools_the_model_sees() {
    let definitions = Tools::definitions();
    for retired in ["create_file", "write_file", "edit_file"] {
        assert!(
            definitions
                .iter()
                .all(|definition| definition.name != retired),
            "{retired} is still registered"
        );
    }
    let property_names = |name: &str| {
        let definition = definitions
            .iter()
            .find(|definition| definition.name == name)
            .unwrap_or_else(|| panic!("{name} is registered"));
        assert_eq!(
            definition.input_schema["additionalProperties"],
            json!(false)
        );
        let mut names = definition.input_schema["properties"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        names.sort();
        (definition.clone(), names)
    };
    let (write, write_properties) = property_names("Write");
    assert_eq!(write_properties, ["content", "file_path"]);
    assert_eq!(
        write.input_schema["required"],
        json!(["file_path", "content"])
    );
    assert!(
        write
            .description
            .starts_with("Writes a file to the local filesystem.")
    );
    assert!(write.description.contains(
        "If this is an existing file, you MUST use the Read tool first to read the file's contents."
    ));
    let (edit, edit_properties) = property_names("Edit");
    assert_eq!(
        edit_properties,
        ["file_path", "new_string", "old_string", "replace_all"]
    );
    assert_eq!(
        edit.input_schema["required"],
        json!(["file_path", "old_string", "new_string"])
    );
    assert!(
        edit.description
            .starts_with("Performs exact string replacements in files.")
    );

    let chat = Chat::new();
    let error = chat
        .run("write_file", json!({"path":"notes.txt","content":"x"}))
        .await
        .unwrap_err();
    assert!(matches!(error, ToolError::UnknownTool(_)));
    for (name, input) in [
        ("Write", json!({"path":"notes.txt","content":"x"})),
        (
            "Edit",
            json!({"file_path":"notes.txt","old_text":"x","new_text":"y"}),
        ),
    ] {
        let error = chat.run(name, input).await.unwrap_err();
        assert!(matches!(error, ToolError::InvalidInput(_)), "{error}");
    }
}

#[tokio::test]
async fn an_existing_file_must_be_read_before_it_is_overwritten_or_edited() {
    let chat = Chat::new();
    chat.file("notes.txt", "Delivery Monday\n");

    assert_eq!(
        rejection(chat.write("notes.txt", "replaced").await),
        NOT_READ
    );
    assert_eq!(
        rejection(
            chat.edit(json!({"file_path":"notes.txt","old_string":"Monday","new_string":"Friday"}))
                .await
        ),
        NOT_READ
    );
    assert_eq!(chat.content("notes.txt"), "Delivery Monday\n");
    assert!(
        !chat.has_reviews(),
        "a rejected call must not prepare a review"
    );

    chat.read(json!({"file_path":"notes.txt"})).await;
    chat.write("notes.txt", "Delivery Friday\n").await.unwrap();
    assert_eq!(chat.content("notes.txt"), "Delivery Friday\n");
}

#[tokio::test]
async fn a_partial_read_is_enough_to_change_an_unmodified_file() {
    let chat = Chat::new();
    chat.file("notes.txt", "first\nsecond\nthird\n");
    chat.read(json!({"file_path":"notes.txt","offset":2,"limit":1}))
        .await;

    chat.edit(json!({"file_path":"notes.txt","old_string":"third","new_string":"last"}))
        .await
        .unwrap();
    assert_eq!(chat.content("notes.txt"), "first\nsecond\nlast\n");
}

#[tokio::test]
async fn a_successful_write_or_edit_records_the_new_content_as_a_full_read() {
    let chat = Chat::new();
    let path = chat.file("notes.txt", "one\r\ntwo\r\n");
    chat.read(json!({"file_path":"notes.txt","limit":1})).await;

    chat.edit(json!({"file_path":"notes.txt","old_string":"two","new_string":"deux"}))
        .await
        .unwrap();
    let record = chat.tools.file_reads().get(&path).unwrap();
    assert_eq!(record.content, "one\ndeux\n");
    assert!(record.is_full_read);
    assert_eq!(record.offset, 1);
    assert_eq!(record.limit, None);
    assert_eq!(
        record.modified,
        std::fs::metadata(&path).unwrap().modified().unwrap()
    );

    chat.write("notes.txt", "rewritten\n").await.unwrap();
    let record = chat.tools.file_reads().get(&path).unwrap();
    assert_eq!(record.content, "rewritten\n");
    assert!(record.is_full_read);
}

#[tokio::test]
async fn consecutive_changes_need_no_new_read() {
    let chat = Chat::new();
    chat.file("notes.txt", "alpha beta\n");
    chat.read(json!({"file_path":"notes.txt"})).await;

    chat.edit(json!({"file_path":"notes.txt","old_string":"alpha","new_string":"gamma"}))
        .await
        .unwrap();
    chat.edit(json!({"file_path":"notes.txt","old_string":"beta","new_string":"delta"}))
        .await
        .unwrap();
    assert_eq!(chat.content("notes.txt"), "gamma delta\n");
    chat.write("notes.txt", "rewritten\n").await.unwrap();
    chat.edit(json!({"file_path":"notes.txt","old_string":"rewritten","new_string":"final"}))
        .await
        .unwrap();
    assert_eq!(chat.content("notes.txt"), "final\n");

    chat.write("created.txt", "new\n").await.unwrap();
    chat.edit(json!({"file_path":"created.txt","old_string":"new","new_string":"edited"}))
        .await
        .unwrap();
    assert_eq!(chat.content("created.txt"), "edited\n");
}

#[tokio::test]
async fn a_file_changed_after_it_was_read_must_be_read_again() {
    let chat = Chat::new();
    let path = chat.file("notes.txt", "Delivery Monday\n");
    chat.read(json!({"file_path":"notes.txt"})).await;
    std::fs::write(&path, "A human changed Monday\n").unwrap();
    touch_later(&path);

    assert_eq!(
        rejection(chat.write("notes.txt", "replaced").await),
        MODIFIED_SINCE_READ
    );
    assert_eq!(
        rejection(
            chat.edit(json!({"file_path":"notes.txt","old_string":"Monday","new_string":"Friday"}))
                .await
        ),
        MODIFIED_SINCE_READ
    );
    assert_eq!(chat.content("notes.txt"), "A human changed Monday\n");

    chat.read(json!({"file_path":"notes.txt"})).await;
    chat.edit(json!({"file_path":"notes.txt","old_string":"Monday","new_string":"Friday"}))
        .await
        .unwrap();
    assert_eq!(chat.content("notes.txt"), "A human changed Friday\n");
}

#[tokio::test]
async fn edit_trusts_a_full_read_whose_content_is_unchanged_despite_a_newer_timestamp() {
    let chat = Chat::new();
    let path = chat.file("notes.txt", "Delivery Monday\n");
    chat.read(json!({"file_path":"notes.txt"})).await;
    touch_later(&path);

    assert_eq!(
        rejection(chat.write("notes.txt", "replaced").await),
        MODIFIED_SINCE_READ,
        "Write compares only timestamps, like Claude Code's FileWriteTool validation"
    );
    chat.edit(json!({"file_path":"notes.txt","old_string":"Monday","new_string":"Friday"}))
        .await
        .unwrap();
    assert_eq!(chat.content("notes.txt"), "Delivery Friday\n");

    let partial = chat.file("partial.txt", "first\nsecond\n");
    chat.read(json!({"file_path":"partial.txt","limit":1}))
        .await;
    touch_later(&partial);
    assert_eq!(
        rejection(
            chat.edit(json!({"file_path":"partial.txt","old_string":"first","new_string":"one"}))
                .await
        ),
        MODIFIED_SINCE_READ
    );
}

#[tokio::test]
async fn new_files_get_their_missing_parent_directories() {
    let chat = Chat::new();

    let prepared = chat
        .tools
        .prepare(
            &tickloop_ids::IdContext::new(),
            &chat.workspace(),
            "Write",
            &json!({"file_path":"reports/2026/q3.md","content":"# Q3\n"}),
        )
        .await
        .unwrap();
    assert_eq!(
        prepared.input["file_path"],
        json!(chat.path("reports/2026/q3.md"))
    );
    assert_eq!(
        prepared.file_change.as_ref().unwrap().summary.action,
        FileChangeAction::Create
    );
    assert!(
        !chat.path("reports").exists(),
        "preparation changes nothing"
    );
    chat.tools
        .execute(prepared, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(chat.content("reports/2026/q3.md"), "# Q3\n");

    chat.edit(
        json!({"file_path":"drafts/new.txt","old_string":"","new_string":"created by edit\n"}),
    )
    .await
    .unwrap();
    assert_eq!(chat.content("drafts/new.txt"), "created by edit\n");

    let error = chat.write("missing/../escape.txt", "x").await.unwrap_err();
    assert!(matches!(error, ToolError::InvalidInput(_)), "{error}");
}

#[tokio::test]
async fn edit_rejections_use_claude_code_messages() {
    let chat = Chat::new();
    chat.file("notes.txt", "old middle old\n");
    chat.file("empty.txt", "");

    assert_eq!(
        rejection(
            chat.edit(json!({"file_path":"notes.txt","old_string":"same","new_string":"same"}))
                .await
        ),
        "No changes to make: old_string and new_string are exactly the same."
    );
    assert_eq!(
        rejection(
            chat.edit(json!({"file_path":"notes.txt","old_string":"","new_string":"content"}))
                .await
        ),
        "Cannot create new file - file already exists."
    );
    assert_eq!(
        rejection(
            chat.edit(json!({"file_path":"absent.txt","old_string":"old","new_string":"new"}))
                .await
        ),
        format!(
            "File does not exist. Note: your current working directory is {}.",
            chat.directory.path().display()
        )
    );

    chat.read(json!({"file_path":"notes.txt"})).await;
    assert_eq!(
        rejection(
            chat.edit(json!({"file_path":"notes.txt","old_string":"missing","new_string":"new"}))
                .await
        ),
        "String to replace not found in file.\nString: missing"
    );
    assert_eq!(
        rejection(
            chat.edit(json!({"file_path":"notes.txt","old_string":"old","new_string":"new"}))
                .await
        ),
        "Found 2 matches of the string to replace, but replace_all is false. To replace all occurrences, set replace_all to true. To replace only one occurrence, please provide more context to uniquely identify the instance.\nString: old"
    );
    chat.edit(
        json!({"file_path":"notes.txt","old_string":"old","new_string":"new","replace_all":"true"}),
    )
    .await
    .unwrap();
    assert_eq!(chat.content("notes.txt"), "new middle new\n");

    chat.read(json!({"file_path":"empty.txt"})).await;
    chat.edit(json!({"file_path":"empty.txt","old_string":"","new_string":"filled\n"}))
        .await
        .unwrap();
    assert_eq!(chat.content("empty.txt"), "filled\n");
}

#[tokio::test]
async fn edit_matches_straight_quotes_against_curly_ones_and_keeps_the_file_style() {
    let chat = Chat::new();
    chat.file(
        "story.txt",
        "She said \u{201c}hello\u{201d} and it\u{2019}s fine.\n",
    );
    chat.read(json!({"file_path":"story.txt"})).await;

    chat.edit(json!({"file_path":"story.txt","old_string":"said \"hello\"","new_string":"said \"goodbye\""}))
        .await
        .unwrap();
    chat.edit(
        json!({"file_path":"story.txt","old_string":"it's fine","new_string":"it's 'really' great"}),
    )
    .await
    .unwrap();
    assert_eq!(
        chat.content("story.txt"),
        "She said \u{201c}goodbye\u{201d} and it\u{2019}s \u{2018}really\u{2019} great.\n"
    );
}

#[tokio::test]
async fn edit_keeps_crlf_line_endings_and_removes_the_line_break_of_deleted_text() {
    let chat = Chat::new();
    chat.file("windows.txt", "one\r\ntwo\r\nthree\r\n");
    chat.file("unix.txt", "keep\ndrop\nkeep too\n");
    chat.read(json!({"file_path":"windows.txt"})).await;
    chat.read(json!({"file_path":"unix.txt"})).await;

    chat.edit(json!({"file_path":"windows.txt","old_string":"one\ntwo","new_string":"uno\ndos"}))
        .await
        .unwrap();
    assert_eq!(chat.content("windows.txt"), "uno\r\ndos\r\nthree\r\n");
    chat.edit(json!({"file_path":"unix.txt","old_string":"drop","new_string":""}))
        .await
        .unwrap();
    assert_eq!(chat.content("unix.txt"), "keep\nkeep too\n");
}

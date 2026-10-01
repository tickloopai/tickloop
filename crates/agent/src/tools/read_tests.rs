use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::tools::{FileReadRecord, FileReadState, ToolError, ToolResult, Tools, Workspace};

fn workspace(path: &Path) -> Workspace {
    Workspace {
        workdir: path.to_path_buf(),
        additional_directories: Vec::new(),
    }
}

fn chat_tools(output: &tempfile::TempDir) -> Tools {
    Tools::new(output.path().join("chat-output"))
}

async fn read(tools: &Tools, directory: &Path, input: Value) -> Result<ToolResult, ToolError> {
    let prepared = tools.prepare(&workspace(directory), "Read", &input).await?;
    tools.execute(prepared, CancellationToken::new()).await
}

async fn read_text(directory: &Path, content: &[u8], input: Value) -> Result<String, String> {
    std::fs::write(directory.join("notes.txt"), content).unwrap();
    let output = tempfile::tempdir().unwrap();
    read(&chat_tools(&output), directory, input)
        .await
        .map(|result| {
            assert!(!result.is_error);
            assert!(result.output_ref.is_none());
            result.content
        })
        .map_err(|error| error.to_string())
}

fn canonical(directory: &Path, name: &str) -> PathBuf {
    directory.canonicalize().unwrap().join(name)
}

#[tokio::test]
async fn read_is_the_only_file_reading_tool_the_model_sees() {
    let definitions = Tools::definitions();
    assert!(
        definitions
            .iter()
            .all(|definition| definition.name != "read_file")
    );
    let read = definitions
        .iter()
        .find(|definition| definition.name == "Read")
        .expect("Read is registered");
    assert_eq!(read.input_schema["required"], json!(["file_path"]));
    assert_eq!(read.input_schema["additionalProperties"], json!(false));
    let mut properties = read.input_schema["properties"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    properties.sort();
    assert_eq!(properties, ["file_path", "limit", "offset"]);
    assert!(read.description.starts_with(
        "Reads a file from the local filesystem. You can access any file directly by using this tool."
    ));
    assert!(
        read.description.contains(
            "- Results are returned using cat -n format, with line numbers starting at 1"
        )
    );
    assert!(
        read.description
            .contains("- This tool allows you to read images")
    );

    let directory = tempfile::tempdir().unwrap();
    let output = tempfile::tempdir().unwrap();
    let error = chat_tools(&output)
        .prepare(
            &workspace(directory.path()),
            "read_file",
            &json!({"path":"notes.txt"}),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, ToolError::UnknownTool(_)), "{error}");
}

#[tokio::test]
async fn lines_are_numbered_like_claude_code_and_crlf_splits_lines() {
    let directory = tempfile::tempdir().unwrap();
    assert_eq!(
        read_text(
            directory.path(),
            b"alpha\r\nbeta\n",
            json!({"file_path":"notes.txt"})
        )
        .await
        .unwrap(),
        "     1\u{2192}alpha\n     2\u{2192}beta\n     3\u{2192}"
    );
    assert_eq!(
        read_text(
            directory.path(),
            "\u{feff}first\nsecond".as_bytes(),
            json!({"file_path":"notes.txt"})
        )
        .await
        .unwrap(),
        "     1\u{2192}first\n     2\u{2192}second"
    );
}

#[tokio::test]
async fn offset_and_limit_select_one_based_lines() {
    let directory = tempfile::tempdir().unwrap();
    let file = b"one\ntwo\nthree\nfour\nfive";
    assert_eq!(
        read_text(
            directory.path(),
            file,
            json!({"file_path":"notes.txt","offset":2,"limit":2})
        )
        .await
        .unwrap(),
        "     2\u{2192}two\n     3\u{2192}three"
    );
    assert_eq!(
        read_text(
            directory.path(),
            file,
            json!({"file_path":"notes.txt","offset":"4","limit":"1"})
        )
        .await
        .unwrap(),
        "     4\u{2192}four"
    );
    assert_eq!(
        read_text(
            directory.path(),
            file,
            json!({"file_path":"notes.txt","offset":0,"limit":1})
        )
        .await
        .unwrap(),
        "     1\u{2192}one"
    );
}

#[tokio::test]
async fn line_numbers_stop_padding_at_six_digits() {
    let directory = tempfile::tempdir().unwrap();
    let file = format!("{}last", "\n".repeat(100_000));
    assert_eq!(
        read_text(
            directory.path(),
            file.as_bytes(),
            json!({"file_path":"notes.txt","offset":99_999,"limit":3})
        )
        .await
        .unwrap(),
        " 99999\u{2192}\n100000\u{2192}\n100001\u{2192}last"
    );
}

#[tokio::test]
async fn without_a_limit_read_returns_the_first_2000_lines() {
    let directory = tempfile::tempdir().unwrap();
    let file = (1..=2500)
        .map(|number| format!("line {number}\n"))
        .collect::<String>();
    let text = read_text(
        directory.path(),
        file.as_bytes(),
        json!({"file_path":"notes.txt"}),
    )
    .await
    .unwrap();
    assert_eq!(text.lines().count(), 2000);
    assert_eq!(text.lines().last(), Some("  2000\u{2192}line 2000"));
}

#[tokio::test]
async fn a_file_over_256_kb_needs_an_explicit_limit() {
    let directory = tempfile::tempdir().unwrap();
    let file = format!("{}\n", "x".repeat(99)).repeat(3072);
    assert_eq!(
        read_text(
            directory.path(),
            file.as_bytes(),
            json!({"file_path":"notes.txt"})
        )
        .await
        .unwrap_err(),
        "File content (300KB) exceeds maximum allowed size (256KB). Use offset and limit parameters to read specific portions of the file, or search for specific content instead of reading the whole file."
    );
    let text = read_text(
        directory.path(),
        file.as_bytes(),
        json!({"file_path":"notes.txt","offset":3000,"limit":10}),
    )
    .await
    .unwrap();
    assert_eq!(text.lines().count(), 10);
    assert!(text.starts_with("  3000\u{2192}xxx"));
}

#[tokio::test]
async fn output_above_the_token_budget_is_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let file = format!("{}\n", "y".repeat(199)).repeat(1000);
    assert_eq!(
        read_text(
            directory.path(),
            file.as_bytes(),
            json!({"file_path":"notes.txt"})
        )
        .await
        .unwrap_err(),
        "File content (50000 tokens) exceeds maximum allowed tokens (25000). Use offset and limit parameters to read specific portions of the file, or search for specific content instead of reading the whole file."
    );
    let text = read_text(
        directory.path(),
        file.as_bytes(),
        json!({"file_path":"notes.txt","limit":400}),
    )
    .await
    .unwrap();
    assert_eq!(text.lines().count(), 400);

    let single_line = "z".repeat(1024 * 1024);
    let error = read_text(
        directory.path(),
        single_line.as_bytes(),
        json!({"file_path":"notes.txt","limit":1}),
    )
    .await
    .unwrap_err();
    assert!(
        error.contains("exceeds maximum allowed tokens (25000)"),
        "{error}"
    );
}

#[tokio::test]
async fn empty_and_short_files_return_claude_code_reminders() {
    let directory = tempfile::tempdir().unwrap();
    assert_eq!(
        read_text(directory.path(), b"", json!({"file_path":"notes.txt"}))
            .await
            .unwrap(),
        "<system-reminder>Warning: the file exists but the contents are empty.</system-reminder>"
    );
    assert_eq!(
        read_text(
            directory.path(),
            b"a\nb",
            json!({"file_path":"notes.txt","offset":5})
        )
        .await
        .unwrap(),
        "<system-reminder>Warning: the file exists but is shorter than the provided offset (5). The file has 2 lines.</system-reminder>"
    );
}

#[tokio::test]
async fn binary_non_utf8_and_directory_reads_fail_explicitly() {
    let directory = tempfile::tempdir().unwrap();
    let binary = read_text(
        directory.path(),
        b"abc\0def",
        json!({"file_path":"notes.txt"}),
    )
    .await
    .unwrap_err();
    assert!(binary.contains("cannot read binary files"), "{binary}");
    let latin1 = read_text(
        directory.path(),
        &[0xff, 0xfe, b'a'],
        json!({"file_path":"notes.txt"}),
    )
    .await
    .unwrap_err();
    assert!(latin1.contains("UTF-8"), "{latin1}");

    std::fs::create_dir(directory.path().join("folder")).unwrap();
    let output = tempfile::tempdir().unwrap();
    let folder = read(
        &chat_tools(&output),
        directory.path(),
        json!({"file_path":"folder"}),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(
        folder.starts_with("EISDIR: illegal operation on a directory"),
        "{folder}"
    );
}

#[tokio::test]
async fn read_inputs_follow_the_claude_code_schema() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("notes.txt"), "text").unwrap();
    let output = tempfile::tempdir().unwrap();
    let tools = chat_tools(&output);
    for input in [
        json!({"path":"notes.txt"}),
        json!({"file_path":"notes.txt","path":"notes.txt"}),
        json!({"file_path":"notes.txt","limit":0}),
        json!({"file_path":"notes.txt","offset":-1}),
        json!({"file_path":"notes.txt","limit":"many"}),
    ] {
        let error = tools
            .prepare(&workspace(directory.path()), "Read", &input)
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::InvalidInput(_)), "{input}");
    }
    let prepared = tools
        .prepare(
            &workspace(directory.path()),
            "Read",
            &json!({"file_path":"notes.txt","limit":5}),
        )
        .await
        .unwrap();
    assert_eq!(
        prepared.input,
        json!({"file_path":canonical(directory.path(), "notes.txt"),"limit":5})
    );
    assert!(prepared.is_read_only);
    assert!(!prepared.requires_permission);
}

#[tokio::test]
async fn a_successful_read_records_what_the_chat_saw() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("notes.txt");
    std::fs::write(&path, "one\r\ntwo\n").unwrap();
    std::os::unix::fs::symlink(&path, directory.path().join("link.txt")).unwrap();
    let output = tempfile::tempdir().unwrap();
    let tools = chat_tools(&output);
    let key = canonical(directory.path(), "notes.txt");
    assert!(tools.file_reads().get(&key).is_none());

    read(&tools, directory.path(), json!({"file_path":"link.txt"}))
        .await
        .unwrap();
    assert_eq!(
        tools.file_reads().get(&key),
        Some(FileReadRecord {
            content: "one\ntwo\n".into(),
            modified: std::fs::metadata(&path).unwrap().modified().unwrap(),
            offset: 1,
            limit: None,
            is_full_read: true,
        })
    );

    read(
        &tools,
        directory.path(),
        json!({"file_path":path,"offset":2,"limit":1}),
    )
    .await
    .unwrap();
    let partial = tools.file_reads().get(&key).unwrap();
    assert_eq!(partial.content, "two");
    assert_eq!((partial.offset, partial.limit), (2, Some(1)));
    assert!(!partial.is_full_read);

    let other_output = tempfile::tempdir().unwrap();
    assert!(chat_tools(&other_output).file_reads().get(&key).is_none());
}

#[tokio::test]
async fn a_read_limited_to_part_of_the_file_is_not_a_full_read() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("notes.txt");
    std::fs::write(&path, "one\ntwo\n").unwrap();
    let output = tempfile::tempdir().unwrap();
    let tools = chat_tools(&output);
    read(
        &tools,
        directory.path(),
        json!({"file_path":"notes.txt","limit":2}),
    )
    .await
    .unwrap();
    let record = tools
        .file_reads()
        .get(&canonical(directory.path(), "notes.txt"))
        .unwrap();
    assert_eq!(record.content, "one\ntwo");
    assert!(!record.is_full_read);
}

#[tokio::test]
async fn a_failed_read_records_nothing() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("notes.txt"), b"abc\0def").unwrap();
    let output = tempfile::tempdir().unwrap();
    let tools = chat_tools(&output);
    read(&tools, directory.path(), json!({"file_path":"notes.txt"}))
        .await
        .unwrap_err();
    assert!(
        tools
            .file_reads()
            .get(&canonical(directory.path(), "notes.txt"))
            .is_none()
    );
}

#[test]
fn recorded_reads_keep_the_most_recently_used_hundred_files() {
    let state = FileReadState::default();
    let record = |content: &str| FileReadRecord {
        content: content.into(),
        modified: std::time::SystemTime::UNIX_EPOCH,
        offset: 1,
        limit: None,
        is_full_read: true,
    };
    for index in 0..100 {
        state.record(PathBuf::from(format!("/work/{index}.txt")), record("x"));
    }
    assert!(state.get(Path::new("/work/0.txt")).is_some());
    state.record(PathBuf::from("/work/100.txt"), record("x"));
    assert!(state.get(Path::new("/work/0.txt")).is_some());
    assert!(state.get(Path::new("/work/1.txt")).is_none());
    assert!(state.get(Path::new("/work/100.txt")).is_some());
}

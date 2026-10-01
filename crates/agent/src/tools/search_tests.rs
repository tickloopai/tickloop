use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::tools::{GLOB_TOOL_NAME, GREP_TOOL_NAME, ToolError, Tools, Workspace};

fn workspace(path: &Path) -> Workspace {
    Workspace {
        workdir: path.to_path_buf(),
        additional_directories: Vec::new(),
    }
}

async fn run(directory: &Path, name: &str, input: Value) -> Result<String, ToolError> {
    let output = tempfile::tempdir().unwrap();
    let tools = Tools::new(output.path().join("chat-output"));
    let prepared = tools.prepare(&workspace(directory), name, &input).await?;
    let result = tools.execute(prepared, CancellationToken::new()).await?;
    assert!(!result.is_error);
    assert!(result.output_ref.is_none());
    Ok(result.content)
}

async fn glob(directory: &Path, input: Value) -> String {
    run(directory, GLOB_TOOL_NAME, input)
        .await
        .unwrap_or_else(|error| panic!("Glob failed: {error}"))
}

async fn grep(directory: &Path, input: Value) -> String {
    run(directory, GREP_TOOL_NAME, input)
        .await
        .unwrap_or_else(|error| panic!("Grep failed: {error}"))
}

fn write(directory: &Path, name: &str, content: &str) -> PathBuf {
    let path = directory.join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, content).unwrap();
    path
}

/// Gives each file a distinct modification time, the first one oldest.
fn age_in_order(paths: &[&Path]) {
    let start = SystemTime::now() - Duration::from_secs(3600);
    for (index, path) in paths.iter().enumerate() {
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(start + Duration::from_secs(60 * index as u64))
            .unwrap();
    }
}

fn canonical(directory: &Path, name: &str) -> String {
    directory
        .canonicalize()
        .unwrap()
        .join(name)
        .display()
        .to_string()
}

/// A Git working tree, so ripgrep's rules apply its `.gitignore`.
fn repository() -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    write(directory.path(), ".git/HEAD", "ref: refs/heads/main\n");
    write(directory.path(), ".gitignore", "ignored*\n");
    directory
}

#[tokio::test]
async fn glob_and_grep_replace_the_lowercase_search_tools() {
    let definitions = Tools::definitions();
    assert!(
        definitions
            .iter()
            .all(|definition| definition.name != "glob" && definition.name != "grep")
    );
    let properties = |name: &str| {
        let definition = definitions
            .iter()
            .find(|definition| definition.name == name)
            .unwrap_or_else(|| panic!("{name} is registered"));
        assert_eq!(definition.input_schema["required"], json!(["pattern"]));
        assert_eq!(
            definition.input_schema["additionalProperties"],
            json!(false)
        );
        let mut properties = definition.input_schema["properties"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        properties.sort();
        (definition.description.clone(), properties)
    };
    let (glob_description, glob_properties) = properties("Glob");
    assert_eq!(glob_properties, ["path", "pattern"]);
    assert!(glob_description.starts_with("- Fast file pattern matching tool"));
    let (grep_description, grep_properties) = properties("Grep");
    assert_eq!(
        grep_properties,
        [
            "-A",
            "-B",
            "-C",
            "-i",
            "-n",
            "context",
            "glob",
            "head_limit",
            "multiline",
            "offset",
            "output_mode",
            "path",
            "pattern",
            "type"
        ]
    );
    assert!(grep_description.starts_with("A powerful search tool built on ripgrep"));
    assert!(!grep_description.contains("Agent"));

    let directory = tempfile::tempdir().unwrap();
    for old in ["glob", "grep"] {
        let error = run(directory.path(), old, json!({"path":".","pattern":"x"}))
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::UnknownTool(_)), "{error}");
    }
    let error = run(
        directory.path(),
        GLOB_TOOL_NAME,
        json!({"pattern":"*","glob":"*.rs"}),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, ToolError::InvalidInput(_)), "{error}");
}

#[tokio::test]
async fn glob_lists_ignored_and_hidden_files_oldest_first_relative_to_the_workdir() {
    let directory = repository();
    let root = directory.path();
    let newest = write(root, "src/main.rs", "");
    let ignored = write(root, "ignored.rs", "");
    let hidden = write(root, ".config/hidden.rs", "");
    let oldest = write(root, "lib.rs", "");
    write(root, ".git/hooks/check.rs", "");
    write(root, ".tickloop/state.rs", "");
    write(root, "service-secret.rs", "");
    write(root, "notes.txt", "");
    std::os::unix::fs::symlink(&oldest, root.join("link.rs")).unwrap();
    age_in_order(&[&oldest, &hidden, &ignored, &newest]);

    assert_eq!(
        glob(root, json!({"pattern":"**/*.rs"})).await,
        "lib.rs\n.config/hidden.rs\nignored.rs\nsrc/main.rs"
    );
    assert_eq!(
        glob(root, json!({"pattern":"*.rs","path":"src"})).await,
        "src/main.rs"
    );
    assert_eq!(
        glob(root, json!({"pattern":"**/*.md"})).await,
        "No files found"
    );
}

#[tokio::test]
async fn glob_accepts_an_absolute_pattern_and_keeps_the_first_hundred_files() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let reports = (0..101)
        .map(|index| write(root, &format!("reports/{index:03}.txt"), ""))
        .collect::<Vec<_>>();
    age_in_order(&reports.iter().map(PathBuf::as_path).collect::<Vec<_>>());
    let pattern = format!("{}/reports/*.txt", root.canonicalize().unwrap().display());

    let listed = glob(root, json!({"pattern": pattern})).await;
    let lines = listed.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 101);
    assert_eq!(lines[0], "reports/000.txt");
    assert_eq!(lines[99], "reports/099.txt");
    assert_eq!(
        lines[100],
        "(Results are truncated. Consider using a more specific path or pattern.)"
    );
}

#[tokio::test]
async fn glob_explains_a_missing_or_non_directory_path() {
    let directory = tempfile::tempdir().unwrap();
    write(directory.path(), "notes.txt", "");
    let missing = run(
        directory.path(),
        GLOB_TOOL_NAME,
        json!({"pattern":"*","path":"missing"}),
    )
    .await
    .unwrap_err();
    assert_eq!(
        missing.to_string(),
        format!(
            "Directory does not exist: missing. Note: your current working directory is {}.",
            directory.path().display()
        )
    );
    let file = run(
        directory.path(),
        GLOB_TOOL_NAME,
        json!({"pattern":"*","path":"notes.txt"}),
    )
    .await
    .unwrap_err();
    assert_eq!(file.to_string(), "Path is not a directory: notes.txt");
    let grep_missing = run(
        directory.path(),
        GREP_TOOL_NAME,
        json!({"pattern":"x","path":"missing.txt"}),
    )
    .await
    .unwrap_err();
    assert_eq!(
        grep_missing.to_string(),
        format!(
            "Path does not exist: missing.txt. Note: your current working directory is {}.",
            directory.path().display()
        )
    );
}

#[tokio::test]
async fn grep_lists_matching_files_newest_first_honoring_gitignore_and_skipping_vcs() {
    let directory = repository();
    let root = directory.path();
    let older = write(root, "docs/plan.md", "Delivery on Friday\n");
    let hidden = write(root, ".notes/today.md", "delivery confirmed\n");
    let newer = write(root, "readme.md", "Next delivery\n");
    write(root, "ignored.md", "delivery\n");
    write(root, ".git/COMMIT_EDITMSG", "delivery\n");
    write(root, ".hg/store.md", "delivery\n");
    write(root, ".jj/repo.md", "delivery\n");
    write(root, ".tickloop/chat.md", "delivery\n");
    write(root, "delivery-token.md", "delivery\n");
    write(root, "unrelated.md", "nothing\n");
    age_in_order(&[&older, &hidden, &newer]);

    assert_eq!(
        grep(root, json!({"pattern":"elivery"})).await,
        "Found 3 files\nreadme.md\n.notes/today.md\ndocs/plan.md"
    );
    assert_eq!(
        grep(
            root,
            json!({"pattern":"Delivery","-i":true,"glob":"docs/*.md"})
        )
        .await,
        "Found 1 file\ndocs/plan.md"
    );
    assert_eq!(
        grep(root, json!({"pattern":"absent"})).await,
        "No files found"
    );
}

#[tokio::test]
async fn grep_content_numbers_lines_and_shows_requested_context() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    write(
        root,
        "log.txt",
        "one\ntwo\nmatch A\nthree\nfour\nfive\nsix\nmatch B\nseven\n",
    );
    write(root, "other.txt", "no\nmatch C\n");
    let content = |extra: Value| {
        let mut input = json!({"pattern":"match","output_mode":"content"});
        input
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        input
    };

    assert_eq!(
        grep(root, content(json!({}))).await,
        "log.txt:3:match A\nlog.txt:8:match B\nother.txt:2:match C"
    );
    assert_eq!(
        grep(root, content(json!({"-n":false,"path":"log.txt"}))).await,
        "match A\nmatch B"
    );
    assert_eq!(
        grep(root, content(json!({"-B":1,"-A":"1","path":"log.txt"}))).await,
        "2-two\n3:match A\n4-three\n--\n7-six\n8:match B\n9-seven"
    );
    assert_eq!(
        grep(root, content(json!({"-C":1,"-B":3,"glob":"log.txt"}))).await,
        "log.txt-2-two\nlog.txt:3:match A\nlog.txt-4-three\n--\nlog.txt-7-six\nlog.txt:8:match B\nlog.txt-9-seven"
    );
    assert_eq!(
        grep(root, content(json!({"context":2,"-C":0,"path":"log.txt"}))).await,
        "1-one\n2-two\n3:match A\n4-three\n5-four\n6-five\n7-six\n8:match B\n9-seven"
    );
    assert_eq!(
        grep(root, content(json!({"-A":1}))).await,
        "log.txt:3:match A\nlog.txt-4-three\n--\nlog.txt:8:match B\nlog.txt-9-seven\n--\nother.txt:2:match C"
    );
    assert_eq!(
        grep(root, content(json!({"pattern":"absent"}))).await,
        "No matches found"
    );
}

#[tokio::test]
async fn grep_counts_matching_lines_per_file_with_a_total() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    write(root, "a.txt", "delivery\ndelivery delivery\nnone\n");
    write(root, "b.txt", "delivery\n");
    write(root, "c.txt", "none\n");

    assert_eq!(
        grep(root, json!({"pattern":"delivery","output_mode":"count"})).await,
        "a.txt:2\nb.txt:1\n\nFound 3 total occurrences across 2 files."
    );
    assert_eq!(
        grep(
            root,
            json!({"pattern":"delivery","output_mode":"count","path":"b.txt"})
        )
        .await,
        "b.txt:1\n\nFound 1 total occurrence across 1 file."
    );
    assert_eq!(
        grep(root, json!({"pattern":"absent","output_mode":"count"})).await,
        "No matches found\n\nFound 0 total occurrences across 0 files."
    );
}

#[tokio::test]
async fn grep_filters_by_file_type_case_and_multiline_patterns() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let rust = write(root, "main.rs", "struct Order {\n    id: u32,\n}\n");
    let python = write(root, "script.py", "class Order:\n    id = 1\n");
    let view = write(root, "view.tsx", "ORDER\n");
    age_in_order(&[&view, &python, &rust]);

    assert_eq!(
        grep(root, json!({"pattern":"Order","type":"rust"})).await,
        "Found 1 file\nmain.rs"
    );
    assert_eq!(
        grep(
            root,
            json!({"pattern":"order","-i":"true","glob":"*.{py,tsx}"})
        )
        .await,
        "Found 2 files\nscript.py\nview.tsx"
    );
    assert_eq!(
        grep(
            root,
            json!({"pattern":"order","-i":true,"glob":"*.rs, *.py"})
        )
        .await,
        "Found 2 files\nmain.rs\nscript.py"
    );
    assert_eq!(
        grep(
            root,
            json!({"pattern":"Order \\{.*id","multiline":true,"output_mode":"content"})
        )
        .await,
        "main.rs:1:struct Order {\nmain.rs:2:    id: u32,"
    );
    assert_eq!(
        grep(root, json!({"pattern":"Order \\{.*id"})).await,
        "No files found",
        "without multiline a pattern stays within one line"
    );
    let unknown = run(
        root,
        GREP_TOOL_NAME,
        json!({"pattern":"Order","type":"cobol-ish"}),
    )
    .await
    .unwrap_err();
    assert!(
        unknown.to_string().contains("unrecognized file type"),
        "{unknown}"
    );
    let invalid = run(root, GREP_TOOL_NAME, json!({"pattern":"(unclosed"}))
        .await
        .unwrap_err();
    assert!(matches!(invalid, ToolError::InvalidInput(_)), "{invalid}");
}

#[tokio::test]
async fn grep_pages_results_with_head_limit_and_offset() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let lines = (1..=300)
        .map(|index| format!("item {index}"))
        .collect::<Vec<_>>();
    write(root, "items.txt", &(lines.join("\n") + "\n"));
    let files = (0..260)
        .map(|index| write(root, &format!("many/{index:03}.txt"), "item\n"))
        .collect::<Vec<_>>();
    age_in_order(&files.iter().map(PathBuf::as_path).collect::<Vec<_>>());

    let default = grep(
        root,
        json!({"pattern":"item","path":"items.txt","output_mode":"content"}),
    )
    .await;
    let (listed, note) = default.split_once("\n\n").unwrap();
    assert_eq!(listed.lines().count(), 250);
    assert!(listed.ends_with("250:item 250"));
    assert_eq!(note, "[Showing results with pagination = limit: 250]");

    let unlimited = grep(
        root,
        json!({"pattern":"item","path":"items.txt","output_mode":"content","head_limit":0}),
    )
    .await;
    assert_eq!(unlimited.lines().count(), 300);
    assert!(!unlimited.contains("pagination"));

    assert_eq!(
        grep(
            root,
            json!({"pattern":"item","path":"items.txt","output_mode":"content","head_limit":2,"offset":"10"})
        )
        .await,
        "11:item 11\n12:item 12\n\n[Showing results with pagination = limit: 2, offset: 10]"
    );
    assert_eq!(
        grep(
            root,
            json!({"pattern":"item","path":"items.txt","output_mode":"content","offset":298})
        )
        .await,
        "299:item 299\n300:item 300\n\n[Showing results with pagination = offset: 298]"
    );

    let files_page = grep(root, json!({"pattern":"item","path":"many"})).await;
    let mut lines = files_page.lines();
    assert_eq!(lines.next(), Some("Found 250 files limit: 250"));
    assert_eq!(lines.next(), Some("many/259.txt"));
    assert_eq!(lines.count(), 249);
    assert_eq!(
        grep(
            root,
            json!({"pattern":"item","path":"many","head_limit":1,"offset":259})
        )
        .await,
        "Found 1 file offset: 259\nmany/000.txt"
    );
    assert_eq!(
        grep(
            root,
            json!({"pattern":"item","path":"many","output_mode":"count","head_limit":2})
        )
        .await,
        "many/000.txt:1\nmany/001.txt:1\n\nFound 2 total occurrences across 2 files. with pagination = limit: 2"
    );
}

#[tokio::test]
async fn grep_omits_long_lines_and_stops_at_binary_data() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    write(
        root,
        "minified.js",
        &format!("token {}\nshort token\n", "x".repeat(600)),
    );
    std::fs::write(root.join("image.bin"), b"\0\0token\n").unwrap();

    assert_eq!(
        grep(
            root,
            json!({"pattern":"token","output_mode":"content","path":"minified.js"})
        )
        .await,
        "1:[Omitted long matching line]\n2:short token"
    );
    assert_eq!(
        grep(root, json!({"pattern":"token"})).await,
        "Found 1 file\nminified.js"
    );
}

#[tokio::test]
async fn grep_skips_files_above_eight_mebibytes() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let large = format!("token\n{}", "x".repeat(8 * 1024 * 1024));
    write(root, "large.log", &large);
    write(root, "small.log", "token\n");

    assert_eq!(
        grep(root, json!({"pattern":"token"})).await,
        "Found 1 file\nsmall.log"
    );
    let explicit = run(
        root,
        GREP_TOOL_NAME,
        json!({"pattern":"token","path":"large.log"}),
    )
    .await
    .unwrap_err();
    assert!(explicit.to_string().contains("8 MiB"), "{explicit}");
}

#[tokio::test]
async fn searching_does_not_follow_symbolic_links() {
    let directory = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let root = directory.path();
    write(outside.path(), "linked.txt", "delivery\n");
    std::fs::create_dir(outside.path().join("nested")).unwrap();
    write(outside.path(), "nested/inner.txt", "delivery\n");
    std::os::unix::fs::symlink(outside.path().join("linked.txt"), root.join("file.txt")).unwrap();
    std::os::unix::fs::symlink(outside.path().join("nested"), root.join("nested")).unwrap();
    write(root, "local.txt", "delivery\n");

    assert_eq!(
        grep(root, json!({"pattern":"delivery"})).await,
        "Found 1 file\nlocal.txt"
    );
    assert_eq!(glob(root, json!({"pattern":"**/*.txt"})).await, "local.txt");
}

#[tokio::test]
async fn searches_outside_the_workspace_need_approval_and_inside_run_without_it() {
    let directory = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    write(directory.path(), "notes.txt", "delivery\n");
    write(outside.path(), "notes.txt", "delivery\n");
    let output = tempfile::tempdir().unwrap();
    let tools = Tools::new(output.path().join("chat-output"));
    let workspace = workspace(directory.path());

    for name in [GLOB_TOOL_NAME, GREP_TOOL_NAME] {
        let inside = tools
            .prepare(&workspace, name, &json!({"pattern":"notes"}))
            .await
            .unwrap();
        assert!(inside.is_read_only && inside.in_workspace);
        assert!(!inside.requires_permission);
        assert_eq!(
            inside.input["path"],
            json!(directory.path().canonicalize().unwrap())
        );

        let elsewhere = tools
            .prepare(
                &workspace,
                name,
                &json!({"pattern":"notes","path":outside.path()}),
            )
            .await
            .unwrap();
        assert!(elsewhere.is_read_only && !elsewhere.in_workspace);
        assert!(elsewhere.requires_permission);
    }
    let absolute_pattern = tools
        .prepare(
            &workspace,
            GLOB_TOOL_NAME,
            &json!({"pattern": format!("{}/*.txt", outside.path().display())}),
        )
        .await
        .unwrap();
    assert!(absolute_pattern.requires_permission);
    assert_eq!(
        absolute_pattern.input["path"],
        json!(outside.path().canonicalize().unwrap())
    );

    for (name, pattern, found) in [
        (GLOB_TOOL_NAME, "notes*", ""),
        (GREP_TOOL_NAME, "delivery", "Found 1 file\n"),
    ] {
        let outside_listing = tools
            .prepare(
                &workspace,
                name,
                &json!({"pattern":pattern,"path":outside.path()}),
            )
            .await
            .unwrap();
        let result = tools
            .execute(outside_listing, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            result.content,
            format!("{found}{}", canonical(outside.path(), "notes.txt")),
            "{name} shows paths outside the working directory in full"
        );
    }
}

#[tokio::test]
async fn a_cancelled_search_stops_without_a_result() {
    let directory = tempfile::tempdir().unwrap();
    write(directory.path(), "notes.txt", "delivery\n");
    let output = tempfile::tempdir().unwrap();
    let tools = Tools::new(output.path().join("chat-output"));
    let cancel = CancellationToken::new();
    cancel.cancel();
    for name in [GLOB_TOOL_NAME, GREP_TOOL_NAME] {
        let prepared = tools
            .prepare(
                &workspace(directory.path()),
                name,
                &json!({"pattern":"notes"}),
            )
            .await
            .unwrap();
        let error = tools.execute(prepared, cancel.clone()).await.unwrap_err();
        assert!(matches!(error, ToolError::Cancelled), "{error}");
    }
}

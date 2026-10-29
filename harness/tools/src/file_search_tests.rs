//! The search over files given as bytes prints exactly what the local `Glob` and `Grep` tools
//! print for the same tree.

use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::{
    GLOB_TOOL_NAME, GREP_TOOL_NAME, ToolError, Tools, Workspace,
    file_search::{
        FileSearchError, GLOB_RESULT_LIMIT, GlobQuery, GrepLine, GrepOutput, SearchedFile,
        glob_files, glob_text, grep_files, grep_text,
    },
    search::grep_query,
};

struct SnapshotFile {
    path: String,
    content: Vec<u8>,
    modified: Option<SystemTime>,
}

/// Every regular file below `root`, as an environment that keeps them elsewhere would hold them.
fn snapshot(root: &Path) -> Vec<SnapshotFile> {
    let mut files = Vec::new();
    let mut directories = vec![PathBuf::new()];
    while let Some(relative) = directories.pop() {
        for entry in std::fs::read_dir(root.join(&relative)).unwrap() {
            let entry = entry.unwrap();
            let path = relative.join(entry.file_name());
            let metadata = std::fs::symlink_metadata(entry.path()).unwrap();
            if metadata.is_dir() {
                directories.push(path);
            } else if metadata.is_file() {
                files.push(SnapshotFile {
                    path: path.to_string_lossy().into_owned(),
                    content: std::fs::read(entry.path()).unwrap(),
                    modified: metadata.modified().ok(),
                });
            }
        }
    }
    files
}

fn searched(files: &[SnapshotFile]) -> impl Iterator<Item = SearchedFile<'_>> {
    files.iter().map(|file| SearchedFile {
        path: &file.path,
        content: &file.content,
        modified: file.modified,
    })
}

async fn local(root: &Path, name: &str, input: &Value) -> Result<String, ToolError> {
    let output = tempfile::tempdir().unwrap();
    let tools = Tools::new(output.path().join("chat-output"));
    let workspace = Workspace {
        folder: Some(root.to_path_buf()),
        additional_directories: Vec::new(),
    };
    let prepared = tools
        .prepare(&tickloop_ids::IdContext::new(), &workspace, name, input)
        .await?;
    Ok(tools
        .execute(prepared, CancellationToken::new())
        .await?
        .content)
}

fn base(path: &str) -> &str {
    if path == "." { "" } else { path }
}

async fn assert_same_glob(root: &Path, input: Value) -> String {
    let expected = local(root, GLOB_TOOL_NAME, &input).await.unwrap();
    let files = snapshot(root);
    let query = GlobQuery {
        pattern: input["pattern"].as_str().unwrap().into(),
        limit: GLOB_RESULT_LIMIT,
    };
    let matches = glob_files(
        searched(&files),
        base(input["path"].as_str().unwrap_or(".")),
        &query,
    )
    .unwrap();
    assert_eq!(glob_text(&matches, str::to_owned), expected, "{input}");
    expected
}

async fn assert_same_grep(root: &Path, input: Value) -> String {
    let expected = local(root, GREP_TOOL_NAME, &input).await.unwrap();
    let files = snapshot(root);
    let (path, query, _) = grep_query(&input).unwrap();
    let matches = grep_files(searched(&files), base(&path), &query).unwrap();
    assert_eq!(
        grep_text(&matches, &query, str::to_owned),
        expected,
        "{input}"
    );
    expected
}

fn write(root: &Path, name: &str, content: impl AsRef<[u8]>) -> PathBuf {
    let path = root.join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, content).unwrap();
    path
}

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

fn repository() -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    write(directory.path(), ".git/HEAD", "ref: refs/heads/main\n");
    write(directory.path(), ".gitignore", "ignored*\n");
    directory
}

#[tokio::test]
async fn glob_lists_the_same_files_in_the_same_order() {
    let directory = repository();
    let root = directory.path();
    let newest = write(root, "src/main.rs", "");
    let ignored = write(root, "ignored.rs", "");
    let hidden = write(root, ".config/hidden.rs", "");
    let oldest = write(root, "lib.rs", "");
    write(root, ".git/hooks/check.rs", "");
    write(root, ".tickloop/state.rs", "");
    write(root, "service-secret.rs", "");
    write(root, "a/z.rs", "");
    write(root, "a.b.rs", "");
    std::os::unix::fs::symlink(&oldest, root.join("link.rs")).unwrap();
    age_in_order(&[&oldest, &hidden, &ignored, &newest]);

    assert!(
        assert_same_glob(root, json!({"pattern":"**/*.rs"}))
            .await
            .starts_with("lib.rs\n.config/hidden.rs\nignored.rs\nsrc/main.rs\n")
    );
    assert_same_glob(root, json!({"pattern":"*.rs","path":"src"})).await;
    assert_same_glob(root, json!({"pattern":"**/*.md"})).await;
    assert_same_glob(root, json!({"pattern":"{a,src}/**"})).await;

    let reports = (0..101)
        .map(|index| write(root, &format!("reports/{index:03}.txt"), ""))
        .collect::<Vec<_>>();
    age_in_order(&reports.iter().map(PathBuf::as_path).collect::<Vec<_>>());
    let listed = assert_same_glob(root, json!({"pattern":"*.txt","path":"reports"})).await;
    assert!(
        listed
            .ends_with("(Results are truncated. Consider using a more specific path or pattern.)")
    );
}

#[tokio::test]
async fn grep_lists_counts_and_prints_the_same_lines() {
    let directory = repository();
    let root = directory.path();
    let older = write(root, "docs/plan.md", "Delivery on Friday\n");
    let hidden = write(root, ".notes/today.md", "delivery confirmed\n");
    let newer = write(root, "readme.md", "Next delivery\n");
    write(root, "ignored.md", "delivery\n");
    write(root, ".git/COMMIT_EDITMSG", "delivery\n");
    write(root, ".hg/store.md", "delivery\n");
    write(root, "delivery-token.md", "delivery\n");
    write(
        root,
        "log.txt",
        "one\ntwo\nmatch A\nthree\nfour\nfive\nsix\nmatch B\nseven\n",
    );
    write(root, "other.txt", "no\nmatch C\n");
    write(root, "main.rs", "struct Order {\n    id: u32,\n}\n");
    write(root, "script.py", "class Order:\n    id = 1\n");
    age_in_order(&[&older, &hidden, &newer]);

    for input in [
        json!({"pattern":"elivery"}),
        json!({"pattern":"Delivery","-i":true,"glob":"docs/*.md"}),
        json!({"pattern":"absent"}),
        json!({"pattern":"match","output_mode":"content"}),
        json!({"pattern":"match","output_mode":"content","-n":false,"path":"log.txt"}),
        json!({"pattern":"match","output_mode":"content","-B":1,"-A":"1","path":"log.txt"}),
        json!({"pattern":"match","output_mode":"content","-C":1,"glob":"log.txt"}),
        json!({"pattern":"match","output_mode":"content","context":2,"path":"log.txt"}),
        json!({"pattern":"match","output_mode":"content","-A":1}),
        json!({"pattern":"absent","output_mode":"content"}),
        json!({"pattern":"match","output_mode":"count"}),
        json!({"pattern":"match","output_mode":"count","path":"log.txt"}),
        json!({"pattern":"absent","output_mode":"count"}),
        json!({"pattern":"Order","type":"rust"}),
        json!({"pattern":"order","-i":true,"glob":"*.rs, *.py"}),
        json!({"pattern":"elivery","glob":"!*.md"}),
        json!({"pattern":"Order \\{.*id","multiline":true,"output_mode":"content"}),
        json!({"pattern":"Order \\{[\\s\\S]*?\\}","multiline":true,"output_mode":"count"}),
        json!({"pattern":"Order \\{.*id"}),
    ] {
        assert_same_grep(root, input).await;
    }
}

#[tokio::test]
async fn grep_pages_the_same_way() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let lines = (1..=300)
        .map(|index| format!("item {index}"))
        .collect::<Vec<_>>();
    write(root, "items.txt", lines.join("\n") + "\n");
    let files = (0..260)
        .map(|index| write(root, &format!("many/{index:03}.txt"), "item\n"))
        .collect::<Vec<_>>();
    age_in_order(&files.iter().map(PathBuf::as_path).collect::<Vec<_>>());

    for input in [
        json!({"pattern":"item","path":"items.txt","output_mode":"content"}),
        json!({"pattern":"item","path":"items.txt","output_mode":"content","head_limit":0}),
        json!({"pattern":"item","path":"items.txt","output_mode":"content","head_limit":2,"offset":"10"}),
        json!({"pattern":"item","path":"items.txt","output_mode":"content","offset":298}),
        json!({"pattern":"item","path":"many"}),
        json!({"pattern":"item","path":"many","head_limit":1,"offset":259}),
        json!({"pattern":"item","path":"many","output_mode":"count","head_limit":2}),
        json!({"pattern":"item","output_mode":"content","-C":1,"head_limit":7}),
    ] {
        assert_same_grep(root, input).await;
    }
}

#[tokio::test]
async fn grep_treats_long_lines_line_endings_and_binary_data_the_same() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    write(
        root,
        "minified.js",
        format!(
            "token {}\nshort token\n{}\nafter\n",
            "x".repeat(600),
            "y".repeat(600)
        ),
    );
    write(root, "image.bin", b"\0\0token\n");
    write(root, "windows.txt", "first token\r\n\r\nlast token\r");
    write(root, "blank.txt", "a\n\nb\n");
    let mut late_binary = b"token\n".to_vec();
    late_binary.extend(std::iter::repeat_n(b'a', 200 * 1024));
    late_binary.extend(b"\n\0token\n");
    write(root, "late.bin", &late_binary);

    for input in [
        json!({"pattern":"token","output_mode":"content","path":"minified.js"}),
        json!({"pattern":"short","output_mode":"content","-C":1,"path":"minified.js"}),
        json!({"pattern":"token"}),
        json!({"pattern":"token","output_mode":"content"}),
        json!({"pattern":"token","output_mode":"count"}),
        json!({"pattern":"token","output_mode":"content","path":"image.bin"}),
        json!({"pattern":"token","output_mode":"count","path":"image.bin"}),
        json!({"pattern":"^$","output_mode":"content","-n":false,"path":"windows.txt"}),
        json!({"pattern":"token$","output_mode":"content","path":"windows.txt"}),
        json!({"pattern":"^$","output_mode":"content","-n":false,"path":"blank.txt"}),
        json!({"pattern":"^$","output_mode":"content","path":"blank.txt"}),
    ] {
        assert_same_grep(root, input).await;
    }
    let printed = assert_same_grep(root, json!({"pattern":"token","output_mode":"content"})).await;
    assert!(printed.contains("late.bin: WARNING: stopped searching binary file after match"));
}

#[tokio::test]
async fn grep_honors_ignore_files_with_ripgrep_precedence() {
    let directory = repository();
    let root = directory.path();
    write(root, ".ignore", "kept-by-rg.txt\nfrom-ignore.txt\n");
    write(root, ".rgignore", "!kept-by-rg.txt\n");
    write(root, ".git/info/exclude", "excluded.txt\n");
    write(root, "nested/.gitignore", "*.log\n!keep.log\n");
    write(root, "nested/deeper/.ignore", "!*.log\n");
    write(root, "vendor/.gitignore", "*\n");
    for name in [
        "kept-by-rg.txt",
        "from-ignore.txt",
        "excluded.txt",
        "ignored-root.txt",
        "nested/drop.log",
        "nested/keep.log",
        "nested/deeper/back.log",
        "vendor/lib.txt",
        "plain.txt",
    ] {
        write(root, name, "needle\n");
    }
    let outside_repository = tempfile::tempdir().unwrap();
    write(outside_repository.path(), ".gitignore", "*.txt\n");
    write(outside_repository.path(), "free.txt", "needle\n");
    write(outside_repository.path(), ".ignore", "hidden.md\n");
    write(outside_repository.path(), "hidden.md", "needle\n");

    for input in [
        json!({"pattern":"needle","output_mode":"count"}),
        json!({"pattern":"needle","output_mode":"count","path":"nested"}),
        json!({"pattern":"needle","output_mode":"count","glob":"vendor/*"}),
        json!({"pattern":"needle","output_mode":"count","path":"from-ignore.txt"}),
    ] {
        assert_same_grep(root, input).await;
    }
    assert_eq!(
        assert_same_grep(
            outside_repository.path(),
            json!({"pattern":"needle","output_mode":"count"})
        )
        .await,
        "free.txt:1\n\nFound 1 total occurrence across 1 file."
    );
}

#[test]
fn bases_must_exist_and_glob_needs_a_directory() {
    let files = [SnapshotFile {
        path: "notes.txt".into(),
        content: b"needle\n".to_vec(),
        modified: None,
    }];
    let glob = GlobQuery {
        pattern: "*".into(),
        limit: GLOB_RESULT_LIMIT,
    };
    assert_eq!(
        glob_files(searched(&files), "notes.txt", &glob),
        Err(FileSearchError::NotADirectory("notes.txt".into()))
    );
    assert_eq!(
        glob_files(searched(&files), "missing", &glob),
        Err(FileSearchError::NotFound("missing".into()))
    );
    assert!(
        glob_files(searched(&[]), "", &glob)
            .unwrap()
            .files
            .is_empty()
    );
    let (_, query, _) = grep_query(&json!({"pattern":"needle","output_mode":"content"})).unwrap();
    let matches = grep_files(searched(&files), "notes.txt", &query).unwrap();
    assert!(matches!(
        &matches.output,
        GrepOutput::Content(lines) if matches!(lines.as_slice(), [GrepLine::Match(text)] if text.path.is_none())
    ));
    let large = [SnapshotFile {
        path: "large.log".into(),
        content: vec![b'x'; 8 * 1024 * 1024 + 1],
        modified: None,
    }];
    assert_eq!(
        grep_files(searched(&large), "large.log", &query),
        Err(FileSearchError::TooLarge("large.log".into()))
    );
}

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::{
    FileReadState, READ_TOOL_NAME, ToolError, Tools, Workspace, permissions::PermissionPolicyExt,
};
use tickloop_agent_protocol::{FileReadRecord, ToolResult};

fn workspace(path: &Path) -> Workspace {
    Workspace {
        folder: Some(path.to_path_buf()),
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

#[tokio::test]
async fn family_temp_writes_edits_and_bash_are_allowed_but_other_families_and_links_are_not() {
    use crate::{Tools, Workspace};
    let directory = tempfile::tempdir().unwrap();
    let internal = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let tools = Tools::new(internal.path().canonicalize().unwrap());
    let workspace = Workspace {
        folder: Some(directory.path().canonicalize().unwrap()),
        additional_directories: vec![],
    };
    let path = internal.path().join("scratch.txt");
    for (name, input) in [
        ("Write", json!({"file_path":path,"content":"owned result"})),
        (
            "Bash",
            json!({"command":format!("touch {}", path.display())}),
        ),
        (
            "Bash",
            json!({"command":format!("echo owned > {}", path.display())}),
        ),
    ] {
        let prepared = tools.prepare(&workspace, name, &input).await.unwrap();
        assert!(!prepared.requires_permission, "{name}: {input}");
    }
    std::fs::write(&path, "owned result").unwrap();
    let read = tools
        .prepare(&workspace, "Read", &json!({"file_path":path}))
        .await
        .unwrap();
    assert!(!read.requires_permission);
    tools.execute(read, CancellationToken::new()).await.unwrap();
    let edit = tools
        .prepare(
            &workspace,
            "Edit",
            &json!({"file_path":path,"old_string":"owned","new_string":"shared"}),
        )
        .await
        .unwrap();
    assert!(!edit.requires_permission);
    let outside = other.path().join("other.txt");
    std::fs::write(&outside, "other-session").unwrap();
    let link = internal.path().join("escape");
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    for candidate in [&outside, &link] {
        let prepared = tools
            .prepare(&workspace, "Read", &json!({"file_path":candidate}))
            .await
            .unwrap();
        assert!(prepared.requires_permission, "{}", candidate.display());
        let prepared = tools
            .prepare(
                &workspace,
                "Bash",
                &json!({"command":format!("cat {}",candidate.display())}),
            )
            .await
            .unwrap();
        assert!(prepared.requires_permission, "{}", candidate.display());
    }
}

#[tokio::test]
async fn family_temp_auto_allow_does_not_widen_plan_permission() {
    use tickloop_agent_protocol::{Decision, ExecutionMode, PermissionPolicy};
    let project = tempfile::tempdir().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let tools = Tools::new(root.clone());
    let workspace = workspace(&project.path().canonicalize().unwrap());
    let policy = PermissionPolicy {
        mode: ExecutionMode::Plan,
        ..PermissionPolicy::default()
    };
    for (name, input) in [
        (
            "Write",
            json!({"file_path":root.join("probe"),"content":"Plan must refuse"}),
        ),
        (
            "Bash",
            json!({"command":format!("touch {}",root.join("probe").display())}),
        ),
    ] {
        let prepared = tools.prepare(&workspace, name, &input).await.unwrap();
        assert_eq!(
            policy.decide(&prepared, &workspace, uuid::Uuid::new_v4()),
            Decision::Deny,
            "{name}"
        );
    }
}

/// Canonical workspace, family temp and unrelated directories for Bash `cwd` scope checks.
struct ShellCwdFixture {
    _directories: [tempfile::TempDir; 3],
    project: PathBuf,
    family_temp: PathBuf,
    outside: PathBuf,
    tools: Tools,
}

impl ShellCwdFixture {
    fn new() -> Self {
        let directories = [(); 3].map(|_| tempfile::tempdir().unwrap());
        let [project, family_temp, outside] =
            [0, 1, 2].map(|index| directories[index].path().canonicalize().unwrap());
        Self {
            tools: Tools::new(family_temp.clone()),
            _directories: directories,
            project,
            family_temp,
            outside,
        }
    }

    fn touch_family_temp_probe(&self) -> String {
        format!("touch {}", self.family_temp.join("probe").display())
    }

    async fn prepare_bash(
        &self,
        workspace: &Workspace,
        command: &str,
        cwd: &Path,
    ) -> crate::PreparedTool {
        self.tools
            .prepare(workspace, "Bash", &json!({"command":command,"cwd":cwd}))
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn family_temp_auto_allow_needs_a_cwd_inside_the_workspace_or_family_temp() {
    use tickloop_agent_protocol::{Decision, ExecutionMode, PermissionPolicy};
    let fixture = ShellCwdFixture::new();
    std::fs::create_dir(fixture.project.join("src")).unwrap();
    std::fs::create_dir(fixture.family_temp.join("work")).unwrap();
    let workspace = workspace(&fixture.project);
    let policy = PermissionPolicy {
        mode: ExecutionMode::AcceptEdits,
        ..PermissionPolicy::default()
    };
    let probe = fixture.touch_family_temp_probe();
    let outside_through_parent = Path::new("..").join(fixture.outside.file_name().unwrap());
    for cwd in [fixture.outside.as_path(), outside_through_parent.as_path()] {
        for command in ["git status", "pwd", "git log", probe.as_str()] {
            let prepared = fixture.prepare_bash(&workspace, command, cwd).await;
            assert!(
                prepared.requires_permission,
                "{command} in {}",
                cwd.display()
            );
            assert_eq!(
                policy.decide(&prepared, &workspace, uuid::Uuid::new_v4()),
                Decision::Ask,
                "{command} in {}",
                cwd.display()
            );
        }
    }
    for cwd in [
        fixture.project.clone(),
        fixture.project.join("src"),
        fixture.family_temp.clone(),
        fixture.family_temp.join("work"),
    ] {
        for command in ["git status", "pwd", probe.as_str()] {
            let prepared = fixture.prepare_bash(&workspace, command, &cwd).await;
            assert!(
                !prepared.requires_permission,
                "{command} in {}",
                cwd.display()
            );
        }
    }
}

#[tokio::test]
async fn family_temp_auto_allow_resolves_cwd_links_before_checking_their_scope() {
    let fixture = ShellCwdFixture::new();
    let workspace = workspace(&fixture.project);
    std::os::unix::fs::symlink(&fixture.outside, fixture.project.join("escape")).unwrap();
    std::os::unix::fs::symlink(&fixture.outside, fixture.family_temp.join("escape")).unwrap();
    std::os::unix::fs::symlink(&fixture.project, fixture.family_temp.join("project")).unwrap();
    for cwd in [
        fixture.project.join("escape"),
        PathBuf::from("escape"),
        fixture.family_temp.join("escape"),
    ] {
        for command in ["git status", "pwd"] {
            let prepared = fixture.prepare_bash(&workspace, command, &cwd).await;
            assert!(
                prepared.requires_permission,
                "{command} in {}",
                cwd.display()
            );
        }
    }
    let prepared = fixture
        .prepare_bash(
            &workspace,
            &fixture.touch_family_temp_probe(),
            &fixture.family_temp.join("project"),
        )
        .await;
    assert!(!prepared.requires_permission);
}

#[tokio::test]
async fn prepared_family_temp_bash_rejects_a_cwd_replaced_by_an_outside_link() {
    let fixture = ShellCwdFixture::new();
    let workspace = workspace(&fixture.project);
    let probe = fixture.touch_family_temp_probe();
    for cwd in [
        fixture.project.join("work"),
        fixture.family_temp.join("work"),
    ] {
        std::fs::create_dir(&cwd).unwrap();
        let prepared = fixture.prepare_bash(&workspace, &probe, &cwd).await;
        assert!(!prepared.requires_permission, "{}", cwd.display());
        std::fs::remove_dir(&cwd).unwrap();
        std::os::unix::fs::symlink(&fixture.outside, &cwd).unwrap();
        assert!(matches!(
            prepared.revalidate_automatic_shell_paths(),
            Err(ToolError::PathChanged(_))
        ));
        let result = fixture
            .tools
            .execute(prepared, CancellationToken::new())
            .await;
        assert!(
            matches!(result, Err(ToolError::PathChanged(_))),
            "{}: {result:?}",
            cwd.display()
        );
        assert!(!fixture.family_temp.join("probe").exists());
        std::fs::remove_file(&cwd).unwrap();
    }
}

#[tokio::test]
async fn prepared_family_temp_bash_rechecks_the_cwd_scope_before_running() {
    let fixture = ShellCwdFixture::new();
    let links = tempfile::tempdir().unwrap();
    let workdir = links.path().join("workspace");
    std::os::unix::fs::symlink(&fixture.project, &workdir).unwrap();
    let workspace = workspace(&workdir);
    let prepared = fixture
        .prepare_bash(
            &workspace,
            &fixture.touch_family_temp_probe(),
            &fixture.project,
        )
        .await;
    assert!(!prepared.requires_permission);
    std::fs::remove_file(&workdir).unwrap();
    std::os::unix::fs::symlink(&fixture.outside, &workdir).unwrap();
    let result = fixture
        .tools
        .execute(prepared, CancellationToken::new())
        .await;
    assert!(
        matches!(result, Err(ToolError::PathChanged(_))),
        "{result:?}"
    );
    assert!(!fixture.family_temp.join("probe").exists());
}

#[tokio::test]
async fn prepared_internal_bash_rejects_a_later_symlink_escape() {
    let project = tempfile::tempdir().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let path = temp.path().canonicalize().unwrap().join("result");
    std::fs::write(&path, "owned").unwrap();
    let target = outside.path().join("secret");
    std::fs::write(&target, "outside").unwrap();
    let tools = Tools::new(temp.path().canonicalize().unwrap());
    let prepared = tools
        .prepare(
            &workspace(&project.path().canonicalize().unwrap()),
            "Bash",
            &json!({"command":format!("cat {}",path.display())}),
        )
        .await
        .unwrap();
    assert!(!prepared.requires_permission);
    std::fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink(&target, &path).unwrap();
    let result = tools.execute(prepared, CancellationToken::new()).await;
    assert!(
        matches!(result, Err(ToolError::PathChanged(_))),
        "{result:?}"
    );
}

#[tokio::test]
async fn recursive_symlink_following_requires_normal_shell_authorization() {
    let project = tempfile::tempdir().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let target = outside.path().join("outside");
    std::fs::write(&target, "OUTSIDE-RESULT").unwrap();
    std::os::unix::fs::symlink(&target, root.join("escape")).unwrap();
    let tools = Tools::new(root.clone());
    let workspace = workspace(&project.path().canonicalize().unwrap());
    for command in [
        format!("rg --follow OUTSIDE {}", root.display()),
        format!("rg -nL OUTSIDE {}", root.display()),
        format!("grep -R OUTSIDE {}", root.display()),
        format!("find -L {}", root.display()),
    ] {
        let prepared = tools
            .prepare(&workspace, "Bash", &json!({"command":command}))
            .await
            .unwrap();
        assert!(prepared.requires_permission, "{command}");
    }
    let prepared = tools
        .prepare(
            &workspace,
            "Bash",
            &json!({"command":format!("rg OUTSIDE {}",root.display())}),
        )
        .await
        .unwrap();
    assert!(!prepared.requires_permission);
}

#[tokio::test]
async fn implicit_directory_diff_and_input_derived_file_paths_need_permission() {
    let project = tempfile::tempdir().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let a = root.join("a");
    let b = root.join("b");
    std::fs::create_dir(&a).unwrap();
    std::fs::create_dir(&b).unwrap();
    let secret = outside.path().join("secret");
    std::fs::write(&secret, "OUTSIDE").unwrap();
    std::os::unix::fs::symlink(&secret, a.join("leak")).unwrap();
    let list = root.join("file-list");
    std::fs::write(&list, secret.to_string_lossy().as_bytes()).unwrap();
    let tools = Tools::new(root.clone());
    let workspace = workspace(&project.path().canonicalize().unwrap());
    for command in [
        format!("diff -rN {} {}", a.display(), b.display()),
        format!("diff {} {}", a.display(), b.display()),
        format!("cat {} | xargs grep .", list.display()),
        format!("diff --from-file={} {}", secret.display(), list.display()),
    ] {
        let prepared = tools
            .prepare(&workspace, "Bash", &json!({"command":command}))
            .await
            .unwrap();
        assert!(prepared.requires_permission, "{command}");
    }
    let file_a = root.join("one");
    let file_b = root.join("two");
    std::fs::write(&file_a, "one").unwrap();
    std::fs::write(&file_b, "two").unwrap();
    let prepared = tools
        .prepare(
            &workspace,
            "Bash",
            &json!({"command":format!("diff -u {} {}",file_a.display(),file_b.display())}),
        )
        .await
        .unwrap();
    assert!(!prepared.requires_permission);
}

#[tokio::test]
async fn pattern_file_arguments_require_the_same_read_permission_as_search_inputs() {
    let project = tempfile::tempdir().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let input = root.join("input");
    std::fs::write(&input, "example").unwrap();
    let patterns = outside.path().join("patterns");
    std::fs::write(&patterns, "example").unwrap();
    let tools = Tools::new(root.clone());
    let workspace = workspace(&project.path().canonicalize().unwrap());
    for command in [
        format!("rg -f {} {}", patterns.display(), input.display()),
        format!("rg --file={} {}", patterns.display(), input.display()),
        format!("grep -f {} {}", patterns.display(), input.display()),
        format!("grep --file={} {}", patterns.display(), input.display()),
    ] {
        let prepared = tools
            .prepare(&workspace, "Bash", &json!({"command":command}))
            .await
            .unwrap();
        assert!(prepared.requires_permission, "{command}");
    }
    std::os::unix::fs::symlink(&patterns, project.path().join("123")).unwrap();
    for command in [
        format!("rg -f123 {}", input.display()),
        format!("grep -f123 {}", input.display()),
        format!("rg -e123 {}", patterns.display()),
    ] {
        let prepared = tools
            .prepare(&workspace, "Bash", &json!({"command":command}))
            .await
            .unwrap();
        assert!(prepared.requires_permission, "{command}");
    }
    let own = root.join("patterns");
    std::fs::write(&own, "example").unwrap();
    let prepared = tools
        .prepare(
            &workspace,
            "Bash",
            &json!({"command":format!("rg -f {} {}",own.display(),input.display())}),
        )
        .await
        .unwrap();
    assert!(!prepared.requires_permission);
}

#[tokio::test]
async fn tilde_directory_operands_use_the_same_normalized_path_checks() {
    let directory = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    let project = directory.path().canonicalize().unwrap();
    let home = std::env::home_dir().unwrap();
    let a = project.join("a");
    let b = project.join("b");
    std::fs::create_dir(&a).unwrap();
    std::fs::create_dir(&b).unwrap();
    let output = tempfile::tempdir().unwrap();
    let tools = Tools::new(output.path().canonicalize().unwrap());
    let command = format!(
        "diff -rN ~/{} ~/{}",
        a.strip_prefix(&home).unwrap().display(),
        b.strip_prefix(&home).unwrap().display()
    );
    let prepared = tools
        .prepare(&workspace(&project), "Bash", &json!({"command":command}))
        .await
        .unwrap();
    assert!(prepared.requires_permission);
}

#[tokio::test]
async fn filename_lists_and_ambiguous_short_values_require_ordinary_permission() {
    let project = tempfile::tempdir().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let input = root.join("input");
    let list = root.join("list");
    let other = outside.path().join("other");
    std::fs::write(&input, "input").unwrap();
    std::fs::write(&other, "other").unwrap();
    std::fs::write(&list, other.to_string_lossy().as_bytes()).unwrap();
    let tools = Tools::new(root.clone());
    let workspace = workspace(&project.path().canonicalize().unwrap());
    for command in [
        format!("file -f {}", list.display()),
        format!("sha256sum -c {}", list.display()),
        format!("sha1sum -bc {}", list.display()),
        format!("md5sum --check {}", list.display()),
        format!("wc --files0-from={}", list.display()),
        format!("du --files0-from {}", list.display()),
        format!("file --magic-file={} {}", other.display(), input.display()),
        format!("base64 -i=123 {}", input.display()),
        format!("grep -f=123 {}", input.display()),
    ] {
        let prepared = tools
            .prepare(&workspace, "Bash", &json!({"command":command}))
            .await
            .unwrap();
        assert!(prepared.requires_permission, "{command}");
    }
    let prepared = tools
        .prepare(
            &workspace,
            "Bash",
            &json!({"command":format!("rg -H input {}",input.display())}),
        )
        .await
        .unwrap();
    assert!(!prepared.requires_permission);
}

#[tokio::test]
async fn quoted_literal_glob_symlink_does_not_get_family_temp_allowance() {
    let project = tempfile::tempdir().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let path = root.join("*");
    let target = outside.path().join("target");
    std::fs::write(&target, "outside fixture").unwrap();
    std::fs::write(&path, "owned fixture").unwrap();
    let tools = Tools::new(root);
    let workspace = workspace(&project.path().canonicalize().unwrap());
    let command = format!("cat '{}'", path.display());
    let prepared = tools
        .prepare(&workspace, "Bash", &json!({"command":command}))
        .await
        .unwrap();
    assert!(!prepared.requires_permission);
    std::fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink(&target, &path).unwrap();
    let prepared = tools
        .prepare(&workspace, "Bash", &json!({"command":command}))
        .await
        .unwrap();
    assert!(prepared.requires_permission);
}

#[tokio::test]
async fn prepared_quoted_literal_glob_rechecks_replaced_symlink() {
    let project = tempfile::tempdir().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let path = root.join("*");
    std::fs::write(&path, "owned fixture").unwrap();
    let target = outside.path().join("target");
    std::fs::write(&target, "outside fixture").unwrap();
    let tools = Tools::new(root);
    let prepared = tools
        .prepare(
            &workspace(&project.path().canonicalize().unwrap()),
            "Bash",
            &json!({"command":format!("cat '{}'",path.display())}),
        )
        .await
        .unwrap();
    assert!(!prepared.requires_permission);
    std::fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink(&target, &path).unwrap();
    assert!(matches!(
        tools.execute(prepared, CancellationToken::new()).await,
        Err(ToolError::PathChanged(_))
    ));
}

/// Refuses the `Read` of a file named `refused.png`, asks for one named `asked.png`, and admits the
/// rest.
struct AdmissionByName;

#[async_trait::async_trait]
impl crate::LocalReadAdmission for AdmissionByName {
    async fn admit(&self, read: &crate::PreparedTool) -> Result<bool, String> {
        match Path::new(read.input["file_path"].as_str().unwrap())
            .file_name()
            .and_then(|name| name.to_str())
        {
            Some("refused.png") => Err("Read of refused.png is denied".into()),
            Some("asked.png") => Ok(true),
            _ => Ok(false),
        }
    }
}

#[tokio::test]
async fn a_local_file_sent_elsewhere_follows_read_rules_and_refuses_sensitive_paths() {
    let parent = tempfile::tempdir().unwrap();
    let project = parent.path().join("token-service");
    std::fs::create_dir(&project).unwrap();
    let project = project.canonicalize().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let output = tempfile::tempdir().unwrap();
    let files = chat_tools(&output)
        .local_file_reader(&workspace(&project), std::sync::Arc::new(AdmissionByName))
        .expect("a Chat with a Folder reads local files");
    std::fs::write(project.join("chart.png"), b"chart").unwrap();
    std::fs::write(elsewhere.path().join("photo.png"), b"photo").unwrap();

    let inside = files.read("chart.png", 16).await.unwrap();
    assert_eq!(inside.path, project.join("chart.png"));
    assert_eq!(inside.read.name, READ_TOOL_NAME);
    assert!(inside.read.in_workspace && !inside.read.requires_permission);
    assert!(inside.read.is_supported_in_delegated_scope());
    assert!(!inside.read_asks);
    assert_eq!(inside.bytes, b"chart");
    let outside = files
        .read(elsewhere.path().join("photo.png").to_str().unwrap(), 16)
        .await
        .unwrap();
    assert!(!outside.read.in_workspace && outside.read.requires_permission);
    assert!(
        !outside.read.is_supported_in_delegated_scope(),
        "a Subagent's own Read would refuse it"
    );
    assert!(matches!(
        files.read("chart.png", 4).await,
        Err(ToolError::Rejected(message)) if message.contains("exceeds maximum allowed size")
    ));
    std::fs::write(project.join("refused.png"), b"far too large").unwrap();
    assert!(
        matches!(
            files.read("refused.png", 4).await,
            Err(ToolError::Rejected(message)) if message == "Read of refused.png is denied"
        ),
        "a refused Read is decided before the file is opened or measured"
    );
    std::fs::write(project.join("asked.png"), b"asked").unwrap();
    assert!(files.read("asked.png", 16).await.unwrap().read_asks);

    std::fs::create_dir(project.join(".ssh")).unwrap();
    std::fs::write(project.join(".ssh/id.png"), b"key").unwrap();
    std::fs::write(project.join("api_token.png"), b"key").unwrap();
    std::os::unix::fs::symlink(project.join(".ssh/id.png"), project.join("innocent.png")).unwrap();
    for sensitive in [".ssh/id.png", "api_token.png", "innocent.png"] {
        assert!(
            matches!(
                files.read(sensitive, 16).await,
                Err(ToolError::Rejected(message)) if message.contains("credentials or secrets")
            ),
            "{sensitive}"
        );
    }
    assert!(matches!(
        files.read(".", 16).await,
        Err(ToolError::Rejected(message)) if message.starts_with("EISDIR")
    ));
}

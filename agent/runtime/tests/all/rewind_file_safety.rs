use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
    sync::Arc,
};

use serde_json::json;
use tickloop_agent_protocol::{CodeRewind, EventKind, RewindMode};
use tickloop_agent_runtime::Agent;
use tickloop_agent_test_support::{ScriptedModel, call, config, next_event, text};
use uuid::Uuid;

const OUTSIDE_CONTENT: &str = "outside the workspace\n";

async fn run_prompt(
    agent: &Agent,
    events: &mut tokio::sync::broadcast::Receiver<tickloop_agent_protocol::Event>,
    chat: Uuid,
    prompt: &str,
) {
    let receipt = agent
        .submit(chat, None, prompt.into())
        .await
        .expect("prompt accepted");
    loop {
        let event = next_event(events).await;
        if event.run_id != receipt.run_id {
            continue;
        }
        match event.kind {
            EventKind::PermissionRequested { request } => agent
                .answer_permission(chat, request.id, true)
                .await
                .expect("permission accepted"),
            EventKind::Completed { .. } => return,
            EventKind::Failed(error) => panic!("prompt failed: {error}"),
            _ => {}
        }
    }
}

struct Workspace {
    _directory: tempfile::TempDir,
    state: PathBuf,
    workdir: PathBuf,
    outside: PathBuf,
}

fn workspace() -> Workspace {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let workdir = root.join("work");
    let outside = root.join("outside");
    fs::create_dir(&workdir).unwrap();
    fs::create_dir(&outside).unwrap();
    Workspace {
        state: root.join("state"),
        workdir,
        outside,
        _directory: directory,
    }
}

/// 第一条提示让模型执行 `edits`，第二条提示只回复文字，使第一条提示的快照可以恢复。
async fn chat_after_edits(
    workspace: &Workspace,
    edits: Vec<Vec<tickloop_agent_protocol::Content>>,
) -> (Agent, Uuid, Vec<Uuid>) {
    let mut replies = edits;
    replies.push(text("edited"));
    replies.push(text("second answer"));
    let model = Arc::new(ScriptedModel::new(replies));
    let agent = Agent::open_embedded(workspace.state.clone(), config(), model).unwrap();
    let chat = agent.create_chat(workspace.workdir.clone()).await.unwrap();
    let mut events = agent.subscribe();
    run_prompt(&agent, &mut events, chat, "edit files").await;
    run_prompt(&agent, &mut events, chat, "keep talking").await;
    let points = agent
        .snapshot(chat)
        .await
        .unwrap()
        .rewind_points()
        .into_iter()
        .map(|point| point.message_id)
        .collect();
    (agent, chat, points)
}

fn failed_paths(code: &CodeRewind) -> Vec<PathBuf> {
    let mut paths = code
        .failed_files
        .iter()
        .map(|(path, _)| path.clone())
        .collect::<Vec<_>>();
    paths.sort();
    paths
}

fn files_containing(directory: &Path, needle: &str) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            // The active recorder atomically replaces private temporary files while we scan.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => panic!("inspect {}: {error}", path.display()),
        };
        if metadata.is_dir() {
            found.extend(files_containing(&path, needle));
        } else if metadata.is_file()
            && fs::read_to_string(&path).is_ok_and(|content| content.contains(needle))
        {
            found.push(path);
        }
    }
    found
}

#[tokio::test]
async fn rewind_does_not_write_through_a_tracked_file_replaced_by_a_symbolic_link() {
    let workspace = workspace();
    let tracked = workspace.workdir.join("tracked.txt");
    fs::write(&tracked, "original\n").unwrap();
    let (agent, chat, points) = chat_after_edits(
        &workspace,
        vec![
            call("read", "Read", json!({"file_path":"tracked.txt"})),
            call(
                "write",
                "Write",
                json!({"file_path":"tracked.txt","content":"agent\n"}),
            ),
        ],
    )
    .await;
    let secret = workspace.outside.join("secret.txt");
    fs::write(&secret, OUTSIDE_CONTENT).unwrap();
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o640)).unwrap();
    fs::remove_file(&tracked).unwrap();
    symlink(&secret, &tracked).unwrap();

    let outcome = agent
        .rewind(chat, points[0], RewindMode::Code)
        .await
        .unwrap();
    let code = outcome.code.unwrap();
    assert!(code.error.is_none());
    assert!(code.changed_files.is_empty());
    assert_eq!(failed_paths(&code), std::slice::from_ref(&tracked));
    assert_eq!(fs::read_to_string(&secret).unwrap(), OUTSIDE_CONTENT);
    assert_eq!(
        fs::metadata(&secret).unwrap().permissions().mode() & 0o777,
        0o640
    );
    assert!(fs::symlink_metadata(&tracked).unwrap().is_symlink());
    agent.shutdown().await;
}

#[tokio::test]
async fn rewind_does_not_delete_or_write_inside_a_parent_replaced_by_a_symbolic_link() {
    let workspace = workspace();
    let nested = workspace.workdir.join("nested");
    fs::create_dir(&nested).unwrap();
    fs::write(nested.join("modified.txt"), "original\n").unwrap();
    let (agent, chat, points) = chat_after_edits(
        &workspace,
        vec![
            call("read", "Read", json!({"file_path":"nested/modified.txt"})),
            call(
                "edit",
                "Write",
                json!({"file_path":"nested/modified.txt","content":"agent\n"}),
            ),
            call(
                "create",
                "Write",
                json!({"file_path":"nested/created.txt","content":"agent created\n"}),
            ),
        ],
    )
    .await;
    fs::remove_dir_all(&nested).unwrap();
    fs::write(workspace.outside.join("modified.txt"), OUTSIDE_CONTENT).unwrap();
    fs::write(workspace.outside.join("created.txt"), OUTSIDE_CONTENT).unwrap();
    symlink(&workspace.outside, &nested).unwrap();

    let preview = agent
        .rewind_file_changes(chat, points[0])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(preview.len(), 2);
    let outcome = agent
        .rewind(chat, points[0], RewindMode::Code)
        .await
        .unwrap();
    let code = outcome.code.unwrap();
    assert!(code.changed_files.is_empty());
    assert_eq!(
        failed_paths(&code),
        [nested.join("created.txt"), nested.join("modified.txt")]
    );
    for name in ["modified.txt", "created.txt"] {
        assert_eq!(
            fs::read_to_string(workspace.outside.join(name)).unwrap(),
            OUTSIDE_CONTENT
        );
    }
    let mut outside_entries = fs::read_dir(&workspace.outside)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    outside_entries.sort();
    assert_eq!(outside_entries, ["created.txt", "modified.txt"]);
    agent.shutdown().await;
}

#[tokio::test]
async fn snapshots_never_back_up_the_target_of_a_symbolic_link() {
    let workspace = workspace();
    let tracked = workspace.workdir.join("tracked.txt");
    fs::write(&tracked, "original\n").unwrap();
    let secret = workspace.outside.join("secret.txt");
    fs::write(&secret, OUTSIDE_CONTENT).unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        call("read", "Read", json!({"file_path":"tracked.txt"})),
        call(
            "write",
            "Write",
            json!({"file_path":"tracked.txt","content":"agent\n"}),
        ),
        text("edited"),
        text("answer while linked"),
    ]));
    let agent = Agent::open_embedded(workspace.state.clone(), config(), model).unwrap();
    let chat = agent.create_chat(workspace.workdir.clone()).await.unwrap();
    let mut events = agent.subscribe();
    run_prompt(&agent, &mut events, chat, "edit the file").await;
    fs::remove_file(&tracked).unwrap();
    symlink(&secret, &tracked).unwrap();
    run_prompt(&agent, &mut events, chat, "prompt while the file is a link").await;
    assert!(files_containing(&workspace.state, OUTSIDE_CONTENT).is_empty());

    fs::remove_file(&tracked).unwrap();
    fs::write(&tracked, "replaced later\n").unwrap();
    let points = agent.snapshot(chat).await.unwrap().rewind_points();
    let outcome = agent
        .rewind(chat, points[1].message_id, RewindMode::Code)
        .await
        .unwrap();
    assert!(outcome.code.unwrap().failed_files.is_empty());
    assert_ne!(fs::read_to_string(&tracked).unwrap(), OUTSIDE_CONTENT);
    assert!(files_containing(&workspace.workdir, OUTSIDE_CONTENT).is_empty());
    assert!(files_containing(&workspace.state, OUTSIDE_CONTENT).is_empty());
    agent.shutdown().await;
}

#[tokio::test]
async fn restored_parent_directories_follow_the_umask_instead_of_owner_only_permissions() {
    let workspace = workspace();
    let deep = workspace.workdir.join("nested").join("deep");
    fs::create_dir_all(&deep).unwrap();
    let tracked = deep.join("tracked.txt");
    fs::write(&tracked, "original\n").unwrap();
    let (agent, chat, points) = chat_after_edits(
        &workspace,
        vec![
            call(
                "read",
                "Read",
                json!({"file_path":"nested/deep/tracked.txt"}),
            ),
            call(
                "write",
                "Write",
                json!({"file_path":"nested/deep/tracked.txt","content":"agent\n"}),
            ),
        ],
    )
    .await;
    fs::remove_dir_all(workspace.workdir.join("nested")).unwrap();
    let reference = workspace.outside.join("reference");
    fs::create_dir(&reference).unwrap();
    let expected_mode = fs::metadata(&reference).unwrap().permissions().mode() & 0o7777;

    let outcome = agent
        .rewind(chat, points[0], RewindMode::Code)
        .await
        .unwrap();
    let code = outcome.code.unwrap();
    assert!(code.failed_files.is_empty(), "{:?}", code.failed_files);
    assert_eq!(fs::read_to_string(&tracked).unwrap(), "original\n");
    for directory in [workspace.workdir.join("nested"), deep] {
        assert_eq!(
            fs::metadata(&directory).unwrap().permissions().mode() & 0o7777,
            expected_mode,
            "{}",
            directory.display()
        );
    }
    agent.shutdown().await;
}

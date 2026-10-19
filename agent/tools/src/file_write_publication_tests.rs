use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::Path,
};

use crate::{Tools, Workspace};
use serde_json::json;
use tokio_util::sync::CancellationToken;

fn workspace(path: &Path) -> Workspace {
    Workspace {
        folder: Some(path.canonicalize().unwrap()),
        additional_directories: Vec::new(),
    }
}

#[tokio::test]
async fn cancellation_and_conflicts_never_publish_a_write() {
    let directory = tempfile::tempdir().unwrap();
    let tools = Tools::new(directory.path().join("output"));
    let workspace = workspace(directory.path());
    let input = json!({"file_path":"notes.txt","content":"new"});
    let rejected = tools.prepare(&workspace, "Write", &input).await.unwrap();
    drop(rejected);
    assert!(!directory.path().join("notes.txt").exists());

    let cancelled = tools.prepare(&workspace, "Write", &input).await.unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(tools.execute(cancelled, cancel).await.is_err());
    assert!(!directory.path().join("notes.txt").exists());

    let conflict = tools.prepare(&workspace, "Write", &input).await.unwrap();
    fs::write(directory.path().join("notes.txt"), "manual change").unwrap();
    assert!(
        tools
            .execute(conflict, CancellationToken::new())
            .await
            .is_err()
    );
    assert_eq!(
        fs::read(directory.path().join("notes.txt")).unwrap(),
        b"manual change"
    );
    assert!(fs::read_dir(directory.path()).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".tickloop-write-")
    }));
}

#[tokio::test]
async fn restrictive_umask_does_not_remove_existing_rwx_permissions() {
    if std::env::var_os("TICKLOOP_WRITE_UMASK_CHILD").is_none() {
        use std::os::unix::process::CommandExt;
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "file_write_publication_tests::restrictive_umask_does_not_remove_existing_rwx_permissions",
                "--nocapture",
            ])
            .env("TICKLOOP_WRITE_UMASK_CHILD", "1");
        unsafe {
            command.pre_exec(|| {
                libc::umask(0o077);
                Ok(())
            });
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("notes");
    fs::write(&target, "old").unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o751)).unwrap();
    let tools = Tools::new(directory.path().join("output"));
    let read = tools
        .prepare(
            &workspace(directory.path()),
            "Read",
            &json!({"file_path":"notes"}),
        )
        .await
        .unwrap();
    tools.execute(read, CancellationToken::new()).await.unwrap();
    let prepared = tools
        .prepare(
            &workspace(directory.path()),
            "Write",
            &json!({"file_path":"notes","content":"new"}),
        )
        .await
        .unwrap();
    tools
        .execute(prepared, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(fs::metadata(target).unwrap().mode() & 0o777, 0o751);
    assert_eq!(
        fs::read_to_string(directory.path().join("notes")).unwrap(),
        "new"
    );
}

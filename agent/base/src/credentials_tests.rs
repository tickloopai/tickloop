use super::*;
use std::io::Write;

fn root(directory: &tempfile::TempDir) -> std::path::PathBuf {
    directory.path().canonicalize().unwrap().join("state")
}

fn write_private(path: &Path, contents: &str) {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    file.write_all(contents.as_bytes()).unwrap();
}

#[test]
fn saves_and_replaces_only_the_named_assignment_with_private_permissions() {
    let directory = tempfile::tempdir().unwrap();
    let root = root(&directory);
    save_api_key(&root, "FIRST_API_KEY", "initial").unwrap();
    std::fs::write(
        root.join(".env"),
        "# keep this comment\nOTHER=value # keep inline\nexport FIRST_API_KEY='initial'\nLAST=untouched\n",
    )
    .unwrap();
    save_api_key(&root, "FIRST_API_KEY", "new \\\" # $HOME `echo` value").unwrap();
    let contents = std::fs::read_to_string(root.join(".env")).unwrap();
    assert!(contents.starts_with("# keep this comment\nOTHER=value # keep inline\n"));
    assert!(contents.ends_with("\nLAST=untouched\n"));
    assert_eq!(contents.matches("FIRST_API_KEY=").count(), 1);
    assert!(contents.contains("\\$HOME"));
    assert!(contents.contains("\\`echo\\`"));
    assert_eq!(
        load_api_key(&root, "FIRST_API_KEY").unwrap().as_deref(),
        Some("new \\\" # $HOME `echo` value")
    );
    assert_eq!(
        api_key_status(&root, "FIRST_API_KEY").unwrap(),
        ApiKeyStatus::Saved
    );
    assert_eq!(
        std::fs::metadata(root.join(".env"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(root.join(".env.lock"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn public_state_directory_without_env_keeps_environment_fallback_and_is_secured_on_save() {
    let directory = tempfile::tempdir().unwrap();
    let root = root(&directory);
    let name = format!("TICKLOOP_UNSET_{}", uuid::Uuid::new_v4().simple()).to_uppercase();
    std::fs::create_dir(&root).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(load_api_key(&root, &name).unwrap(), None);
    assert_eq!(api_key_status(&root, &name).unwrap(), ApiKeyStatus::Missing);

    save_api_key(&root, &name, "fake-key").unwrap();
    assert_eq!(
        std::fs::metadata(&root).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        load_api_key(&root, &name).unwrap().as_deref(),
        Some("fake-key")
    );
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(load_api_key(&root, &name).is_err());
}

#[test]
fn removes_only_the_named_assignment_and_preserves_other_content() {
    let directory = tempfile::tempdir().unwrap();
    let root = root(&directory);
    save_api_key(&root, "FIRST_KEY", "first").unwrap();
    std::fs::write(
        root.join(".env"),
        "# a comment\nOTHER=value # leave this\nexport FIRST_KEY='first'\nLAST=untouched\n",
    )
    .unwrap();
    remove_api_key(&root, "FIRST_KEY").unwrap();
    assert_eq!(
        std::fs::read_to_string(root.join(".env")).unwrap(),
        "# a comment\nOTHER=value # leave this\nLAST=untouched\n"
    );
    assert_eq!(load_api_key(&root, "FIRST_KEY").unwrap(), None);
    assert_eq!(
        load_api_key(&root, "OTHER").unwrap().as_deref(),
        Some("value")
    );
    assert_eq!(
        std::fs::metadata(root.join(".env"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn removing_an_absent_key_does_not_create_or_change_an_env_file() {
    let directory = tempfile::tempdir().unwrap();
    let root = root(&directory);
    remove_api_key(&root, "MISSING_KEY").unwrap();
    assert!(!root.exists());

    std::fs::create_dir(&root).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
    remove_api_key(&root, "MISSING_KEY").unwrap();
    assert_eq!(
        std::fs::metadata(&root).unwrap().permissions().mode() & 0o777,
        0o755
    );
    assert!(!root.join(".env.lock").exists());

    save_api_key(&root, "OTHER_KEY", "other").unwrap();
    let before = std::fs::read(root.join(".env")).unwrap();
    remove_api_key(&root, "MISSING_KEY").unwrap();
    assert_eq!(std::fs::read(root.join(".env")).unwrap(), before);
    remove_api_key(&root, "OTHER_KEY").unwrap();
    assert_eq!(std::fs::read(root.join(".env")).unwrap(), b"");
    assert_eq!(
        std::fs::metadata(root.join(".env"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn rejects_unsafe_files_and_values_without_changing_existing_content() {
    let directory = tempfile::tempdir().unwrap();
    let root = root(&directory);
    save_api_key(&root, "ONE_KEY", "first").unwrap();
    let before = std::fs::read(root.join(".env")).unwrap();
    for name in ["A=B", "1BAD", "KEY\nOTHER"] {
        assert!(save_api_key(&root, name, "second").is_err());
    }
    for value in ["", "\nsecond", "second\r"] {
        assert!(save_api_key(&root, "ONE_KEY", value).is_err());
    }
    assert_eq!(std::fs::read(root.join(".env")).unwrap(), before);

    std::fs::write(root.join(".env"), "ONE_KEY=first\nONE_KEY=second\n").unwrap();
    let duplicate = std::fs::read(root.join(".env")).unwrap();
    assert!(save_api_key(&root, "ONE_KEY", "third").is_err());
    assert!(remove_api_key(&root, "ONE_KEY").is_err());
    assert_eq!(std::fs::read(root.join(".env")).unwrap(), duplicate);

    std::fs::remove_file(root.join(".env")).unwrap();
    let outside = directory.path().join("outside");
    write_private(&outside, "ONE_KEY=outside\n");
    std::os::unix::fs::symlink(&outside, root.join(".env")).unwrap();
    assert!(load_api_key(&root, "ONE_KEY").is_err());
    assert!(save_api_key(&root, "ONE_KEY", "third").is_err());
    assert!(remove_api_key(&root, "ONE_KEY").is_err());
    assert_eq!(
        std::fs::read_to_string(&outside).unwrap(),
        "ONE_KEY=outside\n"
    );
}

#[test]
fn concurrent_writers_preserve_distinct_entries() {
    let directory = tempfile::tempdir().unwrap();
    let root = root(&directory);
    let workers = (0..8)
        .map(|index| {
            let root = root.clone();
            std::thread::spawn(move || {
                save_api_key(&root, &format!("KEY_{index}"), &format!("value_{index}"))
            })
        })
        .collect::<Vec<_>>();
    for worker in workers {
        worker.join().unwrap().unwrap();
    }
    for index in 0..8 {
        assert_eq!(
            load_api_key(&root, &format!("KEY_{index}"))
                .unwrap()
                .as_deref(),
            Some(format!("value_{index}").as_str())
        );
    }
}

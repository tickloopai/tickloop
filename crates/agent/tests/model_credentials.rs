use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use tickloop_agent::model::{
    Message, ModelAuthSource, ModelConfig, ModelProvider, ModelRequest, Protocol, Role,
    credentials::{ApiKeyStatus, api_key_status, load_api_key, remove_api_key, save_api_key},
    model_client,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

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

#[tokio::test]
async fn model_factory_reads_saved_key_for_each_request() {
    let directory = tempfile::tempdir().unwrap();
    let root = root(&directory);
    let key_name = format!("TICKLOOP_TEST_KEY_{}", uuid::Uuid::new_v4().simple()).to_uppercase();
    save_api_key(&root, &key_name, "fake-first-key").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}/anthropic", listener.local_addr().unwrap());
    let (request_sender, mut received_requests) = mpsc::unbounded_channel();
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                let mut buffer = [0; 4096];
                let length = socket.read(&mut buffer).await.unwrap();
                assert_ne!(length, 0);
                bytes.extend_from_slice(&buffer[..length]);
            }
            request_sender
                .send(String::from_utf8(bytes).unwrap())
                .unwrap();
            socket.write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Type: text/event-stream\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}").await.unwrap();
        }
    });
    let config = ModelConfig {
        name: "fixture".into(),
        protocol: Protocol::Messages,
        base_url,
        model: "fixture-model".into(),
        api_key_env: key_name.clone(),
        max_output_tokens: 32,
        provider: ModelProvider::Api,
        auth_source: ModelAuthSource::ApiKeyEnv,
        provider_id: String::new(),
        provider_name: String::new(),
        model_id: String::new(),
        connection: None,
        card: None,
        input_modalities: vec![tickloop_agent::model::InputModality::Text],
    };
    let client = model_client(config, Some(&root)).unwrap();
    let request = || ModelRequest {
        history_sources: Vec::new(),
        messages: vec![Message {
            role: Role::User,
            content: vec![],
        }],
        tools: vec![],
        structured_output: None,
        temperature: None,
        reasoning_effort: None,
    };
    let (events, _) = mpsc::unbounded_channel();
    assert!(
        client
            .complete(request(), events, CancellationToken::new())
            .await
            .is_err()
    );
    let first = received_requests.recv().await.unwrap();
    assert!(first.contains("x-api-key: fake-first-key"));

    save_api_key(&root, &key_name, "fake-second-key").unwrap();
    let (events, _) = mpsc::unbounded_channel();
    assert!(
        client
            .complete(request(), events, CancellationToken::new())
            .await
            .is_err()
    );
    let second = received_requests.recv().await.unwrap();
    assert!(second.contains("x-api-key: fake-second-key"));
    server.await.unwrap();
}

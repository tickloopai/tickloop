use std::{os::unix::fs::PermissionsExt, sync::Arc};
use tickloop_code_settings::mcp_credentials::FileCredentials;
use tickloop_harness::connector::{
    ClientRegistration, Connector, ConnectorCredential, ConnectorCredentials, OAuthSettings,
    OAuthToken,
};
use tokio_util::sync::CancellationToken;

fn credential() -> ConnectorCredential {
    ConnectorCredential {
        server_url: "https://example.com/mcp".into(),
        registration: ClientRegistration {
            authorization_server: "https://example.com".into(),
            client_id: "client".into(),
            redirect_uri: "http://localhost:1234/callback".into(),
        },
        resource: None,
        token: Some(OAuthToken {
            access_token: "private-access".into(),
            refresh_token: Some("private-refresh".into()),
            expires_at: u64::MAX,
            token_endpoint: "https://example.com/token".into(),
            client_id: "client".into(),
            token_type: "Bearer".into(),
            scope: None,
            issuer: "https://example.com".into(),
            revocation_endpoint: None,
        }),
    }
}

#[tokio::test]
async fn independent_adapters_share_private_credentials_and_reject_a_changed_address() {
    let root = tempfile::tempdir().unwrap();
    let first = FileCredentials::new(root.path(), "plugin:notes", "plugin_notes_remote").unwrap();
    let second =
        Arc::new(FileCredentials::new(root.path(), "plugin:notes", "plugin_notes_remote").unwrap());
    let lock = first.lock().await.unwrap();
    first.save(&credential()).await.unwrap();
    drop(lock);
    assert_eq!(second.load().await.unwrap(), Some(credential()));
    let path = root.path().join("credentials/notes/mcp/remote.json");
    assert_eq!(
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    for path in ["credentials", "credentials/notes", "credentials/notes/mcp"] {
        assert_eq!(
            std::fs::metadata(root.path().join(path))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
    let connector = Connector::new(
        "https://other.example/mcp",
        OAuthSettings::default(),
        second,
    )
    .unwrap();
    assert!(
        connector
            .access_token(&CancellationToken::new())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn a_lock_excludes_independent_adapters_and_cancelled_waiters_release_their_files() {
    let root = tempfile::tempdir().unwrap();
    let first = FileCredentials::new(root.path(), "global", "remote").unwrap();
    let second = FileCredentials::new(root.path(), "directory", "remote").unwrap();
    let held = first.lock().await.unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), second.lock())
            .await
            .is_err()
    );
    drop(held);
    let held = tokio::time::timeout(std::time::Duration::from_secs(1), second.lock())
        .await
        .unwrap()
        .unwrap();
    second.save(&credential()).await.unwrap();
    drop(held);
    assert_eq!(first.load().await.unwrap(), Some(credential()));
    let path = root.path().join("credentials/_mcp/remote.json");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(first.load().await.is_err());
    std::fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink(root.path().join("outside"), path).unwrap();
    assert!(first.load().await.is_err());
    assert!(FileCredentials::new(root.path(), "global", "../escape").is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn agents_with_independent_file_adapters_renew_a_rotating_token_once() {
    use tick_oauth_test_server::{AuthorizationServer, METADATA_PATH, TOKEN_PATH};
    let root = tempfile::tempdir().unwrap();
    let server = AuthorizationServer::start();
    server.issue_tokens_lasting(10);
    let first = Connector::new(
        &server.resource(),
        OAuthSettings {
            authorization_server_metadata_url: Some(format!("{}{METADATA_PATH}", server.issuer())),
            ..Default::default()
        },
        Arc::new(FileCredentials::new(root.path(), "global", "remote").unwrap()),
    )
    .unwrap();
    let cancel = CancellationToken::new();
    let login = first.start_login(None, &cancel).await.unwrap();
    login
        .paste()
        .submit(&server.approve(login.authorization_url()))
        .unwrap();
    login.finish(&cancel).await.unwrap();
    server.issue_tokens_lasting(3600);
    let second = Connector::new(
        &server.resource(),
        OAuthSettings::default(),
        Arc::new(FileCredentials::new(root.path(), "global", "remote").unwrap()),
    )
    .unwrap();
    let (one, two) = tokio::join!(first.access_token(&cancel), second.access_token(&cancel));
    assert_eq!(one.unwrap(), two.unwrap());
    assert_eq!(server.requests_to(TOKEN_PATH).len(), 2);
}

#[tokio::test]
async fn the_credential_lock_excludes_another_process() {
    use std::{
        io::{BufRead, BufReader, Write},
        process::{Command, Stdio},
        time::Duration,
    };
    let root = tempfile::tempdir().unwrap();
    let adapter = FileCredentials::new(root.path(), "global", "remote").unwrap();
    drop(adapter.lock().await.unwrap());
    let mut child = Command::new("python3").args(["-u", "-c", "import fcntl,sys\nf=open(sys.argv[1],'r+')\nfcntl.flock(f,fcntl.LOCK_EX)\nprint('locked',flush=True)\nsys.stdin.readline()"])
        .arg(root.path().join("credentials/_mcp/remote.lock")).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(line.trim(), "locked");
    let blocked = tokio::time::timeout(Duration::from_millis(100), adapter.lock())
        .await
        .is_err();
    writeln!(child.stdin.as_mut().unwrap(), "release").unwrap();
    assert!(child.wait().unwrap().success());
    assert!(blocked);
    drop(
        tokio::time::timeout(Duration::from_secs(1), adapter.lock())
            .await
            .unwrap()
            .unwrap(),
    );
}

#[tokio::test]
async fn plugin_service_names_are_encoded_without_collisions_or_path_escape() {
    let root = tempfile::tempdir().unwrap();
    for name in [
        "team/remote",
        "team%2Fremote",
        "team\\remote",
        "remote",
        "Remote",
    ] {
        let adapter =
            FileCredentials::new(root.path(), "plugin:notes", &format!("plugin_notes_{name}"))
                .unwrap();
        let mut saved = credential();
        saved.registration.client_id = name.into();
        let _lock = adapter.lock().await.unwrap();
        adapter.save(&saved).await.unwrap();
    }
    for name in [
        "team/remote",
        "team%2Fremote",
        "team\\remote",
        "remote",
        "Remote",
    ] {
        let adapter =
            FileCredentials::new(root.path(), "plugin:notes", &format!("plugin_notes_{name}"))
                .unwrap();
        assert_eq!(
            adapter
                .load()
                .await
                .unwrap()
                .unwrap()
                .registration
                .client_id,
            name
        );
    }
    assert!(!root.path().join("credentials/notes/mcp/team").exists());
}

#[tokio::test]
async fn long_service_names_keep_distinct_credentials() {
    let root = tempfile::tempdir().unwrap();
    let names = ["x".repeat(300), format!("{}y", "x".repeat(299))];
    for name in &names {
        let adapter = FileCredentials::new(root.path(), "global", name).unwrap();
        let mut saved = credential();
        saved.registration.client_id = name.clone();
        let _held = adapter.lock().await.unwrap();
        adapter.save(&saved).await.unwrap();
    }
    for name in &names {
        let adapter = FileCredentials::new(root.path(), "global", name).unwrap();
        assert_eq!(
            adapter
                .load()
                .await
                .unwrap()
                .unwrap()
                .registration
                .client_id,
            *name
        );
    }
}

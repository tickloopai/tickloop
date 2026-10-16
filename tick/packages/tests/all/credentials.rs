use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    sync::mpsc,
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde_json::{Value, json};
use tick_packages::{
    ApiKey, LoginState, OAuthIssuer, OAuthToken, PackageAuth, PackageCredentials, PackageError,
};

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn token(expires_at: u64, refresh_token: Option<&str>) -> OAuthToken {
    OAuthToken {
        access_token: "secret-access".into(),
        token_type: "Bearer".into(),
        refresh_token: refresh_token.map(str::to_owned),
        scope: None,
        issuer: "https://auth.example.com".into(),
        client_id: "client-1".into(),
        token_endpoint: "https://auth.example.com/token".into(),
        revocation_endpoint: None,
        expires_at,
    }
}

fn oauth2() -> PackageAuth {
    PackageAuth::OAuth2 {
        issuers: vec![OAuthIssuer {
            label: "Server".into(),
            issuer: "https://auth.example.com".into(),
        }],
        scopes: Vec::new(),
    }
}

fn api_key() -> PackageAuth {
    PackageAuth::ApiKey {
        env: "SAMPLE_KEY".into(),
        label: "Sample key".into(),
    }
}

/// Every name in `directory`.
fn names_in(directory: &Path) -> Vec<String> {
    let mut names = fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    names.sort();
    names
}

#[test]
fn credentials_live_in_an_owner_only_directory_named_after_the_package() {
    let directory = tempfile::tempdir().unwrap();
    let config_root = directory.path().join("config");
    let credentials = PackageCredentials::in_config_root(&config_root, "sample").unwrap();
    assert_eq!(
        credentials.directory(),
        config_root.join("credentials/sample")
    );
    assert_eq!(
        credentials.oauth_token_path(),
        config_root.join("credentials/sample/oauth2.json")
    );
    assert_eq!(
        credentials.api_key_path(),
        config_root.join("credentials/sample/api_key")
    );
    for name in ["../sample", "Sample", "", "a/b"] {
        assert!(matches!(
            PackageCredentials::in_config_root(&config_root, name),
            Err(PackageError::Invalid(_))
        ));
    }

    credentials
        .save_api_key(&ApiKey::new("key").unwrap())
        .unwrap();
    assert_eq!(mode(&config_root.join("credentials")), 0o700);
    assert_eq!(mode(credentials.directory()), 0o700);
    assert_eq!(mode(&credentials.api_key_path()), 0o600);

    let opened = PackageCredentials::in_config_root(&config_root, "opened").unwrap();
    fs::create_dir_all(opened.directory()).unwrap();
    fs::set_permissions(opened.directory(), fs::Permissions::from_mode(0o755)).unwrap();
    opened
        .lock_oauth()
        .unwrap()
        .save_token(&token(now() + 60, None))
        .unwrap();
    assert_eq!(mode(opened.directory()), 0o700);
    assert_eq!(mode(&opened.oauth_token_path()), 0o600);
}

#[test]
fn the_login_state_comes_from_the_saved_files_alone() {
    let directory = tempfile::tempdir().unwrap();
    let credentials = PackageCredentials::in_config_root(directory.path(), "sample").unwrap();
    assert_eq!(
        credentials.login_state(&PackageAuth::None),
        LoginState::NotRequired
    );

    assert_eq!(credentials.login_state(&api_key()), LoginState::LoggedOut);
    credentials
        .save_api_key(&ApiKey::new("  key-1\n").unwrap())
        .unwrap();
    assert_eq!(credentials.login_state(&api_key()), LoginState::LoggedIn);
    assert_eq!(fs::read(credentials.api_key_path()).unwrap(), b"key-1");
    for unreadable in [&b""[..], b"  \n", b"\xff\xfe", b"key\x1b[31m"] {
        fs::write(credentials.api_key_path(), unreadable).unwrap();
        assert_eq!(
            credentials.login_state(&api_key()),
            LoginState::Unknown,
            "{unreadable:?}"
        );
    }
    assert!(credentials.remove_api_key().unwrap());
    assert!(!credentials.remove_api_key().unwrap());
    assert_eq!(credentials.login_state(&api_key()), LoginState::LoggedOut);

    assert_eq!(credentials.login_state(&oauth2()), LoginState::LoggedOut);
    let now = now();
    for (saved, state) in [
        (token(now + 600, None), LoginState::LoggedIn),
        (token(now + 600, Some("refresh")), LoginState::LoggedIn),
        (token(now - 600, Some("refresh")), LoginState::LoggedIn),
        (token(now - 600, None), LoginState::Expired),
        (token(now - 600, Some("")), LoginState::Expired),
    ] {
        credentials
            .lock_oauth()
            .unwrap()
            .save_token(&saved)
            .unwrap();
        assert_eq!(credentials.login_state(&oauth2()), state, "{saved:?}");
    }
    let mut saved: Value =
        serde_json::from_slice(&fs::read(credentials.oauth_token_path()).unwrap()).unwrap();
    for (field, value) in [
        ("access_token", json!("")),
        ("client_id", json!("")),
        ("expires_at", json!("soon")),
        ("token_endpoint", json!(null)),
    ] {
        let mut damaged = saved.clone();
        damaged[field] = value;
        fs::write(credentials.oauth_token_path(), damaged.to_string()).unwrap();
        assert_eq!(
            credentials.login_state(&oauth2()),
            LoginState::Unknown,
            "{field}"
        );
    }
    fs::write(credentials.oauth_token_path(), "{").unwrap();
    assert_eq!(credentials.login_state(&oauth2()), LoginState::Unknown);
    saved["unknown_field"] = json!(true);
    fs::write(credentials.oauth_token_path(), saved.to_string()).unwrap();
    assert_eq!(credentials.login_state(&oauth2()), LoginState::Expired);
    assert!(credentials.lock_oauth().unwrap().remove_token().unwrap());
    assert!(!credentials.lock_oauth().unwrap().remove_token().unwrap());
    assert_eq!(credentials.login_state(&oauth2()), LoginState::LoggedOut);
}

#[test]
fn an_oauth_token_is_saved_in_the_standard_fields_and_replaced_at_once() {
    let directory = tempfile::tempdir().unwrap();
    let credentials = PackageCredentials::in_config_root(directory.path(), "sample").unwrap();
    let mut full = token(1_900_000_000, Some("secret-refresh"));
    full.scope = Some("4 6".into());
    full.revocation_endpoint = Some("https://auth.example.com/revoke".into());

    let locked = credentials.lock_oauth().unwrap();
    locked.save_token(&full).unwrap();
    let saved: Value =
        serde_json::from_slice(&fs::read(credentials.oauth_token_path()).unwrap()).unwrap();
    assert_eq!(
        saved,
        json!({
            "access_token": "secret-access",
            "token_type": "Bearer",
            "refresh_token": "secret-refresh",
            "scope": "4 6",
            "issuer": "https://auth.example.com",
            "client_id": "client-1",
            "token_endpoint": "https://auth.example.com/token",
            "revocation_endpoint": "https://auth.example.com/revoke",
            "expires_at": 1_900_000_000,
        })
    );
    assert_eq!(locked.token().unwrap(), Some(full.clone()));

    let short = token(1_900_000_100, None);
    locked.save_token(&short).unwrap();
    let saved: Value =
        serde_json::from_slice(&fs::read(credentials.oauth_token_path()).unwrap()).unwrap();
    assert_eq!(
        saved.as_object().unwrap().keys().collect::<Vec<_>>(),
        [
            "access_token",
            "client_id",
            "expires_at",
            "issuer",
            "token_endpoint",
            "token_type"
        ]
    );
    assert_eq!(credentials.oauth_token().unwrap(), Some(short));
    assert_eq!(
        names_in(credentials.directory()),
        ["oauth2.json", "oauth2.lock"]
    );
    assert_eq!(mode(&credentials.oauth_token_path()), 0o600);

    let shown = format!("{full:?}");
    assert!(!shown.contains("secret"), "{shown}");
    assert!(shown.contains("has_refresh_token: true"), "{shown}");
}

#[test]
fn changes_to_the_oauth_files_wait_for_the_lock() {
    let directory = tempfile::tempdir().unwrap();
    let credentials = PackageCredentials::in_config_root(directory.path(), "sample").unwrap();
    let held = credentials.lock_oauth().unwrap();
    let (locked, waited) = mpsc::channel();
    let other = credentials.clone();
    let waiter = thread::spawn(move || {
        let locked_again = other.lock_oauth().unwrap();
        locked.send(()).unwrap();
        locked_again.save_token(&token(now() + 60, None)).unwrap();
    });
    assert!(
        waited.recv_timeout(Duration::from_millis(300)).is_err(),
        "a second lock waits while the first is held"
    );
    held.save_token(&token(now() + 30, Some("first"))).unwrap();
    drop(held);
    waited.recv_timeout(Duration::from_secs(5)).unwrap();
    waiter.join().unwrap();
    assert_eq!(
        credentials.oauth_token().unwrap().unwrap().refresh_token,
        None
    );
}

#[test]
fn the_client_registered_with_each_issuer_stays_after_a_logout() {
    let directory = tempfile::tempdir().unwrap();
    let credentials = PackageCredentials::in_config_root(directory.path(), "sample").unwrap();
    let locked = credentials.lock_oauth().unwrap();
    assert_eq!(locked.client_id("https://one.example.com").unwrap(), None);
    locked
        .set_client_id("https://one.example.com", Some("client-one"))
        .unwrap();
    locked
        .set_client_id("https://two.example.com", Some("client-two"))
        .unwrap();
    locked.save_token(&token(now() + 60, None)).unwrap();
    locked.remove_token().unwrap();
    assert_eq!(
        locked
            .client_id("https://one.example.com")
            .unwrap()
            .as_deref(),
        Some("client-one")
    );
    locked
        .set_client_id("https://one.example.com", None)
        .unwrap();
    assert_eq!(locked.client_id("https://one.example.com").unwrap(), None);
    assert_eq!(
        locked
            .client_id("https://two.example.com")
            .unwrap()
            .as_deref(),
        Some("client-two")
    );

    fs::write(credentials.directory().join("oauth2-clients.json"), "[").unwrap();
    assert_eq!(locked.client_id("https://two.example.com").unwrap(), None);
    locked
        .set_client_id("https://two.example.com", Some("client-three"))
        .unwrap();
    assert_eq!(
        locked
            .client_id("https://two.example.com")
            .unwrap()
            .as_deref(),
        Some("client-three")
    );
    assert_eq!(
        mode(&credentials.directory().join("oauth2-clients.json")),
        0o600
    );
}

#[test]
fn a_key_is_one_printable_line_and_never_shown() {
    assert_eq!(ApiKey::new("\tkey value \n").unwrap().expose(), "key value");
    for refused in ["", " \n", "key\u{1b}", "two\nlines"] {
        assert!(
            matches!(ApiKey::new(refused), Err(PackageError::Invalid(_))),
            "{refused:?}"
        );
    }
    ApiKey::new(&"k".repeat(4096)).unwrap();
    assert!(ApiKey::new(&"k".repeat(4097)).is_err());
    assert_eq!(
        format!("{:?}", ApiKey::new("secret").unwrap()),
        "ApiKey(..)"
    );
}

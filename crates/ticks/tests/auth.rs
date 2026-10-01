#[path = "support/http.rs"]
mod http;

use serde_json::json;
use std::io::Cursor;
use tickloop_ticks::{
    auth::{AuthEndpoints, run_login, storage::AuthStorage},
    config::Settings,
};

const SDK_REFRESH_WINDOW_SECONDS: u64 = 300;
const ACTIVE_TOKEN_LIFETIME_SECONDS: u64 = SDK_REFRESH_WINDOW_SECONDS + 3;

#[tokio::test]
async fn approval_is_received_after_a_dropped_connection_without_progress_flooding() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let ap_polls = AtomicUsize::new(0);
    let stub = http::Stub::start_with_disconnect(move |request| {
        if request.contains("/register ") {
            Some((201, json!({"client_id":"client"})))
        } else if request.contains("/device/authorize ") {
            Some((200, json!({"device_code":"secret","user_code":"CODE","verification_uri":"https://example.com","expires_in":120,"interval":1})))
        } else if request.contains("x-dc-region: ap") {
            if ap_polls.fetch_add(1, Ordering::SeqCst) == 0 {
                None
            } else {
                Some((200, json!({"access_token":"ap_authorized","expires_in":3600})))
            }
        } else {
            Some((400, json!({"error":"authorization_pending"})))
        }
    }).await;
    let home = tempfile::tempdir().unwrap();
    let storage = AuthStorage::in_home(home.path());
    let endpoints = AuthEndpoints {
        ap: format!("{}/oauth2", stub.url),
        us: format!("{}/oauth2", stub.url),
    };
    let mut output = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        run_login(
            &Settings::default(),
            &home.path().join("config.toml"),
            &storage,
            Some(endpoints),
            &mut Cursor::new("cn\n"),
            &mut output,
        ),
    )
    .await
    .expect("a brief connection failure must not abandon the approved center")
    .unwrap();
    assert_eq!(
        storage.token().unwrap().unwrap().access_token,
        "ap_authorized"
    );
    let output = String::from_utf8(output).unwrap();
    assert_eq!(output.matches("Waiting for Longbridge").count(), 1);
    assert_eq!(output.matches("retrying automatically").count(), 1);
    assert!(!output.contains("remaining"));
    assert!(output.contains("Logged in"));
}

#[tokio::test]
async fn successful_device_authorization_without_expiry_does_not_keep_waiting() {
    let stub = http::Stub::start(|request| {
        if request.contains("/register ") {
            (201, json!({"client_id":"client"}))
        } else if request.contains("/device/authorize ") {
            (200, json!({"device_code":"secret","user_code":"CODE","verification_uri":"https://example.com","expires_in":120,"interval":1}))
        } else if request.contains("x-dc-region: ap") {
            (200, json!({"access_token":"ap_authorized","refresh_token":"ap_refresh","expires_in":null}))
        } else {
            (400, json!({"error":"authorization_pending"}))
        }
    }).await;
    let home = tempfile::tempdir().unwrap();
    let storage = AuthStorage::in_home(home.path());
    let endpoints = AuthEndpoints {
        ap: format!("{}/oauth2", stub.url),
        us: format!("{}/oauth2", stub.url),
    };
    let mut output = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(4),
        run_login(
            &Settings::default(),
            &home.path().join("config.toml"),
            &storage,
            Some(endpoints),
            &mut Cursor::new("cn\n"),
            &mut output,
        ),
    )
    .await
    .expect("successful authorization must end the wait even without expires_in")
    .unwrap();
    let token = storage.token().unwrap().expect("save the approved token");
    assert_eq!(token.access_token, "ap_authorized");
    let lifetime = token
        .expires_at
        .saturating_sub(tickloop_ticks::auth::storage::now_seconds());
    assert!((3595..=3600).contains(&lifetime));
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("Logged in"));
    assert!(!output.contains("ap_authorized"));
}

#[tokio::test]
async fn prepared_region_allows_device_login_without_reading_stdin() {
    struct NoInput;
    impl std::io::Read for NoInput {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            panic!("asynchronous login must not read stdin")
        }
    }
    impl std::io::BufRead for NoInput {
        fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
            panic!("asynchronous login must not read stdin")
        }
        fn consume(&mut self, _: usize) {
            panic!("asynchronous login must not read stdin")
        }
    }
    let stub = http::Stub::start(|request| {
        if request.contains("/register ") { (201, json!({"client_id":"client"})) }
        else if request.contains("/device/authorize ") { (200, json!({"device_code":"secret","user_code":"CODE","verification_uri":"https://example.com","expires_in":30,"interval":1})) }
        else { (200, json!({"access_token":"ap_token","expires_in":3600})) }
    }).await;
    let home = tempfile::tempdir().unwrap();
    let storage = AuthStorage::in_home(home.path());
    let config_path = home.path().join("config.toml");
    std::fs::write(&config_path, "# Keep personal settings\nlanguage = 'en'\n").unwrap();
    let mut settings = Settings::load(&config_path, true).unwrap();
    let mut output = Vec::new();
    settings.region = Some(
        tickloop_ticks::auth::prepare_region(
            &settings,
            &config_path,
            &mut Cursor::new("cn\n"),
            &mut output,
        )
        .unwrap(),
    );
    assert!(
        String::from_utf8(output.clone())
            .unwrap()
            .contains("Select region")
    );
    assert!(storage.token().unwrap().is_none());
    assert!(stub.requests.lock().unwrap().is_empty());
    assert!(
        std::fs::read_to_string(&config_path)
            .unwrap()
            .contains("# Keep personal settings")
    );
    assert_eq!(
        Settings::load(&config_path, true).unwrap().language,
        tickloop_ticks::config::Language::En
    );
    output.clear();
    let endpoints = AuthEndpoints {
        ap: format!("{}/oauth2", stub.url),
        us: format!("{}/oauth2", stub.url),
    };
    run_login(
        &settings,
        &config_path,
        &storage,
        Some(endpoints),
        &mut NoInput,
        &mut output,
    )
    .await
    .unwrap();
    let output = String::from_utf8(output).unwrap();
    assert!(!output.contains("Select region"));
    assert!(output.contains("Logged in"));
}

#[tokio::test]
async fn device_login_registers_once_polls_both_centers_and_saves_private_token() {
    let stub = http::Stub::start(|request| {
        if request.starts_with("POST /oauth2/register ") {
            assert!(request.contains("Tick Loop"));
            (201, json!({"client_id":"test-client"}))
        } else if request.starts_with("POST /oauth2/device/authorize ") {
            assert!(request.contains("client_id=test-client"));
            (200, json!({"device_code":"device-secret","user_code":"ABCD-EFGH","verification_uri":"https://example.com/verify","expires_in":30,"interval":1}))
        } else if request.contains("x-dc-region: us") {
            assert!(request.contains("device_code=device-secret"));
            (200, json!({"access_token":"us_test-access","refresh_token":"us_test-refresh","expires_in":3600}))
        } else {
            (400, json!({"error":"authorization_pending"}))
        }
    }).await;
    let home = tempfile::tempdir().unwrap();
    let storage = AuthStorage::in_home(home.path());
    let config_path = home.path().join(".tickloop/config.toml");
    let endpoints = AuthEndpoints {
        ap: format!("{}/oauth2", stub.url),
        us: format!("{}/oauth2", stub.url),
    };
    for _ in 0..2 {
        let mut output = Vec::new();
        run_login(
            &Settings::load(&config_path, false).unwrap(),
            &config_path,
            &storage,
            Some(endpoints.clone()),
            &mut Cursor::new("global\n"),
            &mut output,
        )
        .await
        .unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("ABCD-EFGH"), "{output}");
        assert!(output.contains("https://example.com/verify"), "{output}");
        assert!(output.contains("30s"), "{output}");
        assert!(output.contains("Logged in"), "{output}");
        assert!(!output.contains("device-secret"));
        assert!(!output.contains("us_test-access"));
    }
    assert_eq!(
        storage.token().unwrap().unwrap().access_token,
        "us_test-access"
    );
    assert_eq!(
        Settings::load(&config_path, true).unwrap().region,
        Some(tickloop_ticks::config::Region::Global)
    );
    let requests = stub.requests.lock().unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.starts_with("POST /oauth2/register "))
            .count(),
        1
    );
    assert!(
        requests
            .iter()
            .any(|request| request.contains("x-dc-region: ap"))
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(storage.directory.join("auth.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}

#[tokio::test]
async fn polling_reports_denial_and_expiry_without_saving_a_token() {
    for (oauth_error, message) in [
        ("access_denied", "denied"),
        ("expired_token", "expired"),
        ("invalid_client", "invalid_client"),
        ("invalid_grant", "invalid_grant"),
    ] {
        let stub = http::Stub::start(move |request| {
            if request.contains("/register ") { (201, json!({"client_id":"client"})) }
            else if request.contains("/device/authorize ") { (200, json!({"device_code":"secret","user_code":"CODE","verification_uri":"https://example.com","expires_in":30,"interval":1})) }
            else if request.contains("x-dc-region: ap") { (400, json!({"error":oauth_error})) }
            else { (400, json!({"error":"authorization_pending"})) }
        }).await;
        let home = tempfile::tempdir().unwrap();
        let storage = AuthStorage::in_home(home.path());
        let endpoints = AuthEndpoints {
            ap: format!("{}/oauth2", stub.url),
            us: format!("{}/oauth2", stub.url),
        };
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(4),
            run_login(
                &Settings::default(),
                &home.path().join("config.toml"),
                &storage,
                Some(endpoints),
                &mut Cursor::new("cn\n"),
                &mut Vec::new(),
            ),
        )
        .await
        .expect("a terminal rejection must cancel the other poller");
        let error = result.unwrap_err();
        assert_eq!(error.code, 3);
        assert!(error.message.contains(message), "{error}");
        assert!(storage.token().unwrap().is_none());
    }
}

#[tokio::test]
async fn malformed_approved_token_is_reported_instead_of_hidden_by_pending_center() {
    let stub = http::Stub::start(|request| {
        if request.contains("/register ") {
            (201, json!({"client_id":"client"}))
        } else if request.contains("/device/authorize ") {
            (200, json!({"device_code":"secret","user_code":"CODE","verification_uri":"https://example.com","expires_in":120,"interval":1}))
        } else if request.contains("x-dc-region: ap") {
            (200, json!({"access_token":"ap_private","expires_in":0,"other":"private-response"}))
        } else {
            (400, json!({"error":"authorization_pending"}))
        }
    }).await;
    let home = tempfile::tempdir().unwrap();
    let storage = AuthStorage::in_home(home.path());
    let endpoints = AuthEndpoints {
        ap: format!("{}/oauth2", stub.url),
        us: format!("{}/oauth2", stub.url),
    };
    let mut output = Vec::new();
    let error = tokio::time::timeout(
        std::time::Duration::from_secs(4),
        run_login(
            &Settings::default(),
            &home.path().join("config.toml"),
            &storage,
            Some(endpoints),
            &mut Cursor::new("cn\n"),
            &mut output,
        ),
    )
    .await
    .expect("a rejected successful response must not silently wait for the other center")
    .unwrap_err();
    assert_eq!(error.code, 4);
    assert!(error.message.contains("invalid token response"));
    assert!(storage.token().unwrap().is_none());
    for private in ["ap_private", "private-response", "secret"] {
        assert!(!error.message.contains(private));
        assert!(!String::from_utf8_lossy(&output).contains(private));
    }
}

#[tokio::test]
async fn slow_down_increases_both_poll_intervals_by_five_seconds() {
    use std::{
        sync::{Arc, Mutex},
        time::Instant,
    };
    let slow_at = Arc::new(Mutex::new(None));
    let observed = slow_at.clone();
    let stub = http::Stub::start(move |request| {
        if request.contains("/register ") { (201, json!({"client_id":"client"})) }
        else if request.contains("/device/authorize ") { (200, json!({"device_code":"secret","user_code":"CODE","verification_uri":"https://example.com","expires_in":20,"interval":1})) }
        else if request.contains("x-dc-region: ap") {
            let mut at = observed.lock().unwrap();
            if at.is_none() { *at = Some(Instant::now()); }
            (400, json!({"error":"slow_down"}))
        } else if observed.lock().unwrap().is_some_and(|at| at.elapsed().as_secs() >= 5) {
            (200, json!({"access_token":"us_token","expires_in":3600}))
        } else { (400, json!({"error":"authorization_pending"})) }
    }).await;
    let home = tempfile::tempdir().unwrap();
    let endpoints = AuthEndpoints {
        ap: format!("{}/oauth2", stub.url),
        us: format!("{}/oauth2", stub.url),
    };
    let started = Instant::now();
    run_login(
        &Settings::default(),
        &home.path().join("config.toml"),
        &AuthStorage::in_home(home.path()),
        Some(endpoints),
        &mut Cursor::new("global\n"),
        &mut Vec::new(),
    )
    .await
    .unwrap();
    assert!(started.elapsed().as_secs() >= 7);
    let requests = stub.requests.lock().unwrap();
    assert!(
        requests
            .iter()
            .filter(|request| request.contains("/token ") && request.contains("x-dc-region: us"))
            .count()
            <= 2,
        "slow_down must slow the sibling poller too"
    );
}

#[tokio::test]
async fn sdk_loads_device_token_without_browser_and_refresh_preserves_login_time() {
    use longbridge::oauth::{StoredToken, TokenStorage};
    let home = tempfile::tempdir().unwrap();
    let storage = AuthStorage::in_home(home.path());
    std::fs::create_dir_all(&storage.directory).unwrap();
    std::fs::write(storage.directory.join("auth.json"), json!({"client_id":"client","access_token":"ap_token","refresh_token":"ap_refresh","expires_at":4102444800_u64,"logged_in_at":100}).to_string()).unwrap();
    let oauth = tickloop_ticks::auth::authorized_oauth(
        &storage,
        tickloop_ticks::config::Region::Global,
        None,
    )
    .await
    .unwrap();
    assert_eq!(oauth.access_token().await.unwrap(), "ap_token");
    storage
        .save(&StoredToken {
            client_id: "client".into(),
            access_token: "ap_refreshed".into(),
            refresh_token: Some("ap_next-refresh".into()),
            expires_at: 4102445800,
        })
        .unwrap();
    assert_eq!(storage.token().unwrap().unwrap().logged_in_at, 100);
    assert_eq!(storage.load("client").unwrap().access_token, "ap_refreshed");
}

#[tokio::test]
async fn expired_us_token_refreshes_at_us_gateway_with_dc_header() {
    let stub = http::Stub::start(|request| {
        assert!(request.starts_with("POST /oauth2/token "));
        if request.contains("x-dc-region: us") && request.contains("grant_type=refresh_token") {
            (200, json!({"access_token":"us_fresh","expires_in":3600}))
        } else {
            (400, json!({"error":"invalid_grant"}))
        }
    })
    .await;
    let home = tempfile::tempdir().unwrap();
    let storage = AuthStorage::in_home(home.path());
    std::fs::create_dir_all(&storage.directory).unwrap();
    std::fs::write(storage.directory.join("auth.json"), json!({"client_id":"client","access_token":"us_old","refresh_token":"us_refresh","expires_at":1,"logged_in_at":100}).to_string()).unwrap();
    let endpoints = AuthEndpoints {
        ap: "http://127.0.0.1:1/unreachable".into(),
        us: format!("{}/oauth2", stub.url),
    };
    let oauth = tickloop_ticks::auth::authorized_oauth(
        &storage,
        tickloop_ticks::config::Region::Cn,
        Some(endpoints),
    )
    .await
    .unwrap();
    assert_eq!(oauth.access_token().await.unwrap(), "us_fresh");
    let token = storage.token().unwrap().unwrap();
    assert_eq!(token.logged_in_at, 100);
    assert_eq!(token.refresh_token.as_deref(), Some("us_refresh"));
}

#[tokio::test]
async fn running_us_oauth_refreshes_at_us_gateway_and_persists_the_new_token() {
    use longbridge::oauth::{StoredToken, TokenStorage};
    use tickloop_ticks::{
        auth::{authorized_oauth, storage::now_seconds},
        config::Region,
    };

    let ap = http::Stub::start(|_| (400, json!({"error":"invalid_grant"}))).await;
    let us = http::Stub::start(|request| {
        if request.starts_with("POST /oauth2/token ")
            && request.contains("x-dc-region: us")
            && request.contains("grant_type=refresh_token")
            && request.contains("refresh_token=us_refresh")
        {
            (200, json!({"access_token":"us_renewed","refresh_token":"us_rotated","token_type":"Bearer","expires_in":3600}))
        } else {
            (400, json!({"error":"invalid_grant"}))
        }
    }).await;
    let home = tempfile::tempdir().unwrap();
    let storage = AuthStorage::in_home(home.path());
    let expires_at = now_seconds() + ACTIVE_TOKEN_LIFETIME_SECONDS;
    storage
        .save(&StoredToken {
            client_id: "client".into(),
            access_token: "us_initial".into(),
            refresh_token: Some("us_refresh".into()),
            expires_at,
        })
        .unwrap();
    let logged_in_at = storage.token().unwrap().unwrap().logged_in_at;
    let endpoints = AuthEndpoints {
        ap: format!("{}/oauth2", ap.url),
        us: format!("{}/oauth2", us.url),
    };
    let oauth = authorized_oauth(&storage, Region::Cn, Some(endpoints))
        .await
        .unwrap();
    assert_eq!(oauth.access_token().await.unwrap(), "us_initial");

    while now_seconds() <= expires_at - SDK_REFRESH_WINDOW_SECONDS {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let token = tokio::time::timeout(std::time::Duration::from_secs(2), oauth.access_token())
        .await
        .expect("running refresh must finish without a browser callback")
        .unwrap();
    assert_eq!(token, "us_renewed");
    assert!(
        ap.requests.lock().unwrap().is_empty(),
        "US credentials must not reach AP"
    );
    assert_eq!(us.requests.lock().unwrap().len(), 1);
    let saved = storage.token().unwrap().unwrap();
    assert_eq!(saved.access_token, "us_renewed");
    assert_eq!(saved.refresh_token.as_deref(), Some("us_rotated"));
    assert_eq!(saved.logged_in_at, logged_in_at);
}

#[tokio::test]
async fn running_ap_oauth_preserves_refresh_token_and_reports_revocation_without_browser_fallback()
{
    use longbridge::oauth::{StoredToken, TokenStorage};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tickloop_ticks::{
        auth::{authorized_oauth, storage::now_seconds},
        config::Region,
    };

    let refreshes = Arc::new(AtomicUsize::new(0));
    let observed = refreshes.clone();
    let ap = http::Stub::start(move |request| {
        assert!(request.starts_with("POST /oauth2/token "));
        assert!(request.contains("x-dc-region: ap"));
        assert!(request.contains("refresh_token=ap_refresh"));
        if observed.fetch_add(1, Ordering::SeqCst) == 0 {
            (200, json!({"access_token":"ap_renewed","token_type":"Bearer","expires_in":1}))
        } else {
            (400, json!({"error":"invalid_grant","error_description":"revoked ap_refresh ap_renewed"}))
        }
    }).await;
    let us = http::Stub::start(|_| (400, json!({"error":"invalid_grant"}))).await;
    let home = tempfile::tempdir().unwrap();
    let storage = AuthStorage::in_home(home.path());
    let expires_at = now_seconds() + ACTIVE_TOKEN_LIFETIME_SECONDS;
    storage
        .save(&StoredToken {
            client_id: "client".into(),
            access_token: "ap_initial".into(),
            refresh_token: Some("ap_refresh".into()),
            expires_at,
        })
        .unwrap();
    let endpoints = AuthEndpoints {
        ap: format!("{}/oauth2", ap.url),
        us: format!("{}/oauth2", us.url),
    };
    let oauth = authorized_oauth(&storage, Region::Global, Some(endpoints))
        .await
        .unwrap();
    assert_eq!(oauth.access_token().await.unwrap(), "ap_initial");

    while now_seconds() <= expires_at - SDK_REFRESH_WINDOW_SECONDS {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(oauth.access_token().await.unwrap(), "ap_renewed");
    let error = tokio::time::timeout(std::time::Duration::from_secs(2), oauth.access_token())
        .await
        .expect("revoked authorization must fail without a browser callback")
        .unwrap_err();
    assert!(matches!(
        error,
        longbridge::oauth::OAuthError::RefreshTokenFailed { .. }
    ));
    assert!(!error.to_string().contains("ap_refresh"));
    assert!(!error.to_string().contains("ap_renewed"));
    let saved = storage.token().unwrap().unwrap();
    assert_eq!(saved.access_token, "ap_renewed");
    assert_eq!(saved.refresh_token.as_deref(), Some("ap_refresh"));
    assert!(
        us.requests.lock().unwrap().is_empty(),
        "AP credentials must not reach US"
    );
    assert_eq!(refreshes.load(Ordering::SeqCst), 2);
}

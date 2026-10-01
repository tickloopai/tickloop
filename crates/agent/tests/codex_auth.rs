use std::os::unix::fs::PermissionsExt;
use std::os::unix::fs::symlink;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tickloop_agent::auth::{AuthError, CodexAuth, Credentials};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

const ACCESS_TOKEN: &str = "ey.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjdC10ZXN0In19.sig";
const NEW_ACCESS_TOKEN: &str = "ey.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjdC1uZXcifX0.sig";

async fn read_http_request(stream: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    loop {
        let mut chunk = [0_u8; 4096];
        let count = stream.read(&mut chunk).await.unwrap();
        assert!(count > 0, "client closed before sending a complete request");
        bytes.extend_from_slice(&chunk[..count]);
        assert!(
            bytes.len() < 64 * 1024,
            "test request is unexpectedly large"
        );
        if let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&bytes[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            if bytes.len() >= header_end + 4 + content_length {
                break;
            }
        }
    }
    String::from_utf8(bytes).unwrap()
}

async fn oauth_server() -> (String, tokio::task::JoinHandle<()>) {
    oauth_server_for_account(ACCESS_TOKEN).await
}

async fn oauth_server_for_account(
    access_token: &'static str,
) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        for _ in 0..3 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_http_request(&mut stream).await;
            let body = if request.starts_with("POST /api/accounts/deviceauth/usercode ") {
                r#"{"device_auth_id":"device-secret","user_code":"ABCD-EFGH","interval":0}"#
                    .to_owned()
            } else if request.starts_with("POST /api/accounts/deviceauth/token ") {
                r#"{"authorization_code":"one-time-code","code_verifier":"verifier"}"#.to_owned()
            } else if request.starts_with("POST /oauth/token ") {
                assert!(request.contains("authorization_code"));
                format!(
                    "{{\"access_token\":\"{access_token}\",\"refresh_token\":\"refresh-secret\",\"expires_in\":3600}}"
                )
            } else {
                panic!(
                    "unexpected OAuth request: {}",
                    request.lines().next().unwrap_or("")
                );
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
        }
    });
    (origin, task)
}

async fn oauth_server_with_refresh() -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    oauth_server_with_refresh_gate(None).await
}

struct RefreshGate {
    started: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
}

async fn oauth_server_with_refresh_gate(
    gate: Option<RefreshGate>,
) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let refreshes = Arc::new(AtomicUsize::new(0));
    let observed = refreshes.clone();
    let task = tokio::spawn(async move {
        let mut gate = gate;
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_http_request(&mut stream).await;
            let body = if request.starts_with("POST /api/accounts/deviceauth/usercode ") {
                r#"{"device_auth_id":"device-secret","user_code":"ABCD-EFGH","interval":0}"#
                    .to_owned()
            } else if request.starts_with("POST /api/accounts/deviceauth/token ") {
                r#"{"authorization_code":"one-time-code","code_verifier":"verifier"}"#.to_owned()
            } else if request.starts_with("POST /oauth/token ") {
                let is_refresh = request.contains("refresh_token=refresh-secret");
                if is_refresh {
                    observed.fetch_add(1, Ordering::SeqCst);
                    if let Some(gate) = gate.take() {
                        let _ = gate.started.send(());
                        let _ = gate.release.await;
                    } else {
                        tokio::time::sleep(Duration::from_millis(300)).await;
                    }
                }
                let expires_in = if is_refresh { 3600 } else { 1 };
                format!(
                    "{{\"access_token\":\"{ACCESS_TOKEN}\",\"refresh_token\":\"refresh-secret\",\"expires_in\":{expires_in}}}"
                )
            } else {
                panic!(
                    "unexpected OAuth request: {}",
                    request.lines().next().unwrap_or("")
                );
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
        }
    });
    (origin, refreshes, task)
}

#[test]
fn credentials_debug_redacts_token_and_account_id() {
    let credentials = Credentials {
        access_token: "sensitive-token".into(),
        account_id: "sensitive-account".into(),
        expires_at_ms: 1,
    };
    let printed = format!("{credentials:?}");
    assert!(!printed.contains("sensitive-token"));
    assert!(!printed.contains("sensitive-account"));
    assert!(printed.contains("[redacted]"));
}

#[tokio::test]
async fn device_login_saves_private_credentials_for_next_request_and_logout_removes_them() {
    let directory = tempfile::tempdir().unwrap();
    let state_dir = directory.path().canonicalize().unwrap();
    let (origin, server) = oauth_server().await;
    let auth = CodexAuth::new_for_loopback_test(&state_dir, &origin).unwrap();
    let cancellation = CancellationToken::new();

    let device = auth.start_device_login(&cancellation).await.unwrap();
    assert_eq!(device.user_code, "ABCD-EFGH");
    assert_eq!(device.verification_uri, format!("{origin}/codex/device"));
    let credentials = auth
        .finish_device_login(device, &cancellation)
        .await
        .unwrap();
    assert_eq!(credentials.account_id, "acct-test");
    assert_eq!(credentials.access_token, ACCESS_TOKEN);
    assert!(!format!("{credentials:?}").contains(ACCESS_TOKEN));

    let file = state_dir.join("auth/openai-codex-credentials.json");
    assert_eq!(
        std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let next_chat = CodexAuth::new_for_loopback_test(&state_dir, &origin).unwrap();
    assert_eq!(
        next_chat
            .resolve_credentials(&cancellation)
            .await
            .unwrap()
            .account_id,
        "acct-test"
    );
    next_chat.logout(&cancellation).await.unwrap();
    assert!(!file.exists());
    assert!(next_chat.resolve_credentials(&cancellation).await.is_err());
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn simultaneous_chats_refresh_the_shared_login_once() {
    let directory = tempfile::tempdir().unwrap();
    let state_dir = directory.path().canonicalize().unwrap();
    let (origin, refreshes, server) = oauth_server_with_refresh().await;
    let first_chat = CodexAuth::new_for_loopback_test(&state_dir, &origin).unwrap();
    let second_chat = CodexAuth::new_for_loopback_test(&state_dir, &origin).unwrap();
    let cancellation = CancellationToken::new();
    let device = first_chat.start_device_login(&cancellation).await.unwrap();
    first_chat
        .finish_device_login(device, &cancellation)
        .await
        .unwrap();

    let (first, second) = tokio::join!(
        first_chat.resolve_credentials(&cancellation),
        second_chat.resolve_credentials(&cancellation),
    );
    assert_eq!(first.unwrap().account_id, "acct-test");
    assert_eq!(second.unwrap().account_id, "acct-test");
    assert_eq!(refreshes.load(Ordering::SeqCst), 1);
    server.abort();
}

#[tokio::test]
async fn refresh_gate_is_used_only_when_a_refresh_request_is_needed() {
    let fresh_state = tempfile::tempdir().unwrap();
    let fresh_path = fresh_state.path().canonicalize().unwrap();
    let (fresh_origin, fresh_server) = oauth_server().await;
    let fresh_auth = CodexAuth::new_for_loopback_test(&fresh_path, &fresh_origin).unwrap();
    let cancellation = CancellationToken::new();
    let device = fresh_auth.start_device_login(&cancellation).await.unwrap();
    fresh_auth
        .finish_device_login(device, &cancellation)
        .await
        .unwrap();
    let fresh = fresh_auth
        .resolve_credentials_with_refresh_gate(&cancellation, || {
            panic!("fresh credentials do not need a refresh attempt")
        })
        .await
        .unwrap();
    assert_eq!(fresh.account_id, "acct-test");
    fresh_server.await.unwrap();

    let expired_state = tempfile::tempdir().unwrap();
    let expired_path = expired_state.path().canonicalize().unwrap();
    let (expired_origin, refreshes, expired_server) = oauth_server_with_refresh().await;
    let expired_auth = CodexAuth::new_for_loopback_test(&expired_path, &expired_origin).unwrap();
    let device = expired_auth
        .start_device_login(&cancellation)
        .await
        .unwrap();
    expired_auth
        .finish_device_login(device, &cancellation)
        .await
        .unwrap();
    let calls = AtomicUsize::new(0);
    let error = expired_auth
        .resolve_credentials_with_refresh_gate(&cancellation, || {
            calls.fetch_add(1, Ordering::SeqCst);
            false
        })
        .await
        .unwrap_err();
    assert!(matches!(error, AuthError::RefreshBudgetExhausted));
    assert!(!error.to_string().contains("/login"));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(refreshes.load(Ordering::SeqCst), 0);
    expired_server.abort();
}

#[tokio::test]
async fn concurrent_refresh_rechecks_credentials_before_spending_another_attempt() {
    let directory = tempfile::tempdir().unwrap();
    let state_dir = directory.path().canonicalize().unwrap();
    let (started_sender, started_receiver) = oneshot::channel();
    let (release_sender, release_receiver) = oneshot::channel();
    let (origin, refreshes, server) = oauth_server_with_refresh_gate(Some(RefreshGate {
        started: started_sender,
        release: release_receiver,
    }))
    .await;
    let auth = CodexAuth::new_for_loopback_test(&state_dir, &origin).unwrap();
    let cancellation = CancellationToken::new();
    let device = auth.start_device_login(&cancellation).await.unwrap();
    auth.finish_device_login(device, &cancellation)
        .await
        .unwrap();

    let first_auth = auth.clone();
    let first = tokio::spawn(async move {
        first_auth
            .resolve_credentials_with_refresh_gate(&CancellationToken::new(), || true)
            .await
    });
    tokio::time::timeout(Duration::from_secs(3), started_receiver)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(refreshes.load(Ordering::SeqCst), 1);

    let gate_calls = Arc::new(AtomicUsize::new(0));
    let waiting_calls = gate_calls.clone();
    let waiting_auth = auth.clone();
    let waiting = tokio::spawn(async move {
        waiting_auth
            .resolve_credentials_with_refresh_gate(&CancellationToken::new(), || {
                waiting_calls.fetch_add(1, Ordering::SeqCst);
                false
            })
            .await
    });
    tokio::task::yield_now().await;
    assert!(!waiting.is_finished());

    let cancelled_auth = auth.clone();
    let cancelled_token = CancellationToken::new();
    let task_token = cancelled_token.clone();
    let cancelled = tokio::spawn(async move {
        cancelled_auth
            .resolve_credentials_with_refresh_gate(&task_token, || {
                panic!("a cancelled lock waiter must not spend a refresh attempt")
            })
            .await
    });
    tokio::task::yield_now().await;
    cancelled_token.cancel();
    let error = tokio::time::timeout(Duration::from_secs(3), cancelled)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(matches!(error, AuthError::Cancelled));
    assert_eq!(refreshes.load(Ordering::SeqCst), 1);

    release_sender.send(()).unwrap();
    let first_credentials = tokio::time::timeout(Duration::from_secs(3), first)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let waiting_credentials = tokio::time::timeout(Duration::from_secs(3), waiting)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(first_credentials.account_id, waiting_credentials.account_id);
    assert_eq!(gate_calls.load(Ordering::SeqCst), 0);
    assert_eq!(refreshes.load(Ordering::SeqCst), 1);
    server.abort();
}

#[tokio::test]
async fn cancelled_device_login_does_not_save_credentials() {
    let directory = tempfile::tempdir().unwrap();
    let state_dir = directory.path().canonicalize().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0_u8; 2048];
        let bytes = stream.read(&mut request).await.unwrap();
        assert!(request[..bytes].starts_with(b"POST /api/accounts/deviceauth/usercode "));
        let body = r#"{"device_auth_id":"device-secret","user_code":"ABCD-EFGH","interval":0}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
    });
    let auth = CodexAuth::new_for_loopback_test(&state_dir, &origin).unwrap();
    let cancellation = CancellationToken::new();
    let device = auth.start_device_login(&cancellation).await.unwrap();
    cancellation.cancel();
    let error = auth
        .finish_device_login(device, &cancellation)
        .await
        .unwrap_err();
    assert!(matches!(error, tickloop_agent::auth::AuthError::Cancelled));
    assert!(
        auth.resolve_credentials(&CancellationToken::new())
            .await
            .is_err()
    );
    server.await.unwrap();
}

#[tokio::test]
async fn login_rejects_symlinked_credential_file_without_changing_its_target() {
    let directory = tempfile::tempdir().unwrap();
    let state_dir = directory.path().canonicalize().unwrap();
    let auth_dir = state_dir.join("auth");
    std::fs::create_dir(&auth_dir).unwrap();
    std::fs::set_permissions(&auth_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let target = state_dir.join("unrelated.json");
    std::fs::write(&target, "leave this alone").unwrap();
    symlink(&target, auth_dir.join("openai-codex-credentials.json")).unwrap();
    let (origin, server) = oauth_server().await;
    let auth = CodexAuth::new_for_loopback_test(&state_dir, &origin).unwrap();
    let cancellation = CancellationToken::new();

    let device = auth.start_device_login(&cancellation).await.unwrap();
    let error = auth
        .finish_device_login(device, &cancellation)
        .await
        .unwrap_err();
    let message = error.to_string();
    assert!(!message.contains(ACCESS_TOKEN));
    assert!(!message.contains("refresh-secret"));
    assert_eq!(std::fs::read_to_string(target).unwrap(), "leave this alone");
    assert!(auth.resolve_credentials(&cancellation).await.is_err());
    server.await.unwrap();
}

#[tokio::test]
async fn oauth_failure_keeps_existing_login_and_hides_response_body() {
    let directory = tempfile::tempdir().unwrap();
    let state_dir = directory.path().canonicalize().unwrap();
    let (origin, first_server) = oauth_server().await;
    let auth = CodexAuth::new_for_loopback_test(&state_dir, &origin).unwrap();
    let cancellation = CancellationToken::new();
    let device = auth.start_device_login(&cancellation).await.unwrap();
    auth.finish_device_login(device, &cancellation)
        .await
        .unwrap();
    first_server.await.unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let failed_origin = format!("http://{}", listener.local_addr().unwrap());
    let failed_server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0_u8; 2048];
        let bytes = stream.read(&mut request).await.unwrap();
        assert!(request[..bytes].starts_with(b"POST /api/accounts/deviceauth/usercode "));
        let body = r#"{"error":"rejected","access_token":"raw-secret-must-stay-hidden"}"#;
        let response = format!(
            "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
    });
    let failed_login = CodexAuth::new_for_loopback_test(&state_dir, &failed_origin).unwrap();
    let error = failed_login
        .start_device_login(&cancellation)
        .await
        .unwrap_err();
    assert!(!error.to_string().contains("raw-secret-must-stay-hidden"));
    assert_eq!(
        auth.resolve_credentials(&cancellation)
            .await
            .unwrap()
            .account_id,
        "acct-test"
    );
    failed_server.await.unwrap();
}

#[tokio::test]
async fn browser_login_checks_state_and_exchanges_pkce_code_after_callback() {
    let directory = tempfile::tempdir().unwrap();
    let state_dir = directory.path().canonicalize().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let token_server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = read_http_request(&mut stream).await;
        assert!(request.starts_with("POST /oauth/token "));
        assert!(request.contains("code=browser-code"));
        assert!(request.contains("code_verifier="));
        let body = format!(
            "{{\"access_token\":\"{ACCESS_TOKEN}\",\"refresh_token\":\"refresh-secret\",\"expires_in\":3600}}"
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
        request
    });
    let auth = CodexAuth::new_for_loopback_test(&state_dir, &origin).unwrap();
    let cancellation = CancellationToken::new();
    let login = auth.start_browser_login(&cancellation).await.unwrap();
    let authorize = reqwest::Url::parse(&login.authorization_url).unwrap();
    let state = authorize
        .query_pairs()
        .find(|(name, _)| name == "state")
        .unwrap()
        .1
        .into_owned();
    assert_eq!(
        authorize
            .query_pairs()
            .find(|(name, _)| name == "code_challenge_method")
            .unwrap()
            .1,
        "S256"
    );
    let callback = reqwest::Url::parse(&login.callback_uri).unwrap();
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", callback.port().unwrap()))
        .await
        .unwrap();
    let wrong = "GET /auth/callback?code=bad&state=wrong HTTP/1.1\r\nHost: localhost\r\n\r\n";
    stream.write_all(wrong.as_bytes()).await.unwrap();
    let mut wrong_response = Vec::new();
    stream.read_to_end(&mut wrong_response).await.unwrap();
    let wrong_response = String::from_utf8(wrong_response).unwrap();
    assert!(wrong_response.starts_with("HTTP/1.1 400"));
    assert!(wrong_response.contains("belongs to a different sign-in flow"));
    assert!(wrong_response.contains("/model → Provider"));
    assert!(!wrong_response.contains("state=wrong"));

    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", callback.port().unwrap()))
        .await
        .unwrap();
    let missing_code =
        format!("GET /auth/callback?state={state} HTTP/1.1\r\nHost: localhost\r\n\r\n");
    stream.write_all(missing_code.as_bytes()).await.unwrap();
    let mut missing_code_response = Vec::new();
    stream
        .read_to_end(&mut missing_code_response)
        .await
        .unwrap();
    let missing_code_response = String::from_utf8(missing_code_response).unwrap();
    assert!(missing_code_response.starts_with("HTTP/1.1 400"));
    assert!(missing_code_response.contains("missing an authorization code"));
    assert!(missing_code_response.contains("/model → Provider"));
    assert!(!missing_code_response.contains(&state));

    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", callback.port().unwrap()))
        .await
        .unwrap();
    let malformed =
        format!("POST /auth/callback?code=bad&state={state} HTTP/1.1\r\nHost: localhost\r\n\r\n");
    stream.write_all(malformed.as_bytes()).await.unwrap();
    let mut malformed_response = Vec::new();
    stream.read_to_end(&mut malformed_response).await.unwrap();
    let malformed_response = String::from_utf8(malformed_response).unwrap();
    assert!(malformed_response.starts_with("HTTP/1.1 400"));
    assert!(malformed_response.contains("Could not read the browser callback"));
    assert!(!malformed_response.contains(&state));

    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", callback.port().unwrap()))
        .await
        .unwrap();
    let valid = format!(
        "GET /auth/callback?code=browser-code&state={state} HTTP/1.1\r\nHost: localhost\r\n\r\n"
    );
    stream.write_all(valid.as_bytes()).await.unwrap();
    let mut response = [0_u8; 1024];
    let bytes = stream.read(&mut response).await.unwrap();
    assert!(response[..bytes].starts_with(b"HTTP/1.1 200"));
    let mut duplicate = tokio::net::TcpStream::connect(("127.0.0.1", callback.port().unwrap()))
        .await
        .unwrap();
    duplicate.write_all(valid.as_bytes()).await.unwrap();
    let bytes = duplicate.read(&mut response).await.unwrap();
    assert!(response[..bytes].starts_with(b"HTTP/1.1 409"));
    let credentials = auth
        .finish_browser_login(login, &cancellation)
        .await
        .unwrap();
    assert_eq!(credentials.account_id, "acct-test");
    let request = token_server.await.unwrap();
    let verifier = request
        .split("code_verifier=")
        .nth(1)
        .unwrap()
        .split('&')
        .next()
        .unwrap();
    assert!(verifier.len() >= 43);
    let challenge = authorize
        .query_pairs()
        .find(|(name, _)| name == "code_challenge")
        .unwrap()
        .1;
    assert_eq!(
        challenge,
        test_base64url(&Sha256::digest(verifier.as_bytes()))
    );
}

#[tokio::test]
async fn browser_callback_reports_code_received_when_exchange_fails() {
    let directory = tempfile::tempdir().unwrap();
    let state_dir = directory.path().canonicalize().unwrap();
    let (initial_origin, initial_server) = oauth_server().await;
    let initial_auth = CodexAuth::new_for_loopback_test(&state_dir, &initial_origin).unwrap();
    let cancellation = CancellationToken::new();
    let device = initial_auth
        .start_device_login(&cancellation)
        .await
        .unwrap();
    initial_auth
        .finish_device_login(device, &cancellation)
        .await
        .unwrap();
    initial_server.await.unwrap();
    let credential_file = state_dir.join("auth/openai-codex-credentials.json");
    let saved_before = std::fs::read(&credential_file).unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let failed_origin = format!("http://{}", listener.local_addr().unwrap());
    let failed_server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = read_http_request(&mut stream).await;
        assert!(request.starts_with("POST /oauth/token "));
        assert!(request.contains("code=fake-browser-code"));
        let body = r#"{"error":"invalid_grant","secret":"raw-secret-must-stay-hidden"}"#;
        stream
            .write_all(
                format!(
                    "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
    });
    let failed_auth = CodexAuth::new_for_loopback_test(&state_dir, &failed_origin).unwrap();
    let login = failed_auth
        .start_browser_login(&cancellation)
        .await
        .unwrap();
    let state = reqwest::Url::parse(&login.authorization_url)
        .unwrap()
        .query_pairs()
        .find(|(name, _)| name == "state")
        .unwrap()
        .1
        .into_owned();
    let callback = reqwest::Url::parse(&login.callback_uri).unwrap();
    let mut stream = TcpStream::connect(("127.0.0.1", callback.port().unwrap()))
        .await
        .unwrap();
    let request = format!(
        "GET /auth/callback?code=fake-browser-code&state={state} HTTP/1.1\r\nHost: localhost\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut browser_response = Vec::new();
    stream.read_to_end(&mut browser_response).await.unwrap();
    let browser_response = String::from_utf8(browser_response).unwrap();
    assert!(browser_response.starts_with("HTTP/1.1 200"));
    assert!(browser_response.contains("Authorization code received"));
    assert!(browser_response.contains("Return to Tick Loop"));
    assert!(!browser_response.contains("authentication completed"));
    assert!(!browser_response.contains("fake-browser-code"));
    assert!(!browser_response.contains(&state));

    let error = failed_auth
        .finish_browser_login(login, &cancellation)
        .await
        .unwrap_err();
    assert!(!error.to_string().contains("raw-secret-must-stay-hidden"));
    assert_eq!(std::fs::read(&credential_file).unwrap(), saved_before);
    assert_eq!(
        initial_auth
            .resolve_credentials(&cancellation)
            .await
            .unwrap()
            .account_id,
        "acct-test"
    );
    failed_server.await.unwrap();
}

#[tokio::test]
async fn browser_callback_accepts_absolute_form_request_target() {
    let directory = tempfile::tempdir().unwrap();
    let state_dir = directory.path().canonicalize().unwrap();
    let auth = CodexAuth::new_for_loopback_test(&state_dir, "http://127.0.0.1:12345").unwrap();
    let cancellation = CancellationToken::new();
    let login = auth.start_browser_login(&cancellation).await.unwrap();
    let state = reqwest::Url::parse(&login.authorization_url)
        .unwrap()
        .query_pairs()
        .find(|(name, _)| name == "state")
        .unwrap()
        .1
        .into_owned();
    let callback = reqwest::Url::parse(&login.callback_uri).unwrap();
    let mut stream = TcpStream::connect(("127.0.0.1", callback.port().unwrap()))
        .await
        .unwrap();
    let wrong_origin = format!(
        "GET http://example.invalid:{}/auth/callback?code=fake-code&state={state} HTTP/1.1\r\nHost: localhost\r\n\r\n",
        callback.port().unwrap()
    );
    stream.write_all(wrong_origin.as_bytes()).await.unwrap();
    let mut response = [0_u8; 256];
    let count = stream.read(&mut response).await.unwrap();
    assert!(response[..count].starts_with(b"HTTP/1.1 400"));

    let mut stream = TcpStream::connect(("127.0.0.1", callback.port().unwrap()))
        .await
        .unwrap();
    let request = format!(
        "GET {}?code=fake-code&state={state} HTTP/1.1\r\nHost: localhost\r\n\r\n",
        login.callback_uri
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let count = stream.read(&mut response).await.unwrap();
    assert!(
        response[..count].starts_with(b"HTTP/1.1 200"),
        "{}",
        String::from_utf8_lossy(&response[..count])
    );
}

#[tokio::test]
async fn browser_callback_parses_only_the_request_line_as_utf8() {
    let directory = tempfile::tempdir().unwrap();
    let state_dir = directory.path().canonicalize().unwrap();
    let auth = CodexAuth::new_for_loopback_test(&state_dir, "http://127.0.0.1:12345").unwrap();
    let cancellation = CancellationToken::new();
    let login = auth.start_browser_login(&cancellation).await.unwrap();
    let state = reqwest::Url::parse(&login.authorization_url)
        .unwrap()
        .query_pairs()
        .find(|(name, _)| name == "state")
        .unwrap()
        .1
        .into_owned();
    let callback = reqwest::Url::parse(&login.callback_uri).unwrap();
    let mut stream = TcpStream::connect(("127.0.0.1", callback.port().unwrap()))
        .await
        .unwrap();
    let mut request = format!(
        "GET /auth/callback?code=fake-code&state={state} HTTP/1.1\r\nHost: localhost\r\nX-Opaque: "
    )
    .into_bytes();
    request.extend_from_slice(b"\xff\r\n\r\n");
    stream.write_all(&request).await.unwrap();
    let mut response = [0_u8; 256];
    let count = stream.read(&mut response).await.unwrap();
    assert!(
        response[..count].starts_with(b"HTTP/1.1 200"),
        "{}",
        String::from_utf8_lossy(&response[..count])
    );
}

fn test_base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut output = String::new();
    for chunk in bytes.chunks(3) {
        let word = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        let count = match chunk.len() {
            1 => 2,
            2 => 3,
            _ => 4,
        };
        for shift in [18, 12, 6, 0].into_iter().take(count) {
            output.push(ALPHABET[((word >> shift) & 63) as usize] as char);
        }
    }
    output
}

#[tokio::test]
async fn manual_browser_callback_requires_full_matching_url() {
    let directory = tempfile::tempdir().unwrap();
    let state_dir = directory.path().canonicalize().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let token_server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = read_http_request(&mut stream).await;
        assert!(request.contains("code=manual-code"));
        let body = format!(
            "{{\"access_token\":\"{ACCESS_TOKEN}\",\"refresh_token\":\"refresh-secret\",\"expires_in\":3600}}"
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
    });
    let auth = CodexAuth::new_for_loopback_test(&state_dir, &origin).unwrap();
    let cancellation = CancellationToken::new();
    let login = auth.start_browser_login(&cancellation).await.unwrap();
    let callback = login.manual_callback();
    let state = reqwest::Url::parse(&login.authorization_url)
        .unwrap()
        .query_pairs()
        .find(|(name, _)| name == "state")
        .unwrap()
        .1
        .into_owned();
    assert!(matches!(
        callback.submit("manual-code"),
        Err(tickloop_agent::auth::AuthError::InvalidCallback)
    ));
    assert!(matches!(
        callback.submit(&format!(
            "{}?code=manual-code&state=wrong",
            login.callback_uri
        )),
        Err(tickloop_agent::auth::AuthError::StateMismatch)
    ));
    callback
        .submit(&format!(
            "{}?code=manual-code&state={state}",
            login.callback_uri
        ))
        .unwrap();
    assert!(matches!(
        callback.submit(&format!(
            "{}?code=manual-code&state={state}",
            login.callback_uri
        )),
        Err(tickloop_agent::auth::AuthError::BrowserLoginEnded)
    ));
    assert_eq!(
        auth.finish_browser_login(login, &cancellation)
            .await
            .unwrap()
            .account_id,
        "acct-test"
    );
    token_server.await.unwrap();
}

#[tokio::test]
async fn browser_login_reports_callback_port_conflict() {
    let directory = tempfile::tempdir().unwrap();
    let state_dir = directory.path().canonicalize().unwrap();
    let occupied = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = occupied.local_addr().unwrap().port();
    let auth = CodexAuth::new_for_loopback_test_with_callback_port(
        &state_dir,
        "http://127.0.0.1:12345",
        port,
    )
    .unwrap();
    let error = auth
        .start_browser_login(&CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        tickloop_agent::auth::AuthError::CallbackPortUnavailable
    ));
}

#[tokio::test]
async fn browser_error_callback_reports_denial_without_saving() {
    let directory = tempfile::tempdir().unwrap();
    let state_dir = directory.path().canonicalize().unwrap();
    let auth = CodexAuth::new_for_loopback_test(&state_dir, "http://127.0.0.1:12345").unwrap();
    let cancellation = CancellationToken::new();
    let login = auth.start_browser_login(&cancellation).await.unwrap();
    let state = reqwest::Url::parse(&login.authorization_url)
        .unwrap()
        .query_pairs()
        .find(|(name, _)| name == "state")
        .unwrap()
        .1
        .into_owned();
    let port = reqwest::Url::parse(&login.callback_uri)
        .unwrap()
        .port()
        .unwrap();
    let mut callback = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    callback
        .write_all(format!("GET /auth/callback?error=access_denied&state={state} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut response = [0_u8; 1024];
    let bytes = callback.read(&mut response).await.unwrap();
    assert!(response[..bytes].starts_with(b"HTTP/1.1 400"));
    assert!(matches!(
        auth.finish_browser_login(login, &cancellation).await,
        Err(tickloop_agent::auth::AuthError::AuthorizationDenied)
    ));
    assert!(matches!(
        auth.resolve_credentials(&cancellation).await,
        Err(tickloop_agent::auth::AuthError::NotLoggedIn)
    ));
}

#[tokio::test]
async fn cancelling_browser_login_releases_its_callback_listener() {
    let directory = tempfile::tempdir().unwrap();
    let state_dir = directory.path().canonicalize().unwrap();
    let auth = CodexAuth::new_for_loopback_test(&state_dir, "http://127.0.0.1:12345").unwrap();
    let cancellation = CancellationToken::new();
    let login = auth.start_browser_login(&cancellation).await.unwrap();
    let port = reqwest::Url::parse(&login.callback_uri)
        .unwrap()
        .port()
        .unwrap();
    cancellation.cancel();
    assert!(matches!(
        auth.finish_browser_login(login, &cancellation).await,
        Err(tickloop_agent::auth::AuthError::Cancelled)
    ));
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(listener) = TcpListener::bind(("127.0.0.1", port)).await {
                drop(listener);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test(start_paused = true)]
async fn browser_login_times_out_without_callback() {
    let directory = tempfile::tempdir().unwrap();
    let state_dir = directory.path().canonicalize().unwrap();
    let auth = CodexAuth::new_for_loopback_test(&state_dir, "http://127.0.0.1:12345").unwrap();
    let cancellation = CancellationToken::new();
    let login = auth.start_browser_login(&cancellation).await.unwrap();
    let task = tokio::spawn(async move { auth.finish_browser_login(login, &cancellation).await });
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(15 * 60)).await;
    assert!(matches!(
        task.await.unwrap(),
        Err(tickloop_agent::auth::AuthError::TimedOut)
    ));
}

const AUTH_CHILD_PLAN: &str = "TICKLOOP_TEST_CODEX_AUTH_CHILD_PLAN";

#[derive(Serialize, Deserialize)]
struct AuthChildPlan {
    state_dir: PathBuf,
    oauth_origin: String,
    action: String,
    ready: PathBuf,
    go: PathBuf,
    output: PathBuf,
}

#[test]
fn codex_auth_child_process() {
    let Ok(serialized) = std::env::var(AUTH_CHILD_PLAN) else {
        return;
    };
    let plan: AuthChildPlan = serde_json::from_str(&serialized).unwrap();
    std::fs::write(&plan.ready, "ready").unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while !plan.go.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "parent did not release child"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let auth = CodexAuth::new_for_loopback_test(&plan.state_dir, &plan.oauth_origin).unwrap();
    let cancellation = CancellationToken::new();
    let result = runtime.block_on(async {
        match plan.action.as_str() {
            "resolve" => auth
                .resolve_credentials(&cancellation)
                .await
                .map(|credentials| credentials.account_id),
            "logout" => auth
                .logout(&cancellation)
                .await
                .map(|()| "logged out".to_owned()),
            "relogin" => {
                let device = auth.start_device_login(&cancellation).await.unwrap();
                auth.finish_device_login(device, &cancellation)
                    .await
                    .map(|credentials| credentials.account_id)
            }
            "resolve_expect_failure" => match auth.resolve_credentials(&cancellation).await {
                Ok(_) => panic!("expected a credential storage failure"),
                Err(error) => Ok(error.to_string()),
            },
            _ => panic!("unknown auth child action"),
        }
    });
    std::fs::write(plan.output, result.unwrap()).unwrap();
}

fn spawn_auth_child(plan: &AuthChildPlan) -> tokio::process::Child {
    let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "codex_auth_child_process", "--nocapture"])
        .env(AUTH_CHILD_PLAN, serde_json::to_string(plan).unwrap())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command.spawn().unwrap()
}

async fn wait_for_file(path: &std::path::Path) {
    tokio::time::timeout(Duration::from_secs(15), async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

async fn child_succeeds(child: tokio::process::Child) {
    let output = tokio::time::timeout(Duration::from_secs(15), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn separate_processes_refresh_one_shared_credential_once() {
    let directory = tempfile::tempdir().unwrap();
    let state_dir = directory.path().canonicalize().unwrap();
    let (origin, refreshes, server) = oauth_server_with_refresh().await;
    let auth = CodexAuth::new_for_loopback_test(&state_dir, &origin).unwrap();
    let cancellation = CancellationToken::new();
    let device = auth.start_device_login(&cancellation).await.unwrap();
    auth.finish_device_login(device, &cancellation)
        .await
        .unwrap();

    let go = state_dir.join("children-go");
    let first = AuthChildPlan {
        state_dir: state_dir.clone(),
        oauth_origin: origin.clone(),
        action: "resolve".into(),
        ready: state_dir.join("child-1-ready"),
        go: go.clone(),
        output: state_dir.join("child-1-output"),
    };
    let second = AuthChildPlan {
        state_dir: state_dir.clone(),
        oauth_origin: origin.clone(),
        action: "resolve".into(),
        ready: state_dir.join("child-2-ready"),
        go: go.clone(),
        output: state_dir.join("child-2-output"),
    };
    let first_child = spawn_auth_child(&first);
    let second_child = spawn_auth_child(&second);
    wait_for_file(&first.ready).await;
    wait_for_file(&second.ready).await;
    std::fs::write(go, "go").unwrap();
    tokio::join!(child_succeeds(first_child), child_succeeds(second_child));
    assert_eq!(std::fs::read_to_string(first.output).unwrap(), "acct-test");
    assert_eq!(std::fs::read_to_string(second.output).unwrap(), "acct-test");
    assert_eq!(refreshes.load(Ordering::SeqCst), 1);
    server.abort();
}

#[tokio::test]
async fn logout_in_another_process_waits_for_refresh_then_removes_credential() {
    let directory = tempfile::tempdir().unwrap();
    let state_dir = directory.path().canonicalize().unwrap();
    let (started_tx, started_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let (origin, refreshes, server) = oauth_server_with_refresh_gate(Some(RefreshGate {
        started: started_tx,
        release: release_rx,
    }))
    .await;
    let auth = CodexAuth::new_for_loopback_test(&state_dir, &origin).unwrap();
    let cancellation = CancellationToken::new();
    let device = auth.start_device_login(&cancellation).await.unwrap();
    auth.finish_device_login(device, &cancellation)
        .await
        .unwrap();

    let refresh = AuthChildPlan {
        state_dir: state_dir.clone(),
        oauth_origin: origin.clone(),
        action: "resolve".into(),
        ready: state_dir.join("refresh-ready"),
        go: state_dir.join("refresh-go"),
        output: state_dir.join("refresh-output"),
    };
    let refresh_child = spawn_auth_child(&refresh);
    wait_for_file(&refresh.ready).await;
    std::fs::write(&refresh.go, "go").unwrap();
    tokio::time::timeout(Duration::from_secs(15), started_rx)
        .await
        .unwrap()
        .unwrap();

    let logout = AuthChildPlan {
        state_dir: state_dir.clone(),
        oauth_origin: origin.clone(),
        action: "logout".into(),
        ready: state_dir.join("logout-ready"),
        go: state_dir.join("logout-go"),
        output: state_dir.join("logout-output"),
    };
    let logout_child = spawn_auth_child(&logout);
    wait_for_file(&logout.ready).await;
    std::fs::write(&logout.go, "go").unwrap();
    release_tx.send(()).unwrap();

    tokio::join!(child_succeeds(refresh_child), child_succeeds(logout_child));
    assert_eq!(
        std::fs::read_to_string(refresh.output).unwrap(),
        "acct-test"
    );
    assert_eq!(
        std::fs::read_to_string(logout.output).unwrap(),
        "logged out"
    );
    assert!(matches!(
        auth.resolve_credentials(&cancellation).await,
        Err(tickloop_agent::auth::AuthError::NotLoggedIn)
    ));
    assert_eq!(refreshes.load(Ordering::SeqCst), 1);
    server.abort();
}

#[tokio::test]
async fn relogin_in_another_process_wins_over_in_flight_old_refresh() {
    let directory = tempfile::tempdir().unwrap();
    let state_dir = directory.path().canonicalize().unwrap();
    let (started_tx, started_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let (old_origin, refreshes, old_server) = oauth_server_with_refresh_gate(Some(RefreshGate {
        started: started_tx,
        release: release_rx,
    }))
    .await;
    let auth = CodexAuth::new_for_loopback_test(&state_dir, &old_origin).unwrap();
    let cancellation = CancellationToken::new();
    let device = auth.start_device_login(&cancellation).await.unwrap();
    auth.finish_device_login(device, &cancellation)
        .await
        .unwrap();

    let refresh = AuthChildPlan {
        state_dir: state_dir.clone(),
        oauth_origin: old_origin.clone(),
        action: "resolve".into(),
        ready: state_dir.join("old-refresh-ready"),
        go: state_dir.join("old-refresh-go"),
        output: state_dir.join("old-refresh-output"),
    };
    let refresh_child = spawn_auth_child(&refresh);
    wait_for_file(&refresh.ready).await;
    std::fs::write(&refresh.go, "go").unwrap();
    tokio::time::timeout(Duration::from_secs(15), started_rx)
        .await
        .unwrap()
        .unwrap();

    let (new_origin, new_server) = oauth_server_for_account(NEW_ACCESS_TOKEN).await;
    let relogin = AuthChildPlan {
        state_dir: state_dir.clone(),
        oauth_origin: new_origin,
        action: "relogin".into(),
        ready: state_dir.join("relogin-ready"),
        go: state_dir.join("relogin-go"),
        output: state_dir.join("relogin-output"),
    };
    let relogin_child = spawn_auth_child(&relogin);
    wait_for_file(&relogin.ready).await;
    std::fs::write(&relogin.go, "go").unwrap();
    tokio::time::timeout(Duration::from_secs(15), new_server)
        .await
        .unwrap()
        .unwrap();
    release_tx.send(()).unwrap();

    tokio::join!(child_succeeds(refresh_child), child_succeeds(relogin_child));
    assert_eq!(
        std::fs::read_to_string(refresh.output).unwrap(),
        "acct-test"
    );
    assert_eq!(std::fs::read_to_string(relogin.output).unwrap(), "acct-new");
    assert_eq!(
        auth.resolve_credentials(&cancellation)
            .await
            .unwrap()
            .account_id,
        "acct-new"
    );
    assert_eq!(refreshes.load(Ordering::SeqCst), 1);
    old_server.abort();
}

#[tokio::test]
async fn crashed_refresh_holder_releases_lock_for_another_process() {
    let directory = tempfile::tempdir().unwrap();
    let state_dir = directory.path().canonicalize().unwrap();
    let (started_tx, started_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let (origin, refreshes, server) = oauth_server_with_refresh_gate(Some(RefreshGate {
        started: started_tx,
        release: release_rx,
    }))
    .await;
    let auth = CodexAuth::new_for_loopback_test(&state_dir, &origin).unwrap();
    let cancellation = CancellationToken::new();
    let device = auth.start_device_login(&cancellation).await.unwrap();
    auth.finish_device_login(device, &cancellation)
        .await
        .unwrap();

    let holder = AuthChildPlan {
        state_dir: state_dir.clone(),
        oauth_origin: origin.clone(),
        action: "resolve".into(),
        ready: state_dir.join("holder-ready"),
        go: state_dir.join("holder-go"),
        output: state_dir.join("holder-output"),
    };
    let mut child = spawn_auth_child(&holder);
    wait_for_file(&holder.ready).await;
    std::fs::write(&holder.go, "go").unwrap();
    tokio::time::timeout(Duration::from_secs(15), started_rx)
        .await
        .unwrap()
        .unwrap();
    child.kill().await.unwrap();
    child.wait().await.unwrap();
    release_tx.send(()).unwrap();

    let recovered = tokio::time::timeout(
        Duration::from_secs(10),
        auth.resolve_credentials(&cancellation),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(recovered.account_id, "acct-test");
    assert_eq!(refreshes.load(Ordering::SeqCst), 2);
    server.abort();
}

#[tokio::test]
async fn failed_refresh_save_in_child_preserves_last_complete_credential() {
    let directory = tempfile::tempdir().unwrap();
    let state_dir = directory.path().canonicalize().unwrap();
    let (origin, refreshes, server) = oauth_server_with_refresh().await;
    let auth = CodexAuth::new_for_loopback_test(&state_dir, &origin).unwrap();
    let cancellation = CancellationToken::new();
    let device = auth.start_device_login(&cancellation).await.unwrap();
    auth.finish_device_login(device, &cancellation)
        .await
        .unwrap();
    let credential_path = state_dir.join("auth/openai-codex-credentials.json");
    let saved_before = std::fs::read(&credential_path).unwrap();
    let auth_dir = state_dir.join("auth");
    std::fs::set_permissions(&auth_dir, std::fs::Permissions::from_mode(0o500)).unwrap();

    let child_plan = AuthChildPlan {
        state_dir: state_dir.clone(),
        oauth_origin: origin.clone(),
        action: "resolve_expect_failure".into(),
        ready: state_dir.join("failed-save-ready"),
        go: state_dir.join("failed-save-go"),
        output: state_dir.join("failed-save-output"),
    };
    let child = spawn_auth_child(&child_plan);
    wait_for_file(&child_plan.ready).await;
    std::fs::write(&child_plan.go, "go").unwrap();
    child_succeeds(child).await;
    std::fs::set_permissions(&auth_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        std::fs::read_to_string(child_plan.output)
            .unwrap()
            .contains("storage failed")
    );
    assert_eq!(std::fs::read(&credential_path).unwrap(), saved_before);
    assert_eq!(refreshes.load(Ordering::SeqCst), 1);
    assert_eq!(
        auth.resolve_credentials(&cancellation)
            .await
            .unwrap()
            .account_id,
        "acct-test"
    );
    assert_eq!(refreshes.load(Ordering::SeqCst), 2);
    server.abort();
}

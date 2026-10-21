use std::{
    os::unix::fs::{PermissionsExt, symlink},
    sync::Arc,
};

use serde_json::json;
use tick_oauth_test_server::{
    AuthorizationServer, DEVICE_AUTHORIZATION_PATH, METADATA_PATH, REGISTRATION_PATH,
    REVOCATION_PATH, TOKEN_PATH, TokenReply, USER_CODE,
};

use super::*;

struct Fixture {
    directory: tempfile::TempDir,
    server: AuthorizationServer,
    client: ArtifactClient,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let server = AuthorizationServer::start_with_path("/account");
        server.edit_metadata(|metadata| {
            metadata
                .as_object_mut()
                .unwrap()
                .remove("registration_endpoint");
        });
        let client = ArtifactClient::for_origin(
            directory.path().canonicalize().unwrap(),
            server.issuer().strip_suffix("/account").unwrap(),
        )
        .unwrap();
        Self {
            directory,
            server,
            client,
        }
    }

    async fn login(&self) -> ArtifactUser {
        self.server
            .answer_polls([TokenReply::Approve, TokenReply::Approve]);
        let device = self.client.start_device_login().await.unwrap();
        self.client
            .finish_device_login(device, &CancellationToken::new())
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn accounts_login_discovers_path_issuer_refreshes_artifacts_and_revokes_the_family() {
    let fixture = Fixture::new();
    fixture.server.answer_polls([
        TokenReply::Pending,
        TokenReply::Approve,
        TokenReply::Approve,
    ]);
    let device = fixture.client.start_device_login().await.unwrap();
    assert_eq!(device.user_code, USER_CODE);
    assert_eq!(
        device.verification_uri,
        format!("{}/verify?user_code={USER_CODE}", fixture.server.issuer())
    );
    let user = fixture
        .client
        .finish_device_login(device, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(user.id, "account-1");
    let account = fixture.client.read_account().unwrap().unwrap();
    assert_eq!(account.oauth.access_token, "access-1");
    assert_eq!(account.oauth.refresh_token(), Some("refresh-2"));
    assert_eq!(
        fixture
            .client
            .credential_snapshot()
            .unwrap()
            .unwrap()
            .access_token,
        "access-2"
    );
    assert_eq!(
        fixture.client.status().await.unwrap().unwrap().email,
        "a@example.test"
    );
    let mode =
        |path: &std::path::Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&fixture.client.credential_path()), 0o600);
    assert_eq!(
        mode(fixture.client.credential_path().parent().unwrap()),
        0o700
    );
    assert_eq!(
        mode(&fixture.client.credential_path().with_extension("lock")),
        0o600
    );
    let metadata = fixture.server.requests_to(METADATA_PATH);
    assert_eq!(
        metadata[0].path,
        "/.well-known/oauth-authorization-server/account"
    );
    assert!(fixture.server.requests_to(REGISTRATION_PATH).is_empty());
    let requests = fixture.server.requests_to(DEVICE_AUTHORIZATION_PATH);
    assert_eq!(requests[0].form["client_id"], CLIENT_ID);
    assert_eq!(
        requests[0].form["scope"],
        "openid profile email offline_access artifacts.agent artifacts.console"
    );
    assert!(!requests[0].form.contains_key("resource"));
    let tokens = fixture.server.requests_to(TOKEN_PATH);
    assert!(!tokens[0].form.contains_key("resource"));
    assert_eq!(
        tokens[2].form["resource"],
        format!("{}/artifact", fixture.client.origin())
    );
    assert_eq!(tokens[2].form["refresh_token"], "refresh-1");
    assert!(tokens.iter().all(|request| request.authorization.is_none()));
    for request in fixture.server.requests_to("/userinfo") {
        assert_eq!(request.authorization.as_deref(), Some("Bearer access-1"));
    }
    assert!(matches!(
        fixture.client.logout().await.unwrap(),
        LogoutOutcome::Removed
    ));
    let revocations = fixture.server.requests_to(REVOCATION_PATH);
    assert_eq!(revocations[0].form["token"], "refresh-2");
    assert_eq!(revocations[0].form["client_id"], CLIENT_ID);
    assert_eq!(revocations[0].form["token_type_hint"], "refresh_token");
    assert!(!fixture.client.credential_path().exists());
}

#[tokio::test]
async fn accounts_login_rejects_plural_issuer_metadata_without_a_fallback() {
    let fixture = Fixture::new();
    fixture.server.edit_metadata(|metadata| {
        metadata["issuer"] = json!(format!("{}/accounts", fixture.client.origin()));
    });
    assert!(matches!(
        fixture.client.start_device_login().await,
        Err(ArtifactError::Login(LoginError::Failed(reason)))
            if reason.contains("describes another issuer than")
    ));
    let requests = fixture.server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].path,
        "/.well-known/oauth-authorization-server/account"
    );
    assert!(!fixture.client.credential_path().exists());
}

#[tokio::test]
async fn resource_caches_share_a_rotating_refresh_token_and_serialize_concurrent_refreshes() {
    let fixture = Fixture::new();
    fixture.login().await;
    let second_resource = format!("{}/another-resource", fixture.client.origin());
    fixture.server.answer_polls([TokenReply::Approve]);
    let first = fixture.client.credential_snapshot().unwrap().unwrap();
    {
        let _lock = fixture.client.lock().await.unwrap();
        let mut account = fixture.client.read_account().unwrap().unwrap();
        fixture
            .client
            .renew_resource(&mut account, &second_resource)
            .await
            .unwrap();
        assert_eq!(account.resources[&second_resource].access_token, "access-3");
        fixture.client.save_account(&account).unwrap();
    }
    assert_eq!(
        fixture
            .client
            .credential_snapshot()
            .unwrap()
            .unwrap()
            .access_token,
        first.access_token
    );
    let mut account = fixture.client.read_account().unwrap().unwrap();
    account
        .resources
        .get_mut(&fixture.client.resource())
        .unwrap()
        .expires_at = now_seconds();
    fixture.client.save_account(&account).unwrap();
    fixture.server.answer_polls([TokenReply::Approve]);
    let client = Arc::new(fixture.client.clone());
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let client = client.clone();
        tasks.push(tokio::spawn(async move {
            client
                .access_snapshot()
                .await
                .unwrap()
                .unwrap()
                .access_token
        }));
    }
    for task in tasks {
        assert_eq!(task.await.unwrap(), "access-4");
    }
    let requests = fixture.server.requests_to(TOKEN_PATH);
    assert_eq!(requests.len(), 4);
    assert_eq!(requests[2].form["refresh_token"], "refresh-2");
    assert_eq!(requests[3].form["refresh_token"], "refresh-3");
    let account = fixture.client.read_account().unwrap().unwrap();
    assert_eq!(account.oauth.refresh_token(), Some("refresh-4"));
    assert_eq!(account.resources.len(), 2);
    assert_eq!(account.oauth.access_token, "access-1");
    let other = ArtifactClient::for_origin(
        fixture.directory.path().canonicalize().unwrap(),
        "https://other.example",
    )
    .unwrap();
    assert_eq!(other.accounts_issuer(), "https://other.example/account");
    assert!(other.credential_snapshot().unwrap().is_none());
}

#[tokio::test]
async fn a_renewed_artifacts_token_keeps_the_account_identity_and_cache_key() {
    let fixture = Fixture::new();
    fixture.login().await;
    let signed_in = fixture.client.credential_snapshot().unwrap().unwrap();
    let mut account = fixture.client.read_account().unwrap().unwrap();
    account
        .resources
        .get_mut(&fixture.client.resource())
        .unwrap()
        .expires_at = now_seconds();
    fixture.client.save_account(&account).unwrap();
    fixture.server.answer_polls([TokenReply::Approve]);
    let renewed = fixture.client.access_snapshot().await.unwrap().unwrap();
    assert_ne!(renewed.access_token, signed_in.access_token);
    assert!(fixture.client.credentials_match(&signed_in, &renewed));
    assert_eq!(
        fixture.client.credential_key(&signed_in),
        fixture.client.credential_key(&renewed)
    );
    let other_account = CredentialSnapshot {
        account_id: "other-account".into(),
        ..renewed.clone()
    };
    assert!(!fixture.client.credentials_match(&signed_in, &other_account));
    assert_ne!(
        fixture.client.credential_key(&signed_in),
        fixture.client.credential_key(&other_account)
    );
    let other_origin = CredentialSnapshot {
        origin: "https://other.example".into(),
        ..renewed
    };
    assert!(!fixture.client.credentials_match(&signed_in, &other_origin));
}

#[tokio::test]
async fn status_renews_userinfo_with_the_latest_refresh_token_before_renewing_artifacts() {
    let fixture = Fixture::new();
    fixture.login().await;
    let mut account = fixture.client.read_account().unwrap().unwrap();
    account.oauth.expires_at = now_seconds();
    account
        .resources
        .get_mut(&fixture.client.resource())
        .unwrap()
        .expires_at = now_seconds();
    fixture.client.save_account(&account).unwrap();
    fixture
        .server
        .answer_polls([TokenReply::Approve, TokenReply::Approve]);
    assert!(fixture.client.status().await.unwrap().is_some());
    let requests = fixture.server.requests_to(TOKEN_PATH);
    assert_eq!(requests[2].form["refresh_token"], "refresh-2");
    assert!(!requests[2].form.contains_key("resource"));
    assert_eq!(requests[3].form["refresh_token"], "refresh-3");
    assert_eq!(requests[3].form["resource"], fixture.client.resource());
    assert_eq!(
        fixture
            .server
            .requests_to("/userinfo")
            .last()
            .unwrap()
            .authorization
            .as_deref(),
        Some("Bearer access-3")
    );
    assert_eq!(
        fixture
            .client
            .credential_snapshot()
            .unwrap()
            .unwrap()
            .access_token,
        "access-4"
    );
}

#[tokio::test]
async fn lost_exchange_asks_to_log_in_again_without_overwriting_an_existing_login() {
    let fixture = Fixture::new();
    fixture.login().await;
    fixture.server.answer_polls([TokenReply::LostExchange]);
    let device = fixture.client.start_device_login().await.unwrap();
    let error = fixture
        .client
        .finish_device_login(device, &CancellationToken::new())
        .await
        .err()
        .unwrap();
    assert!(matches!(error, ArtifactError::LoginInterrupted));
    assert_eq!(error.to_string(), "login interrupted, try again");
    assert_eq!(
        fixture
            .client
            .credential_snapshot()
            .unwrap()
            .unwrap()
            .access_token,
        "access-2"
    );
    assert_eq!(fixture.server.requests_to(TOKEN_PATH).len(), 4);
}

#[tokio::test]
async fn device_error_codes_are_terminal_and_do_not_register_or_persist() {
    for (code, status) in [
        ("access_denied", 400),
        ("expired_token", 400),
        ("invalid_grant", 400),
        ("invalid_target", 400),
        ("invalid_client", 401),
    ] {
        let fixture = Fixture::new();
        fixture
            .server
            .answer_polls([TokenReply::Error(status, code.into())]);
        let device = fixture.client.start_device_login().await.unwrap();
        let error = fixture
            .client
            .finish_device_login(device, &CancellationToken::new())
            .await
            .err()
            .unwrap();
        match code {
            "access_denied" => assert!(matches!(error, ArtifactError::AccessDenied)),
            "expired_token" => assert!(matches!(error, ArtifactError::Expired)),
            "invalid_grant" => assert!(matches!(error, ArtifactError::LoginInterrupted)),
            _ => assert!(
                matches!(error, ArtifactError::Login(LoginError::Refused { code: actual, .. }) if actual == code)
            ),
        }
        assert!(!fixture.client.credential_path().exists());
        assert!(fixture.server.requests_to(REGISTRATION_PATH).is_empty());
        assert_eq!(fixture.server.requests_to(TOKEN_PATH).len(), 1);
    }
}

#[tokio::test]
async fn slow_down_is_observed_and_cancellation_stops_without_polling() {
    let fixture = Fixture::new();
    let device = fixture.client.start_device_login().await.unwrap();
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    assert!(matches!(
        fixture
            .client
            .finish_device_login(device, &cancellation)
            .await,
        Err(ArtifactError::Cancelled)
    ));
    assert!(fixture.server.requests_to(TOKEN_PATH).is_empty());
    fixture.server.answer_polls([
        TokenReply::SlowDown,
        TokenReply::Approve,
        TokenReply::Approve,
    ]);
    let device = fixture.client.start_device_login().await.unwrap();
    fixture
        .client
        .finish_device_login(device, &CancellationToken::new())
        .await
        .unwrap();
    let polls = fixture.server.requests_to(TOKEN_PATH);
    assert!(polls[1].at.duration_since(polls[0].at) >= Duration::from_secs(6));
}

#[tokio::test]
async fn background_login_keeps_an_account_saved_while_approval_was_pending() {
    let fixture = Fixture::new();
    let pending = fixture.client.start_device_login().await.unwrap();
    fixture.login().await;
    fixture
        .server
        .answer_polls([TokenReply::Approve, TokenReply::Approve]);
    assert!(
        fixture
            .client
            .finish_background_device_login(pending, &CancellationToken::new())
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        fixture
            .client
            .credential_snapshot()
            .unwrap()
            .unwrap()
            .access_token,
        "access-2"
    );
    assert_eq!(
        fixture.server.requests_to(REVOCATION_PATH)[0].form["token"],
        "refresh-4"
    );
}

#[tokio::test]
async fn failed_logout_retains_the_latest_refresh_token_for_explicit_retry() {
    let fixture = Fixture::new();
    fixture.login().await;
    fixture.server.answer_revocations(503);
    assert!(matches!(
        fixture.client.logout().await,
        Err(ArtifactError::RevocationUnknown)
    ));
    assert_eq!(
        fixture
            .client
            .read_account()
            .unwrap()
            .unwrap()
            .oauth
            .refresh_token(),
        Some("refresh-2")
    );
    fixture.server.answer_revocations(200);
    assert!(matches!(
        fixture.client.logout().await.unwrap(),
        LogoutOutcome::Removed
    ));
}

#[tokio::test]
async fn refused_refresh_clears_the_account_but_server_failure_preserves_it() {
    let fixture = Fixture::new();
    fixture.login().await;
    let mut account = fixture.client.read_account().unwrap().unwrap();
    account
        .resources
        .get_mut(&fixture.client.resource())
        .unwrap()
        .expires_at = now_seconds();
    fixture.client.save_account(&account).unwrap();
    fixture.server.answer_polls([
        TokenReply::Status(503),
        TokenReply::Error(400, "invalid_grant".into()),
    ]);
    assert!(matches!(
        fixture.client.access_snapshot().await,
        Err(ArtifactError::Connection)
    ));
    assert!(fixture.client.read_account().unwrap().is_some());
    assert!(fixture.client.access_snapshot().await.unwrap().is_none());
    assert!(fixture.client.read_account().unwrap().is_none());
}

#[tokio::test]
async fn userinfo_refusal_removes_credentials_and_wrong_identity_does_not_replace_them() {
    let fixture = Fixture::new();
    fixture.login().await;
    fixture
        .server
        .answer_userinfo(200, json!({"sub":"someone-else"}));
    assert!(matches!(
        fixture.client.status().await,
        Err(ArtifactError::InvalidResponse)
    ));
    assert!(fixture.client.read_account().unwrap().is_some());
    fixture
        .server
        .answer_userinfo(401, json!({"error":"invalid_token"}));
    assert!(fixture.client.status().await.unwrap().is_none());
}

#[tokio::test]
async fn private_storage_rejects_symlinks_and_permissive_credentials_or_lock_files() {
    let fixture = Fixture::new();
    fixture.login().await;
    let path = fixture.client.credential_path();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(matches!(
        fixture.client.read_account(),
        Err(ArtifactError::Storage)
    ));
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let external = fixture.directory.path().join("external");
    std::fs::rename(&path, &external).unwrap();
    symlink(&external, &path).unwrap();
    assert!(matches!(
        fixture.client.read_account(),
        Err(ArtifactError::Storage)
    ));
    std::fs::remove_file(&path).unwrap();
    std::fs::rename(&external, &path).unwrap();
    let lock = path.with_extension("lock");
    std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(matches!(
        fixture.client.logout().await,
        Err(ArtifactError::Storage)
    ));
    assert!(fixture.server.requests_to(REVOCATION_PATH).is_empty());
}

#[tokio::test]
async fn an_old_token_cannot_invalidate_a_replaced_login() {
    let fixture = Fixture::new();
    fixture.login().await;
    let old = fixture.client.credential_snapshot().unwrap().unwrap();
    fixture.login().await;
    fixture
        .client
        .invalidate_credential(&old.access_token)
        .await
        .unwrap();
    assert_eq!(
        fixture
            .client
            .credential_snapshot()
            .unwrap()
            .unwrap()
            .access_token,
        "access-4"
    );
}

#[tokio::test]
async fn a_refused_resource_does_not_remove_the_account_or_another_cached_audience() {
    let fixture = Fixture::new();
    fixture.login().await;
    let second_resource = format!("{}/another-resource", fixture.client.origin());
    let mut account = fixture.client.read_account().unwrap().unwrap();
    account.resources.remove(&fixture.client.resource());
    account.resources.insert(
        second_resource.clone(),
        ResourceToken {
            access_token: "other-access".into(),
            expires_at: now_seconds() + 600,
        },
    );
    fixture.client.save_account(&account).unwrap();
    fixture
        .server
        .answer_polls([TokenReply::Error(400, "invalid_target".into())]);
    assert!(matches!(fixture.client.access_snapshot().await,
        Err(ArtifactError::Login(LoginError::Refused { code, .. })) if code == "invalid_target"));
    let saved = fixture.client.read_account().unwrap().unwrap();
    assert_eq!(saved.oauth.refresh_token(), Some("refresh-2"));
    assert_eq!(
        saved.resources[&second_resource].access_token,
        "other-access"
    );
}

#[tokio::test]
async fn obsolete_artifacts_credentials_are_deleted_without_being_used() {
    let fixture = Fixture::new();
    let directory = fixture.client.credential_directory().unwrap();
    let legacy = directory.join(format!(
        "{:x}.json",
        Sha256::digest(fixture.client.origin().as_bytes())
    ));
    std::fs::write(&legacy, b"obsolete credential").unwrap();
    assert!(fixture.client.status().await.unwrap().is_none());
    assert!(!legacy.exists());
    assert!(fixture.server.requests().is_empty());
}

#[tokio::test]
async fn logout_and_a_new_login_serialize_without_removing_the_new_login() {
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::oneshot,
    };
    let fixture = Fixture::new();
    fixture.login().await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut account = fixture.client.read_account().unwrap().unwrap();
    account.oauth.revocation_endpoint =
        Some(format!("http://{}/revoke", listener.local_addr().unwrap()));
    fixture.client.save_account(&account).unwrap();
    let (started, received) = oneshot::channel();
    let (release, allowed) = oneshot::channel();
    let serving = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut bytes = [0; 4096];
        assert!(stream.read(&mut bytes).await.unwrap() > 0);
        started.send(()).unwrap();
        allowed.await.unwrap();
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
    });
    let client = fixture.client.clone();
    let logout = tokio::spawn(async move { client.logout().await });
    received.await.unwrap();
    fixture
        .server
        .answer_polls([TokenReply::Approve, TokenReply::Approve]);
    let device = fixture.client.start_device_login().await.unwrap();
    let client = fixture.client.clone();
    let login = tokio::spawn(async move {
        client
            .finish_device_login(device, &CancellationToken::new())
            .await
    });
    release.send(()).unwrap();
    assert!(matches!(
        logout.await.unwrap().unwrap(),
        LogoutOutcome::Removed
    ));
    login.await.unwrap().unwrap();
    serving.await.unwrap();
    assert_eq!(
        fixture
            .client
            .credential_snapshot()
            .unwrap()
            .unwrap()
            .access_token,
        "access-4"
    );
}

#[tokio::test]
async fn userinfo_redirects_do_not_forward_the_account_token() {
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };
    let fixture = Fixture::new();
    fixture.login().await;
    let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let redirect = TcpListener::bind("127.0.0.1:0").await.unwrap();
    fixture.server.edit_metadata(|metadata| {
        metadata["userinfo_endpoint"] = json!(format!(
            "http://{}/userinfo",
            redirect.local_addr().unwrap()
        ))
    });
    let response = format!(
        "HTTP/1.1 302 Found\r\nLocation: http://{}/stolen\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        target.local_addr().unwrap()
    );
    let serving = tokio::spawn(async move {
        let (mut stream, _) = redirect.accept().await.unwrap();
        let mut bytes = [0; 4096];
        assert!(stream.read(&mut bytes).await.unwrap() > 0);
        stream.write_all(response.as_bytes()).await.unwrap();
    });
    assert!(matches!(
        fixture.client.status().await,
        Err(ArtifactError::Connection)
    ));
    assert!(fixture.client.credential_snapshot().unwrap().is_some());
    assert!(
        tokio::time::timeout(Duration::from_millis(100), target.accept())
            .await
            .is_err()
    );
    serving.await.unwrap();
}

#[test]
fn accounts_and_artifacts_follow_the_same_validated_tickloop_origin() {
    let directory = tempfile::tempdir().unwrap();
    for (configured, expected) in [
        (DEFAULT_ORIGIN, "https://tickloop.ai"),
        ("https://pre.tickloop.ai", "https://pre.tickloop.ai"),
        (
            "https://service.example:8443/",
            "https://service.example:8443",
        ),
        ("http://127.0.0.1:12345/", "http://127.0.0.1:12345"),
    ] {
        let client = ArtifactClient::for_origin(directory.path(), configured).unwrap();
        assert_eq!(client.accounts_issuer(), format!("{expected}/account"));
        assert_eq!(client.resource(), format!("{expected}/artifact"));
        assert_eq!(
            mcp_endpoint(client.origin()),
            format!("{expected}/artifact/mcp")
        );
    }
}

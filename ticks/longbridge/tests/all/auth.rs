//! The Longbridge login against a local authorization server: the device login of
//! `tick-longbridge auth` and the renewal every `tick-longbridge` makes of the saved token.

use std::{os::unix::fs::PermissionsExt, path::Path, time::Duration};

use serde_json::json;
use tick_longbridge::{
    AUTH_HINT,
    auth::{self, DeviceLogin, LoginNotice},
    region::Region,
};
use tick_packages::{LoginState, OAuthIssuer, PackageAuth};
use tick_provider::exit_code;
use tick_test_support::authorization_server::{
    AuthorizationServer, DEVICE_CODE_GRANT, REGISTRATION_PATH, TOKEN_PATH, TokenReply, USER_CODE,
};
use tokio::sync::mpsc;

use crate::support::login::{credentials, now, save, saved, token};

/// Bounds a login whose failure would otherwise wait for its device code to expire.
const HANG_GUARD: Duration = Duration::from_secs(30);
/// Just outside the SDK's five-minute refresh window.
const ACTIVE_FOR: u64 = 300 + 3;

/// Logs in with `server` until the login ends, and returns what it ended with and the notices
/// the waiting screen would have shown.
async fn log_in(
    config_root: &Path,
    server: &AuthorizationServer,
) -> (tick_provider::error::Result<()>, Vec<&'static str>) {
    let (notices, mut shown) = mpsc::unbounded_channel();
    let result = tokio::time::timeout(HANG_GUARD, async {
        let login = DeviceLogin::start(&credentials(config_root), server.issuer()).await?;
        assert_eq!(login.user_code(), USER_CODE);
        login.finish(&notices).await
    })
    .await
    .expect("the login ends");
    drop(notices);
    let mut messages = Vec::new();
    while let Some(notice) = shown.recv().await {
        messages.push(notice.message());
    }
    (result, messages)
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// The login Tickloop's manifest declares for the package, with `server` as its one issuer.
fn package_auth(server: &AuthorizationServer) -> PackageAuth {
    PackageAuth::OAuth2 {
        issuers: vec![OAuthIssuer {
            label: "Local".into(),
            issuer: server.issuer().into(),
        }],
        scopes: Vec::new(),
    }
}

/// A device login registers as a public device client once, polls as RFC 8628 says and saves
/// the token in the standard file Tickloop keeps, which Tickloop then reads as logged in. The
/// next login reuses the client, also one Tickloop registered.
#[tokio::test]
async fn a_device_login_registers_once_and_saves_the_token_tickloop_reads() {
    let root = tempfile::tempdir().unwrap();
    let server = AuthorizationServer::start();
    server.answer_polls([TokenReply::Pending, TokenReply::Approve]);

    let (result, notices) = log_in(root.path(), &server).await;

    result.unwrap();
    assert!(notices.is_empty(), "{notices:?}");
    let registrations = server.requests_to(REGISTRATION_PATH);
    assert_eq!(registrations.len(), 1);
    assert_eq!(
        registrations[0].json,
        Some(json!({
            "client_name": "Tickloop",
            "grant_types": [DEVICE_CODE_GRANT, "refresh_token"],
            "token_endpoint_auth_method": "none",
            "redirect_uris": ["http://localhost:60355/callback"],
        }))
    );
    let polls = server.requests_to(TOKEN_PATH);
    assert_eq!(polls.len(), 2);
    assert_eq!(polls[0].form["grant_type"], DEVICE_CODE_GRANT);
    assert_eq!(polls[0].form["device_code"], "device-secret-client-1");
    assert_eq!(polls[0].form["client_id"], "client-1");
    let login = saved(root.path()).unwrap();
    assert_eq!(login.access_token, "access-1");
    assert_eq!(login.refresh_token.as_deref(), Some("refresh-1"));
    assert_eq!(login.token_type, "Bearer");
    assert_eq!(login.issuer, server.issuer());
    assert_eq!(login.client_id, "client-1");
    assert_eq!(login.token_endpoint, format!("{}/token", server.issuer()));
    assert_eq!(
        login.revocation_endpoint,
        Some(format!("{}/revoke", server.issuer()))
    );
    assert!((now() + 3590..=now() + 3600).contains(&login.expires_at));
    let files = credentials(root.path());
    assert_eq!(mode(files.directory()), 0o700);
    assert_eq!(mode(&files.oauth_token_path()), 0o600);
    assert_eq!(
        files.login_state(&package_auth(&server)),
        LoginState::LoggedIn
    );

    server.answer_polls([TokenReply::Approve]);
    log_in(root.path(), &server).await.0.unwrap();
    assert_eq!(server.requests_to(REGISTRATION_PATH).len(), 1);
    assert_eq!(saved(root.path()).unwrap().access_token, "access-2");

    let elsewhere = tempfile::tempdir().unwrap();
    credentials(elsewhere.path())
        .lock_oauth()
        .unwrap()
        .set_client_id(server.issuer(), Some("tickloop-client"))
        .unwrap();
    server.answer_polls([TokenReply::Approve]);
    log_in(elsewhere.path(), &server).await.0.unwrap();
    assert_eq!(server.requests_to(REGISTRATION_PATH).len(), 1);
    assert_eq!(
        saved(elsewhere.path()).unwrap().client_id,
        "tickloop-client"
    );
}

/// A server that forgot the registered client gets a new registration, once.
#[tokio::test]
async fn a_forgotten_client_is_registered_again() {
    let root = tempfile::tempdir().unwrap();
    let server = AuthorizationServer::start();
    server.answer_polls([TokenReply::Approve]);
    log_in(root.path(), &server).await.0.unwrap();
    server.forget_clients();
    server.answer_polls([TokenReply::Approve]);

    log_in(root.path(), &server).await.0.unwrap();

    assert_eq!(server.requests_to(REGISTRATION_PATH).len(), 2);
    assert_eq!(saved(root.path()).unwrap().client_id, "client-2");
}

/// A refused or expired authorization saves nothing and asks the user to log in again.
#[tokio::test]
async fn a_refused_or_expired_authorization_saves_nothing() {
    for (reply, reason) in [
        (TokenReply::Denied, "authorization denied"),
        (TokenReply::Expired, "device code expired"),
        (
            TokenReply::Error(400, "invalid_grant".into()),
            "Longbridge refused the device authorization (invalid_grant)",
        ),
    ] {
        let root = tempfile::tempdir().unwrap();
        let server = AuthorizationServer::start();
        server.answer_polls([TokenReply::Pending, reply]);

        let error = log_in(root.path(), &server).await.0.unwrap_err();

        assert_eq!(error.code, exit_code::NOT_AUTHORIZED, "{error}");
        assert!(error.message.starts_with(reason), "{error}");
        assert!(error.message.ends_with(AUTH_HINT), "{error}");
        assert_eq!(saved(root.path()), None);
    }
}

/// A device code that runs out while the user has not approved it ends the login.
#[tokio::test]
async fn a_device_code_that_runs_out_ends_the_login() {
    let root = tempfile::tempdir().unwrap();
    let server = AuthorizationServer::start();
    server.edit_device_authorization(|device| device["expires_in"] = json!(2));

    let error = log_in(root.path(), &server).await.0.unwrap_err();

    assert_eq!(error.code, exit_code::NOT_AUTHORIZED, "{error}");
    assert!(error.message.starts_with("device code expired"), "{error}");
}

/// An answer that is no token is reported, and nothing is saved.
#[tokio::test]
async fn an_approval_without_a_token_is_reported() {
    let root = tempfile::tempdir().unwrap();
    let server = AuthorizationServer::start();
    server.answer_polls([TokenReply::Token(json!({"token_type": "Bearer"}))]);

    let error = log_in(root.path(), &server).await.0.unwrap_err();

    assert_eq!(error.code, exit_code::NETWORK, "{error}");
    assert_eq!(saved(root.path()), None);
}

/// While the server cannot answer, the login keeps polling, backing off, and says so once.
#[tokio::test]
async fn polling_rides_out_server_failures_and_says_so_once() {
    let root = tempfile::tempdir().unwrap();
    let server = AuthorizationServer::start();
    server.answer_polls([
        TokenReply::Status(503),
        TokenReply::Status(500),
        TokenReply::Approve,
    ]);

    let (result, notices) = log_in(root.path(), &server).await;

    result.unwrap();
    assert_eq!(notices, [LoginNotice::TemporaryFailureRetry.message()]);
    let polls = server.requests_to(TOKEN_PATH);
    assert_eq!(polls.len(), 3);
    assert!(polls[2].at - polls[1].at > polls[1].at - polls[0].at);
    assert_eq!(saved(root.path()).unwrap().access_token, "access-1");
}

/// A server asking for slower polling gets it, five seconds slower, and the user is told.
#[tokio::test]
async fn polling_slows_down_when_asked() {
    let root = tempfile::tempdir().unwrap();
    let server = AuthorizationServer::start();
    server.answer_polls([TokenReply::SlowDown, TokenReply::Approve]);

    let (result, notices) = log_in(root.path(), &server).await;

    result.unwrap();
    assert_eq!(notices, [LoginNotice::SlowerPolling.message()]);
    let polls = server.requests_to(TOKEN_PATH);
    assert_eq!(polls.len(), 2);
    let slowed = polls[1].at - polls[0].at;
    assert!(slowed >= Duration::from_secs(6), "{slowed:?}");
}

/// Only an https server is logged in with, or one on this machine.
#[tokio::test]
async fn a_server_without_https_or_device_authorization_is_refused() {
    let root = tempfile::tempdir().unwrap();
    let credentials = credentials(root.path());
    let error = DeviceLogin::start(&credentials, "http://example.com")
        .await
        .err()
        .unwrap();
    assert!(error.message.contains("only https"), "{error}");

    let server = AuthorizationServer::start();
    server.edit_metadata(|metadata| {
        metadata["grant_types_supported"] = json!(["refresh_token"]);
    });
    let error = DeviceLogin::start(&credentials, server.issuer())
        .await
        .err()
        .unwrap();
    assert!(error.message.contains("no device authorization"), "{error}");
    assert!(server.requests_to(REGISTRATION_PATH).is_empty());
}

/// A due token is renewed at its token endpoint and written back with the refresh token the
/// server rotated it to.
#[tokio::test]
async fn a_due_token_is_renewed_and_its_rotated_refresh_token_saved() {
    let root = tempfile::tempdir().unwrap();
    let server = AuthorizationServer::start();
    save(
        root.path(),
        &token(server.issuer(), "ap_lapsed", Some("us_refresh"), 1),
    );
    server.answer_polls([TokenReply::Approve]);

    let renewed = auth::current_token(&credentials(root.path()))
        .await
        .unwrap();

    assert_eq!(renewed.access_token, "access-1");
    assert_eq!(saved(root.path()).unwrap(), renewed);
    assert_eq!(renewed.refresh_token.as_deref(), Some("refresh-1"));
    assert_eq!(renewed.client_id, "client-1");
    assert!(renewed.expires_at > now() + 3000);
    let refreshes = server.requests_to(TOKEN_PATH);
    assert_eq!(refreshes.len(), 1);
    assert_eq!(refreshes[0].form["grant_type"], "refresh_token");
    assert_eq!(refreshes[0].form["refresh_token"], "us_refresh");
    assert_eq!(refreshes[0].form["client_id"], "client-1");

    // A server that does not rotate the refresh token keeps the one there was.
    save(
        root.path(),
        &token(server.issuer(), "ap_lapsed", Some("ap_refresh"), 1),
    );
    server.answer_polls([TokenReply::Token(
        json!({"access_token": "ap_renewed", "token_type": "Bearer", "expires_in": 7200}),
    )]);
    let renewed = auth::current_token(&credentials(root.path()))
        .await
        .unwrap();
    assert_eq!(renewed.access_token, "ap_renewed");
    assert_eq!(renewed.refresh_token.as_deref(), Some("ap_refresh"));
}

/// A token that is not due is used as it is, without the network.
#[tokio::test]
async fn a_token_that_is_not_due_is_not_renewed() {
    let root = tempfile::tempdir().unwrap();
    let server = AuthorizationServer::start();
    let current = token(
        server.issuer(),
        "ap_current",
        Some("ap_refresh"),
        now() + 3600,
    );
    save(root.path(), &current);

    assert_eq!(
        auth::current_token(&credentials(root.path()))
            .await
            .unwrap(),
        current
    );
    assert!(server.requests_to(TOKEN_PATH).is_empty());
}

/// A renewal Longbridge refuses asks the user to log in again and leaves the saved token.
#[tokio::test]
async fn a_refused_renewal_asks_to_log_in_again() {
    let root = tempfile::tempdir().unwrap();
    let server = AuthorizationServer::start();
    let lapsed = token(server.issuer(), "ap_lapsed", Some("ap_refresh"), 1);
    save(root.path(), &lapsed);
    server.answer_polls([TokenReply::Error(400, "invalid_grant".into())]);

    let error = auth::current_token(&credentials(root.path()))
        .await
        .unwrap_err();

    assert_eq!(error.code, exit_code::NOT_AUTHORIZED, "{error}");
    assert!(error.message.ends_with(AUTH_HINT), "{error}");
    assert!(!error.message.contains("ap_refresh"), "{error}");
    assert_eq!(saved(root.path()).unwrap(), lapsed);

    let expired = token(server.issuer(), "ap_lapsed", None, 1);
    save(root.path(), &expired);
    let error = auth::current_token(&credentials(root.path()))
        .await
        .unwrap_err();
    assert_eq!(error.code, exit_code::NOT_AUTHORIZED, "{error}");
    assert_eq!(
        server.requests_to(TOKEN_PATH).len(),
        1,
        "nothing to renew with"
    );
}

/// A renewal the server cannot answer, even with an OAuth error in a 5xx body, is a network
/// failure that leaves the login in place, and the next renewal passes.
#[tokio::test]
async fn a_renewal_the_server_cannot_answer_is_retried_later() {
    let root = tempfile::tempdir().unwrap();
    let server = AuthorizationServer::start();
    let lapsed = token(server.issuer(), "ap_lapsed", Some("ap_refresh"), 1);
    save(root.path(), &lapsed);
    server.answer_polls([
        TokenReply::Error(503, "temporarily_unavailable".into()),
        TokenReply::Error(502, "invalid_grant".into()),
        TokenReply::Approve,
    ]);

    for _ in 0..2 {
        let error = auth::current_token(&credentials(root.path()))
            .await
            .unwrap_err();
        assert_eq!(error.code, exit_code::NETWORK, "{error}");
        assert!(!error.message.contains("ap_refresh"), "{error}");
        assert_eq!(saved(root.path()).unwrap(), lapsed);
    }
    let renewed = auth::current_token(&credentials(root.path()))
        .await
        .unwrap();
    assert_eq!(renewed.access_token, "access-1");
    assert_eq!(server.requests_to(TOKEN_PATH).len(), 3);
}

/// A process waiting on the lock while another renews the token takes the renewed token
/// instead of asking again, which a rotating server would refuse.
#[tokio::test]
async fn a_renewal_waits_for_the_one_under_way_and_takes_its_token() {
    let root = tempfile::tempdir().unwrap();
    let server = AuthorizationServer::start();
    save(
        root.path(),
        &token(server.issuer(), "ap_lapsed", Some("ap_refresh"), 1),
    );
    let other_process = credentials(root.path()).lock_oauth().unwrap();
    let waiting = tokio::spawn({
        let credentials = credentials(root.path());
        async move { auth::current_token(&credentials).await }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!waiting.is_finished(), "the renewal waits for the lock");
    let renewed = token(
        server.issuer(),
        "ap_renewed",
        Some("ap_rotated"),
        now() + 3600,
    );
    other_process.save_token(&renewed).unwrap();
    drop(other_process);

    assert_eq!(waiting.await.unwrap().unwrap(), renewed);
    assert!(server.requests_to(TOKEN_PATH).is_empty());
}

/// An SDK client renews its token through the saved file once the token enters the SDK's
/// refresh window while it runs, and never starts a browser login.
#[tokio::test]
async fn a_running_sdk_client_renews_through_the_saved_token() {
    let root = tempfile::tempdir().unwrap();
    let server = AuthorizationServer::start();
    let expires_at = now() + ACTIVE_FOR;
    save(
        root.path(),
        &token(
            server.issuer(),
            "ap_running",
            Some("ap_refresh"),
            expires_at,
        ),
    );
    let authorized = auth::authorized_oauth(&credentials(root.path()))
        .await
        .unwrap();
    assert_eq!(authorized.oauth.access_token().await.unwrap(), "ap_running");
    assert!(server.requests_to(TOKEN_PATH).is_empty());
    while expires_at.saturating_sub(now()) >= 300 {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    server.answer_polls([TokenReply::Approve]);

    assert_eq!(authorized.oauth.access_token().await.unwrap(), "access-1");
    assert_eq!(saved(root.path()).unwrap().access_token, "access-1");
    assert_eq!(server.requests_to(TOKEN_PATH).len(), 1);
}

/// The API is reached through the network of the server the login was made with, and a US
/// account's through the global one.
#[tokio::test]
async fn the_api_network_follows_the_login() {
    let root = tempfile::tempdir().unwrap();
    for (issuer, access_token, region) in [
        ("https://openapi.longbridge.cn", "ap_token", Region::Cn),
        ("https://openapi.longbridge.com", "ap_token", Region::Global),
        ("https://openapi.longbridge.cn", "us_token", Region::Global),
    ] {
        save(
            root.path(),
            &token(issuer, access_token, None, now() + 3600),
        );
        let authorized = auth::authorized_oauth(&credentials(root.path()))
            .await
            .unwrap();
        assert_eq!(authorized.region, region, "{issuer} {access_token}");
    }
}

/// A token Tickloop's own login saved is what `tick-longbridge` uses.
#[tokio::test]
async fn a_token_tickloop_saved_is_used() {
    let root = tempfile::tempdir().unwrap();
    let server = AuthorizationServer::start();
    server.answer_polls([TokenReply::Approve]);
    let issuer = OAuthIssuer {
        label: "Local".into(),
        issuer: server.issuer().into(),
    };
    let login = tick_oauth::DeviceLogin::start(
        reqwest::Client::new(),
        &credentials(root.path()),
        &issuer.issuer,
        &[],
    )
    .await
    .unwrap();
    login
        .finish(&tokio_util::sync::CancellationToken::new(), |_| {})
        .await
        .unwrap();

    let current = auth::current_token(&credentials(root.path()))
        .await
        .unwrap();
    assert_eq!(current.access_token, "access-1");
    auth::check_saved(&credentials(root.path())).unwrap();
    assert_eq!(saved(root.path()).unwrap(), current);
}

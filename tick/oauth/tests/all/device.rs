//! Device logins: discovery, registration, the device code and its polling, and what is saved.

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    time::{Duration, Instant},
};

use serde_json::{Value, json};
use tick_oauth::{DeviceLogin, LoginError, Progress};
use tick_packages::LoginState;
use tick_test_support::authorization_server::{
    DEVICE_AUTHORIZATION_PATH, DEVICE_CODE_GRANT, METADATA_PATH, REGISTRATION_PATH,
    REVOCATION_PATH, TOKEN_PATH, TokenReply, USER_CODE,
};
use tokio_util::sync::CancellationToken;

use crate::support::{Login, http, now};

/// Runs a login to its end and returns what it ended with and the progress it reported.
async fn run_reporting(login: &Login) -> (Result<(), LoginError>, Vec<Progress>) {
    let started = login.start().await.unwrap();
    let mut reported = Vec::new();
    let result = started
        .finish(&CancellationToken::new(), |progress| {
            reported.push(progress)
        })
        .await;
    (result, reported)
}

#[tokio::test]
async fn a_device_login_registers_once_and_saves_the_token_the_user_approves() {
    let login = Login::new();
    let issuer = login.server.issuer().to_owned();
    login
        .server
        .answer_polls([TokenReply::Pending, TokenReply::Approve]);

    let started = login.start().await.unwrap();
    assert_eq!(started.user_code(), USER_CODE);
    assert_eq!(started.issuer(), issuer);
    assert_eq!(started.verification_uri(), format!("{issuer}/verify"));
    assert_eq!(
        started.link(),
        format!("{issuer}/verify?user_code={USER_CODE}")
    );
    assert_eq!(started.expires_in(), Duration::from_secs(600));
    assert!(started.remaining() <= Duration::from_secs(600));
    assert!(!format!("{started:?}").contains("device-secret"));
    started
        .finish(&CancellationToken::new(), |_| {})
        .await
        .unwrap();

    let saved = login.saved();
    let expires_at = saved["expires_at"].as_u64().unwrap();
    assert!(
        (now() + 3590..=now() + 3600).contains(&expires_at),
        "{saved}"
    );
    assert_eq!(
        saved,
        json!({
            "access_token": "access-1",
            "token_type": "Bearer",
            "refresh_token": "refresh-1",
            "issuer": issuer,
            "client_id": "client-1",
            "token_endpoint": format!("{issuer}{TOKEN_PATH}"),
            "revocation_endpoint": format!("{issuer}{REVOCATION_PATH}"),
            "expires_at": expires_at,
        })
    );
    let mode = |path: &std::path::Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&login.credentials.oauth_token_path()), 0o600);
    assert_eq!(mode(login.credentials.directory()), 0o700);
    assert_eq!(
        login.credentials.login_state(&login.auth()),
        LoginState::LoggedIn
    );

    let requests = login.server.requests();
    let paths = requests
        .iter()
        .map(|request| format!("{} {}", request.method, request.path))
        .collect::<Vec<_>>();
    assert_eq!(
        paths,
        [
            format!("GET {METADATA_PATH}"),
            format!("POST {REGISTRATION_PATH}"),
            format!("POST {DEVICE_AUTHORIZATION_PATH}"),
            format!("POST {TOKEN_PATH}"),
            format!("POST {TOKEN_PATH}"),
        ]
    );
    assert_eq!(
        requests[1].json,
        Some(json!({
            "client_name": "Tickloop",
            "grant_types": [DEVICE_CODE_GRANT, "refresh_token"],
            "token_endpoint_auth_method": "none",
            "redirect_uris": ["http://localhost:60355/callback"],
        }))
    );
    assert_eq!(
        requests[2].form.iter().collect::<Vec<_>>(),
        [(&"client_id".to_owned(), &"client-1".to_owned())]
    );
    for poll in &requests[3..] {
        assert_eq!(poll.form["grant_type"], DEVICE_CODE_GRANT);
        assert_eq!(poll.form["device_code"], "device-secret-client-1");
        assert_eq!(poll.form["client_id"], "client-1");
    }
    let spacing = requests[4].at - requests[3].at;
    assert!(spacing >= Duration::from_millis(950), "{spacing:?}");

    // The next login reuses the registered client and replaces the token.
    login.server.answer_polls([TokenReply::Approve]);
    login.run().await.unwrap();
    assert_eq!(login.server.requests_to(REGISTRATION_PATH).len(), 1);
    assert_eq!(login.saved()["access_token"], "access-2");
    assert_eq!(login.saved()["client_id"], "client-1");
}

#[tokio::test]
async fn polling_slows_down_by_five_seconds_when_asked() {
    let login = Login::new();
    login
        .server
        .answer_polls([TokenReply::SlowDown, TokenReply::Approve]);
    let (result, reported) = run_reporting(&login).await;
    result.unwrap();
    assert_eq!(reported, [Progress::SlowerPolling]);
    let polls = login.server.requests_to(TOKEN_PATH);
    assert_eq!(polls.len(), 2);
    let slowed = polls[1].at - polls[0].at;
    assert!(
        slowed >= Duration::from_millis(5950) && slowed < Duration::from_secs(8),
        "{slowed:?}"
    );
}

/// Only a 400 or 401 with an OAuth error answers a poll (RFC 6749 section 5.2): any other status
/// is the server failing, even with an OAuth error in its body, and polling backs off and goes on.
#[tokio::test]
async fn polling_rides_out_server_failures_even_with_an_oauth_error_in_their_body() {
    let login = Login::new();
    login.server.answer_polls([
        TokenReply::Error(503, "temporarily_unavailable".into()),
        TokenReply::Error(502, "access_denied".into()),
        TokenReply::Approve,
    ]);
    let (result, reported) = run_reporting(&login).await;
    result.unwrap();
    assert_eq!(reported, [Progress::Unreachable], "said once");
    let polls = login.server.requests_to(TOKEN_PATH);
    assert_eq!(polls.len(), 3);
    let (first, second) = (polls[1].at - polls[0].at, polls[2].at - polls[1].at);
    assert!(
        first >= Duration::from_millis(1950) && second >= Duration::from_millis(3950),
        "{first:?} {second:?}"
    );
    assert_eq!(login.saved()["access_token"], "access-1");

    // A server that cannot answer until the code expires is named in the failure.
    let login = Login::new();
    login
        .server
        .edit_device_authorization(|device| device["expires_in"] = json!(4));
    login
        .server
        .answer_polls([TokenReply::Status(502), TokenReply::Status(502)]);
    let error = login.run().await.unwrap_err();
    assert!(matches!(error, LoginError::Failed(_)), "{error:?}");
    assert_eq!(
        error.to_string(),
        format!(
            "the code expired while the authorization server could not be reached: \
             {}{TOKEN_PATH} answered HTTP 502 Bad Gateway",
            login.server.issuer()
        )
    );
}

#[tokio::test]
async fn a_refused_expired_or_cancelled_login_saves_nothing() {
    for (reply, expected) in [
        (TokenReply::Denied, LoginError::Denied),
        (TokenReply::Expired, LoginError::Expired),
    ] {
        let login = Login::new();
        login.server.answer_polls([TokenReply::Pending, reply]);
        assert_eq!(login.run().await.unwrap_err(), expected);
        assert!(!login.credentials.oauth_token_path().exists());
    }

    let login = Login::new();
    login
        .server
        .answer_polls([TokenReply::Error(401, "invalid_grant".into())]);
    let error = login.run().await.unwrap_err();
    assert_eq!(
        error,
        LoginError::Refused {
            code: "invalid_grant".into(),
            message: format!(
                "{}{TOKEN_PATH} refused the device code: invalid_grant",
                login.server.issuer()
            ),
        }
    );
    assert!(!error.to_string().contains("device-secret"), "{error}");

    // A code that expires before the next poll ends the login without waiting for the server.
    let login = Login::new();
    login
        .server
        .edit_device_authorization(|device| device["expires_in"] = json!(2));
    let started = Instant::now();
    assert_eq!(login.run().await.unwrap_err(), LoginError::Expired);
    assert!(started.elapsed() < Duration::from_secs(3));
    assert_eq!(login.server.requests_to(TOKEN_PATH).len(), 1);

    let login = Login::new();
    let started = login.start().await.unwrap();
    let cancellation = CancellationToken::new();
    let cancel = cancellation.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        cancel.cancel();
    });
    let waited = Instant::now();
    assert_eq!(
        started.finish(&cancellation, |_| {}).await.unwrap_err(),
        LoginError::Cancelled
    );
    assert!(waited.elapsed() < Duration::from_secs(1));
    assert!(!login.credentials.oauth_token_path().exists());

    // An approval without a token saves nothing.
    let login = Login::new();
    login
        .server
        .answer_polls([TokenReply::Token(json!({"token_type": "Bearer"}))]);
    let error = login.run().await.unwrap_err();
    assert!(
        matches!(&error, LoginError::Failed(reason) if reason.ends_with("sent an invalid token response")),
        "{error:?}"
    );
    assert!(!login.credentials.oauth_token_path().exists());
}

#[tokio::test]
async fn a_client_the_server_forgot_is_registered_again_once() {
    let login = Login::new();
    login.server.answer_polls([TokenReply::Approve]);
    login.run().await.unwrap();
    login.server.forget_clients();

    login.server.answer_polls([TokenReply::Approve]);
    login.run().await.unwrap();
    assert_eq!(login.server.requests_to(REGISTRATION_PATH).len(), 2);
    assert_eq!(login.saved()["client_id"], "client-2");

    // A client the server refuses right after registering it fails the login.
    login.server.forget_clients();
    login
        .server
        .answer_registrations(201, json!({"client_id": "client-1"}));
    let error = login.start().await.unwrap_err();
    assert!(
        matches!(&error, LoginError::Refused { code, .. } if code == "invalid_client"),
        "{error:?}"
    );
}

#[tokio::test]
async fn scopes_are_requested_with_the_device_code() {
    let login = Login::new();
    login
        .start_with(&["4".to_owned(), "quote:read".to_owned()])
        .await
        .unwrap();
    let device = &login.server.requests_to(DEVICE_AUTHORIZATION_PATH)[0];
    assert_eq!(device.form["scope"], "4 quote:read");
}

#[tokio::test]
async fn the_server_must_describe_itself_and_offer_the_device_grant() {
    type MetadataEdit = fn(&mut Value);
    let cases: [(&str, MetadataEdit); 6] = [
        ("describes another issuer than", |metadata| {
            metadata["issuer"] = json!("https://other.example.com")
        }),
        ("offers no device authorization (RFC 8628)", |metadata| {
            metadata
                .as_object_mut()
                .unwrap()
                .remove("device_authorization_endpoint");
        }),
        ("offers no device authorization (RFC 8628)", |metadata| {
            metadata["grant_types_supported"] = json!(["authorization_code"])
        }),
        ("offers no device authorization (RFC 8628)", |metadata| {
            metadata
                .as_object_mut()
                .unwrap()
                .remove("grant_types_supported");
        }),
        (
            "token_endpoint http://example.com/token is refused: only https is accepted, or http on this machine",
            |metadata| metadata["token_endpoint"] = json!("http://example.com/token"),
        ),
        (
            "offers no client registration (registration_endpoint)",
            |metadata| {
                metadata
                    .as_object_mut()
                    .unwrap()
                    .remove("registration_endpoint");
            },
        ),
    ];
    for (reason, edit) in cases {
        let login = Login::new();
        login.server.edit_metadata(edit);
        let error = login.start().await.unwrap_err().to_string();
        assert!(error.contains(reason), "{reason}: {error}");
        assert!(
            login
                .server
                .requests_to(DEVICE_AUTHORIZATION_PATH)
                .is_empty()
        );
    }

    let login = Login::new();
    login.server.edit_device_authorization(|device| {
        device["verification_uri"] = json!("http://example.com/verify")
    });
    let error = login.start().await.unwrap_err().to_string();
    assert!(
        error.contains("verification_uri http://example.com/verify is refused"),
        "{error}"
    );

    let login = Login::new();
    login.server.answer_registrations(
        201,
        json!({"client_id": "confidential", "token_endpoint_auth_method": "client_secret_basic"}),
    );
    let error = login.start().await.unwrap_err().to_string();
    assert!(
        error.ends_with("sent an invalid client registration response"),
        "{error}"
    );
    login
        .server
        .answer_registrations(400, json!({"error": "invalid_client_metadata"}));
    let error = login.start().await.unwrap_err();
    assert!(
        matches!(&error, LoginError::Refused { code, .. } if code == "invalid_client_metadata"),
        "{error:?}"
    );
    assert!(
        error
            .to_string()
            .ends_with("refused to register Tickloop: invalid_client_metadata"),
        "{error}"
    );
    login
        .server
        .answer_registrations(503, json!({"error": "invalid_client_metadata"}));
    let error = login.start().await.unwrap_err();
    assert!(
        matches!(&error, LoginError::Failed(reason) if reason.ends_with("answered HTTP 503 Service Unavailable")),
        "a 503 is no OAuth refusal: {error:?}"
    );

    let error = DeviceLogin::start(http(), &login.credentials, "http://auth.example.com", &[])
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(
        error,
        "issuer http://auth.example.com/ is refused: only https is accepted, or http on this machine"
    );
}

//! Logging a package in and out against a local authorization server, with a private config
//! root.

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde_json::{Value, json};
use tick_packages::{ApiKey, LoginState, OAuthIssuer, OAuthToken, PackageAuth, PackageCredentials};
use tickloop_agent_packages::{LoggedOut, LoginError, OAuthLogin, log_out};
use tickloop_agent_test_support::authorization_server::{
    AuthorizationServer, DEVICE_AUTHORIZATION_PATH, DEVICE_CODE_GRANT, METADATA_PATH,
    REGISTRATION_PATH, REVOCATION_PATH, TOKEN_PATH, TokenReply, USER_CODE,
};
use tokio_util::sync::CancellationToken;

struct Login {
    _directory: tempfile::TempDir,
    credentials: PackageCredentials,
    server: AuthorizationServer,
}

impl Login {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let credentials = PackageCredentials::in_config_root(directory.path(), "sample").unwrap();
        Self {
            _directory: directory,
            credentials,
            server: AuthorizationServer::start(),
        }
    }

    fn issuer(&self) -> OAuthIssuer {
        OAuthIssuer {
            label: "Local".into(),
            issuer: self.server.issuer().to_owned(),
        }
    }

    fn auth(&self) -> PackageAuth {
        PackageAuth::OAuth2 {
            issuers: vec![self.issuer()],
            scopes: Vec::new(),
        }
    }

    async fn start(&self) -> Result<OAuthLogin, LoginError> {
        OAuthLogin::start(&self.credentials, &self.issuer(), &[]).await
    }

    /// Starts a login and waits for it to end.
    async fn run(&self) -> Result<(), LoginError> {
        self.start().await?.finish(&CancellationToken::new()).await
    }

    fn saved(&self) -> Value {
        serde_json::from_slice(&fs::read(self.credentials.oauth_token_path()).unwrap()).unwrap()
    }

    fn state(&self) -> LoginState {
        self.credentials.login_state(&self.auth())
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
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
    assert_eq!(started.verification_uri(), format!("{issuer}/verify"));
    assert_eq!(
        started.link(),
        format!("{issuer}/verify?user_code={USER_CODE}")
    );
    assert_eq!(started.expires_in(), Duration::from_secs(600));
    assert!(!format!("{started:?}").contains("device-secret"));
    started.finish(&CancellationToken::new()).await.unwrap();

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
    assert_eq!(login.state(), LoginState::LoggedIn);

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
    assert!(login.server.requests_to(REGISTRATION_PATH).len() == 1);
    assert_eq!(login.saved()["access_token"], "access-2");
    assert_eq!(login.saved()["client_id"], "client-1");
}

#[tokio::test]
async fn polling_slows_down_by_five_seconds_when_asked() {
    let login = Login::new();
    login
        .server
        .answer_polls([TokenReply::SlowDown, TokenReply::Approve]);
    login.run().await.unwrap();
    let polls = login.server.requests_to(TOKEN_PATH);
    assert_eq!(polls.len(), 2);
    let slowed = polls[1].at - polls[0].at;
    assert!(
        slowed >= Duration::from_millis(5950) && slowed < Duration::from_secs(8),
        "{slowed:?}"
    );
    assert_eq!(login.state(), LoginState::LoggedIn);
}

#[tokio::test]
async fn polling_backs_off_while_the_server_fails() {
    let login = Login::new();
    login
        .server
        .answer_polls([TokenReply::Status(503), TokenReply::Approve]);
    login.run().await.unwrap();
    let polls = login.server.requests_to(TOKEN_PATH);
    assert_eq!(polls.len(), 2);
    let backed_off = polls[1].at - polls[0].at;
    assert!(
        backed_off >= Duration::from_millis(1950) && backed_off < Duration::from_secs(4),
        "{backed_off:?}"
    );
    assert_eq!(login.state(), LoginState::LoggedIn);

    // A server that cannot be reached until the code expires is named in the failure.
    let login = Login::new();
    login
        .server
        .edit_device_authorization(|device| device["expires_in"] = json!(4));
    login
        .server
        .answer_polls([TokenReply::Status(502), TokenReply::Status(502)]);
    let error = login.run().await.unwrap_err().to_string();
    assert_eq!(
        error,
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
        assert_eq!(login.state(), LoginState::LoggedOut);
    }

    let login = Login::new();
    login
        .server
        .answer_polls([TokenReply::Error(400, "invalid_grant".into())]);
    let error = login.run().await.unwrap_err().to_string();
    assert_eq!(
        error,
        format!(
            "{}{TOKEN_PATH} refused the device code: invalid_grant",
            login.server.issuer()
        )
    );
    assert!(!error.contains("device-secret"), "{error}");

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
        started.finish(&cancellation).await.unwrap_err(),
        LoginError::Cancelled
    );
    assert!(waited.elapsed() < Duration::from_secs(1));
    assert_eq!(login.state(), LoginState::LoggedOut);
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
    let error = login.start().await.unwrap_err().to_string();
    assert!(error.contains("(invalid_client)"), "{error}");
}

#[tokio::test]
async fn scopes_are_requested_with_the_device_code() {
    let login = Login::new();
    let scopes = ["4".to_owned(), "quote:read".to_owned()];
    OAuthLogin::start(&login.credentials, &login.issuer(), &scopes)
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
    let error = login.start().await.unwrap_err().to_string();
    assert!(
        error.ends_with("refused to register Tickloop: invalid_client_metadata"),
        "{error}"
    );

    let credentials = &login.credentials;
    let remote = OAuthIssuer {
        label: "Remote".into(),
        issuer: "http://auth.example.com".into(),
    };
    let error = OAuthLogin::start(credentials, &remote, &[])
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(
        error,
        "issuer http://auth.example.com/ is refused: only https is accepted, or http on this machine"
    );
}

#[tokio::test]
async fn logging_out_revokes_the_refresh_token_and_deletes_the_saved_login() {
    let login = Login::new();
    let auth = login.auth();
    assert_eq!(
        log_out(&login.credentials, &auth).await.unwrap(),
        LoggedOut {
            had_credentials: false,
            revocation_failure: None,
        }
    );
    assert!(login.server.requests().is_empty());

    login.server.answer_polls([TokenReply::Approve]);
    login.run().await.unwrap();
    assert_eq!(
        log_out(&login.credentials, &auth).await.unwrap(),
        LoggedOut {
            had_credentials: true,
            revocation_failure: None,
        }
    );
    let revocation = &login.server.requests_to(REVOCATION_PATH)[0];
    assert_eq!(revocation.form["token"], "refresh-1");
    assert_eq!(revocation.form["token_type_hint"], "refresh_token");
    assert_eq!(revocation.form["client_id"], "client-1");
    assert_eq!(login.state(), LoginState::LoggedOut);

    // A failed revocation is reported and the login is deleted all the same.
    login.server.answer_polls([TokenReply::Approve]);
    login.run().await.unwrap();
    login.server.answer_revocations(503);
    let logged_out = log_out(&login.credentials, &auth).await.unwrap();
    assert!(logged_out.had_credentials);
    let failure = logged_out.revocation_failure.unwrap();
    assert_eq!(
        failure,
        format!(
            "{}{REVOCATION_PATH} answered HTTP 503 Service Unavailable",
            login.server.issuer()
        )
    );
    assert!(!failure.contains("refresh-2"));
    assert_eq!(login.state(), LoginState::LoggedOut);

    // Without a refresh token the access token is revoked; without an endpoint nothing is.
    let issuer = login.server.issuer().to_owned();
    let mut token = OAuthToken {
        access_token: "access-only".into(),
        token_type: "Bearer".into(),
        refresh_token: None,
        scope: None,
        issuer: issuer.clone(),
        client_id: "client-1".into(),
        token_endpoint: format!("{issuer}{TOKEN_PATH}"),
        revocation_endpoint: Some(format!("{issuer}{REVOCATION_PATH}")),
        expires_at: now() + 60,
    };
    login.server.answer_revocations(200);
    login
        .credentials
        .lock_oauth()
        .unwrap()
        .save_token(&token)
        .unwrap();
    log_out(&login.credentials, &auth).await.unwrap();
    let revocation = login.server.requests_to(REVOCATION_PATH).pop().unwrap();
    assert_eq!(revocation.form["token"], "access-only");
    assert_eq!(revocation.form["token_type_hint"], "access_token");
    token.revocation_endpoint = None;
    login
        .credentials
        .lock_oauth()
        .unwrap()
        .save_token(&token)
        .unwrap();
    let revocations = login.server.requests_to(REVOCATION_PATH).len();
    assert!(
        log_out(&login.credentials, &auth)
            .await
            .unwrap()
            .had_credentials
    );
    assert_eq!(login.server.requests_to(REVOCATION_PATH).len(), revocations);

    // A token that cannot be read is deleted without asking the server.
    fs::write(login.credentials.oauth_token_path(), "{").unwrap();
    assert!(
        log_out(&login.credentials, &auth)
            .await
            .unwrap()
            .had_credentials
    );
    assert_eq!(login.server.requests_to(REVOCATION_PATH).len(), revocations);
}

#[tokio::test]
async fn an_api_key_logs_out_by_deleting_it() {
    let directory = tempfile::tempdir().unwrap();
    let credentials = PackageCredentials::in_config_root(directory.path(), "sample").unwrap();
    let auth = PackageAuth::ApiKey {
        env: "SAMPLE_KEY".into(),
        label: "Sample key".into(),
    };
    credentials
        .save_api_key(&ApiKey::new("secret").unwrap())
        .unwrap();
    assert_eq!(credentials.login_state(&auth), LoginState::LoggedIn);
    assert!(log_out(&credentials, &auth).await.unwrap().had_credentials);
    assert_eq!(credentials.login_state(&auth), LoginState::LoggedOut);
    assert!(!log_out(&credentials, &auth).await.unwrap().had_credentials);
    assert_eq!(
        log_out(&credentials, &PackageAuth::None).await.unwrap_err(),
        "the package needs no login"
    );
}

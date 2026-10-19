//! Logging a package in and out under the Agent's HTTP policy, against a local authorization
//! server and a private config root; the OAuth client itself is covered by `tick/oauth`.

use std::fs;

use tick_packages::{ApiKey, LoginState, OAuthIssuer, OAuthToken, PackageAuth, PackageCredentials};
use tick_test_support::authorization_server::{
    AuthorizationServer, METADATA_PATH, REVOCATION_PATH, TOKEN_PATH, TokenReply, USER_CODE,
};
use tickloop_agent_packages::{LoggedOut, LoginError, log_out, start_login};
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
            server: AuthorizationServer::start_with_path("/account"),
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

    /// Logs in through the Agent's client until the login ends.
    async fn run(&self) -> Result<(), LoginError> {
        start_login(&self.credentials, &self.issuer(), &[])
            .await?
            .finish(&CancellationToken::new(), |_| {})
            .await
    }

    fn state(&self) -> LoginState {
        self.credentials.login_state(&self.auth())
    }
}

#[tokio::test]
async fn the_agent_logs_in_through_its_own_http_client() {
    let login = Login::new();
    login
        .server
        .answer_polls([TokenReply::Pending, TokenReply::Approve]);
    let started = start_login(&login.credentials, &login.issuer(), &[])
        .await
        .unwrap();
    assert_eq!(started.user_code(), USER_CODE);
    started
        .finish(&CancellationToken::new(), |_| {})
        .await
        .unwrap();
    assert_eq!(login.state(), LoginState::LoggedIn);
    assert_eq!(
        login.server.requests_to(METADATA_PATH)[0].path,
        "/.well-known/oauth-authorization-server/account"
    );
    assert_eq!(login.server.requests_to(TOKEN_PATH).len(), 2);

    let remote = OAuthIssuer {
        label: "Remote".into(),
        issuer: "http://auth.example.com".into(),
    };
    assert!(
        start_login(&login.credentials, &remote, &[])
            .await
            .unwrap_err()
            .to_string()
            .contains("only https is accepted")
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

    // A token that cannot be read is deleted without asking the server.
    let issuer = login.server.issuer().to_owned();
    login
        .credentials
        .lock_oauth()
        .unwrap()
        .save_token(&OAuthToken {
            access_token: "access-only".into(),
            token_type: "Bearer".into(),
            refresh_token: None,
            scope: None,
            issuer: issuer.clone(),
            client_id: "client-1".into(),
            token_endpoint: format!("{issuer}{TOKEN_PATH}"),
            revocation_endpoint: Some(format!("{issuer}{REVOCATION_PATH}")),
            expires_at: 1,
        })
        .unwrap();
    fs::write(login.credentials.oauth_token_path(), "{").unwrap();
    let revocations = login.server.requests_to(REVOCATION_PATH).len();
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

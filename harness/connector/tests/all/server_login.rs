use serde_json::{Value, json};
use tick_oauth_test_server::{REGISTRATION_PATH, TOKEN_PATH};
use tickloop_harness_connector::{Connector, LoginError, OAuthSettings, PendingServerLogin};

use crate::support::{Fixture, query_field};

const REDIRECT_URI: &str = "https://chat.example.test/connectors/callback";
const OTHER_REDIRECT_URI: &str = "https://chat.example.test/other/callback";

impl Fixture {
    async fn start_server_login(&self, redirect_uri: &str) -> PendingServerLogin {
        let challenge = self.challenge().await;
        self.connector
            .start_server_login(redirect_uri, Some(&challenge), &self.cancellation)
            .await
            .unwrap()
    }

    /// The query of the browser's request to the host's redirect address after the user
    /// approves.
    async fn approved_query(&self, login: &PendingServerLogin) -> String {
        let redirect =
            tokio::task::block_in_place(|| self.server.approve(login.authorization_url()));
        assert!(
            redirect.starts_with(&format!("{}?", login.redirect_uri())),
            "{redirect}"
        );
        reqwest::Url::parse(&redirect)
            .unwrap()
            .query()
            .unwrap()
            .to_owned()
    }
}

/// What the host stores between the two requests, edited as `edit` says.
fn stored(login: &PendingServerLogin, edit: impl FnOnce(&mut Value)) -> PendingServerLogin {
    let mut saved = serde_json::to_value(login).unwrap();
    edit(&mut saved);
    serde_json::from_value(saved).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_login_registers_the_hosts_redirect_and_finishes_in_another_connector() {
    let fixture = Fixture::new();
    let login = fixture.start_server_login(REDIRECT_URI).await;

    let authorization_url = login.authorization_url().to_owned();
    assert_eq!(
        query_field(&authorization_url, "code_challenge_method").as_deref(),
        Some("S256")
    );
    assert_eq!(
        query_field(&authorization_url, "redirect_uri").as_deref(),
        Some(REDIRECT_URI)
    );
    assert_eq!(
        query_field(&authorization_url, "state").as_deref(),
        Some(login.state())
    );
    let registration = &fixture.server.requests_to(REGISTRATION_PATH)[0];
    let registration = registration.json.as_ref().unwrap();
    assert_eq!(registration["client_name"], json!("Tickloop"));
    assert_eq!(registration["redirect_uris"], json!([REDIRECT_URI]));
    assert_eq!(registration["application_type"], json!("web"));

    let saved = serde_json::to_string(&login).unwrap();
    drop(login);
    let restored: PendingServerLogin = serde_json::from_str(&saved).unwrap();
    let another = fixture.another_connector();
    let query = fixture.approved_query(&restored).await;
    another
        .finish_server_login(restored, &query, &fixture.cancellation)
        .await
        .unwrap();

    let exchange = &fixture.server.requests_to(TOKEN_PATH)[0];
    assert_eq!(exchange.form["grant_type"], "authorization_code");
    assert_eq!(exchange.form["redirect_uri"], REDIRECT_URI);
    assert_eq!(
        Some(&exchange.form["resource"]),
        query_field(&authorization_url, "resource").as_ref()
    );
    let credential = fixture.credentials.current().unwrap();
    assert_eq!(credential.registration.redirect_uri, REDIRECT_URI);
    assert_eq!(credential.server_url, fixture.server.resource());
    let token = fixture
        .connector
        .access_token(&fixture.cancellation)
        .await
        .unwrap();
    assert!(fixture.resource_accepts(&token).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pending_server_login_shows_no_secret_in_debug_output() {
    const SECRET_REDIRECT: &str =
        "https://chat.example.test/connectors/callback/tenant-7f3a?key=s3cr%2Fet%20value";
    let fixture = Fixture::new();
    let login = fixture.start_server_login(SECRET_REDIRECT).await;
    let saved = serde_json::to_value(&login).unwrap();
    let verifier = saved["pkce_verifier"].as_str().unwrap();
    assert!(!verifier.is_empty());

    let shown = format!("{login:?}");
    assert!(!shown.contains(login.state()), "{shown}");
    assert!(!shown.contains(verifier), "{shown}");
    assert!(!shown.contains("code_challenge"), "{shown}");
    for hidden in ["/connectors", "tenant-7f3a", "key=", "s3cr", "%2F", "%20"] {
        assert!(!shown.contains(hidden), "{hidden}: {shown}");
    }
    for address in [
        saved["server_url"].as_str().unwrap(),
        saved["registration"]["authorization_server"]
            .as_str()
            .unwrap(),
    ] {
        let url = reqwest::Url::parse(address).unwrap();
        if url.path() != "/" {
            assert!(!shown.contains(url.path()), "{shown}");
        }
    }
    assert!(shown.contains("https://chat.example.test"), "{shown}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_callback_of_another_login_is_refused_and_leaves_the_login_usable() {
    let fixture = Fixture::new();
    let login = fixture.start_server_login(REDIRECT_URI).await;
    let query = fixture.approved_query(&login).await;
    let foreign = query.replace(login.state(), "another-login");

    for refused in [foreign.as_str(), "code=code-1", ""] {
        assert_eq!(
            fixture
                .connector
                .finish_server_login(login.clone(), refused, &fixture.cancellation)
                .await,
            Err(LoginError::StateMismatch),
            "{refused}"
        );
    }
    let elsewhere = Connector::new(
        &format!("{}/elsewhere", fixture.server.issuer()),
        OAuthSettings::default(),
        fixture.credentials.clone(),
    )
    .unwrap();
    assert_eq!(
        elsewhere
            .finish_server_login(login.clone(), &query, &fixture.cancellation)
            .await,
        Err(LoginError::StateMismatch)
    );
    assert!(fixture.server.requests_to(TOKEN_PATH).is_empty());
    assert_eq!(fixture.credentials.current(), None);

    fixture
        .connector
        .finish_server_login(login, &format!("?{query}"), &fixture.cancellation)
        .await
        .unwrap();
    assert!(fixture.credentials.current().unwrap().token.is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_expired_server_login_saves_nothing() {
    let fixture = Fixture::new();
    let login = fixture.start_server_login(REDIRECT_URI).await;
    let query = fixture.approved_query(&login).await;
    let expired = stored(&login, |saved| saved["expires_at"] = json!(0));

    let error = fixture
        .connector
        .finish_server_login(expired, &query, &fixture.cancellation)
        .await
        .unwrap_err();
    assert_eq!(error, LoginError::Expired);
    assert_eq!(error.to_string(), "the login expired; start it again");
    assert!(fixture.server.requests_to(TOKEN_PATH).is_empty());
    assert_eq!(fixture.credentials.current(), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_authorization_server_error_saves_nothing() {
    let fixture = Fixture::new();
    let login = fixture.start_server_login(REDIRECT_URI).await;
    let state = login.state().to_owned();
    let refusals = [
        (
            format!("error=server_error&state={state}"),
            LoginError::AuthorizationFailed,
        ),
        (format!("state={state}"), LoginError::InvalidCallback),
        (
            format!("code=a&code=b&state={state}"),
            LoginError::InvalidCallback,
        ),
        (
            format!("code=code-1&state={state}&iss=https%3A%2F%2Fother.example.test"),
            LoginError::IssuerMismatch,
        ),
    ];
    for (query, expected) in refusals {
        assert_eq!(
            fixture
                .connector
                .finish_server_login(login.clone(), &query, &fixture.cancellation)
                .await,
            Err(expected),
            "{query}"
        );
    }
    fixture.server.deny_authorizations();
    let denied = fixture.approved_query(&login).await;
    assert_eq!(
        fixture
            .connector
            .finish_server_login(login.clone(), &denied, &fixture.cancellation)
            .await,
        Err(LoginError::Denied)
    );
    assert_eq!(
        fixture
            .connector
            .finish_server_login(
                login,
                &format!("code=unknown&state={state}"),
                &fixture.cancellation
            )
            .await,
        Err(LoginError::TokenExchange)
    );
    assert_eq!(fixture.credentials.current(), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_registration_is_reused_only_for_the_same_redirect_address() {
    let fixture = Fixture::new();
    let first = fixture.start_server_login(REDIRECT_URI).await;
    let query = fixture.approved_query(&first).await;
    fixture
        .connector
        .finish_server_login(first, &query, &fixture.cancellation)
        .await
        .unwrap();
    let registered = fixture.credentials.current().unwrap().registration;

    let again = fixture.start_server_login(REDIRECT_URI).await;
    assert_eq!(
        query_field(again.authorization_url(), "client_id"),
        Some(registered.client_id.clone())
    );
    assert_eq!(fixture.server.requests_to(REGISTRATION_PATH).len(), 1);

    let elsewhere = fixture.start_server_login(OTHER_REDIRECT_URI).await;
    assert_ne!(
        query_field(elsewhere.authorization_url(), "client_id"),
        Some(registered.client_id)
    );
    let registrations = fixture.server.requests_to(REGISTRATION_PATH);
    assert_eq!(registrations.len(), 2);
    assert_eq!(
        registrations[1].json.as_ref().unwrap()["redirect_uris"],
        json!([OTHER_REDIRECT_URI])
    );
    let query = fixture.approved_query(&elsewhere).await;
    fixture
        .connector
        .finish_server_login(elsewhere, &query, &fixture.cancellation)
        .await
        .unwrap();
    assert_eq!(
        fixture
            .credentials
            .current()
            .unwrap()
            .registration
            .redirect_uri,
        OTHER_REDIRECT_URI
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_configured_client_id_is_used_without_registering() {
    let fixture = Fixture::with_settings(OAuthSettings {
        client_id: Some("configured-client".into()),
        ..OAuthSettings::default()
    });
    let login = fixture.start_server_login(REDIRECT_URI).await;

    assert_eq!(
        query_field(login.authorization_url(), "client_id").as_deref(),
        Some("configured-client")
    );
    assert!(fixture.server.requests_to(REGISTRATION_PATH).is_empty());
    let query = fixture.approved_query(&login).await;
    fixture
        .connector
        .finish_server_login(login, &query, &fixture.cancellation)
        .await
        .unwrap();
    let credential = fixture.credentials.current().unwrap();
    assert_eq!(credential.registration.client_id, "configured-client");
}

#[tokio::test]
async fn a_redirect_address_must_be_https_without_a_fragment() {
    let fixture = Fixture::new();
    for redirect_uri in [
        "http://chat.example.test/connectors/callback",
        "https://chat.example.test/connectors/callback#fragment",
        "https://user:secret@chat.example.test/connectors/callback",
        "not an address",
    ] {
        assert_eq!(
            fixture
                .connector
                .start_server_login(redirect_uri, None, &fixture.cancellation)
                .await
                .unwrap_err(),
            LoginError::RedirectAddress,
            "{redirect_uri}"
        );
    }
    assert!(fixture.server.requests().is_empty());
}

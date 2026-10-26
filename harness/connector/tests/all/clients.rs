use std::sync::Arc;

use tick_oauth_test_server::{
    AuthorizationServer, METADATA_PATH, PROTECTED_RESOURCE_METADATA_PATH, REGISTRATION_PATH,
};
use tickloop_harness_connector::{
    ConfigurationError, Connector, InMemoryCredentials, OAuthSettings,
};

use crate::support::{Fixture, query_field};

#[tokio::test(flavor = "multi_thread")]
async fn a_configured_client_id_skips_registration() {
    let fixture = Fixture::with_settings(OAuthSettings {
        client_id: Some("preregistered-client".into()),
        ..OAuthSettings::default()
    });
    fixture.log_in().await;

    assert!(fixture.server.requests_to(REGISTRATION_PATH).is_empty());
    let credential = fixture.credentials.current().unwrap();
    assert_eq!(credential.registration.client_id, "preregistered-client");
    let token = fixture
        .connector
        .access_token(&fixture.cancellation)
        .await
        .unwrap();
    assert!(fixture.resource_accepts(&token).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_configured_metadata_address_replaces_discovery() {
    let server = AuthorizationServer::start();
    let metadata_url = format!("{}{METADATA_PATH}", server.issuer());
    let connector = Connector::new(
        &server.resource(),
        OAuthSettings {
            authorization_server_metadata_url: Some(metadata_url),
            ..OAuthSettings::default()
        },
        Arc::new(InMemoryCredentials::default()),
    )
    .unwrap();

    let login = connector
        .start_login(None, &Default::default())
        .await
        .unwrap();

    assert!(
        server
            .requests()
            .iter()
            .all(|request| !request.path.starts_with(PROTECTED_RESOURCE_METADATA_PATH))
    );
    assert_eq!(
        query_field(login.authorization_url(), "client_id").as_deref(),
        Some("client-1")
    );
}

#[test]
fn remote_addresses_must_use_https() {
    let credentials = Arc::new(InMemoryCredentials::default());
    let connect = |server: &str, metadata: Option<&str>| {
        Connector::new(
            server,
            OAuthSettings {
                authorization_server_metadata_url: metadata.map(str::to_owned),
                ..OAuthSettings::default()
            },
            credentials.clone(),
        )
        .map(|_| ())
    };

    assert_eq!(
        connect("http://mcp.example.test/mcp", None),
        Err(ConfigurationError::ServerAddress)
    );
    assert_eq!(
        connect(
            "https://mcp.example.test/mcp",
            Some("http://auth.example.test/.well-known/oauth-authorization-server")
        ),
        Err(ConfigurationError::MetadataAddress)
    );
    assert_eq!(connect("https://mcp.example.test/mcp", None), Ok(()));
    assert_eq!(connect("http://127.0.0.1:9/mcp", None), Ok(()));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_login_reuses_the_kept_registration() {
    let fixture = Fixture::new();
    fixture.log_in().await;
    fixture
        .connector
        .logout(&fixture.cancellation)
        .await
        .unwrap();
    fixture.log_in().await;

    assert_eq!(fixture.server.requests_to(REGISTRATION_PATH).len(), 1);
    let token = fixture
        .connector
        .access_token(&fixture.cancellation)
        .await
        .unwrap();
    assert!(fixture.resource_accepts(&token).await);
}

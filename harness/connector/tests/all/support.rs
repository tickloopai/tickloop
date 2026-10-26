use std::sync::Arc;

use reqwest::{StatusCode, header::WWW_AUTHENTICATE};
use tick_oauth_test_server::AuthorizationServer;
use tickloop_harness_connector::{
    AccessToken, Connector, InMemoryCredentials, OAuthSettings, PendingLogin,
};
use tokio_util::sync::CancellationToken;

pub struct Fixture {
    pub server: AuthorizationServer,
    pub credentials: Arc<InMemoryCredentials>,
    pub connector: Connector,
    pub cancellation: CancellationToken,
}

impl Fixture {
    pub fn new() -> Self {
        Self::with_settings(OAuthSettings::default())
    }

    pub fn with_settings(settings: OAuthSettings) -> Self {
        let server = AuthorizationServer::start();
        let credentials = Arc::new(InMemoryCredentials::default());
        let connector = Connector::new(&server.resource(), settings, credentials.clone()).unwrap();
        Self {
            server,
            credentials,
            connector,
            cancellation: CancellationToken::new(),
        }
    }

    /// A second Agent using the same server and credentials.
    pub fn another_connector(&self) -> Connector {
        Connector::new(
            &self.server.resource(),
            OAuthSettings::default(),
            self.credentials.clone(),
        )
        .unwrap()
    }

    /// The `WWW-Authenticate` challenge the resource answers an unauthenticated request with.
    pub async fn challenge(&self) -> String {
        let response = http_client()
            .post(self.server.resource())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        response.headers()[WWW_AUTHENTICATE]
            .to_str()
            .unwrap()
            .to_owned()
    }

    pub async fn start_login(&self) -> PendingLogin {
        let challenge = self.challenge().await;
        self.connector
            .start_login(Some(&challenge), &self.cancellation)
            .await
            .unwrap()
    }

    /// The redirect address the authorization server sends an approving user's browser to.
    pub async fn approve(&self, login: &PendingLogin) -> String {
        tokio::task::block_in_place(|| self.server.approve(login.authorization_url()))
    }

    /// Logs in through the loopback callback.
    pub async fn log_in(&self) {
        let login = self.start_login().await;
        let redirect = self.approve(&login).await;
        let browser = http_client().get(&redirect).send().await.unwrap();
        assert_eq!(browser.status(), StatusCode::OK);
        login.finish(&self.cancellation).await.unwrap();
    }

    /// Whether the resource accepts `token`.
    pub async fn resource_accepts(&self, token: &AccessToken) -> bool {
        http_client()
            .post(self.server.resource())
            .bearer_auth(token.secret())
            .send()
            .await
            .unwrap()
            .status()
            == StatusCode::OK
    }
}

pub fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

pub fn query_field(url: &str, name: &str) -> Option<String> {
    reqwest::Url::parse(url)
        .unwrap()
        .query_pairs()
        .find(|(field, _)| field == name)
        .map(|(_, value)| value.into_owned())
}

use std::{
    fs,
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::Value;
use tick_oauth::{DeviceLogin, LoginError};
use tick_packages::{OAuthIssuer, PackageAuth, PackageCredentials};
use tick_test_support::authorization_server::AuthorizationServer;
use tokio_util::sync::CancellationToken;

/// A package's credentials in a private config root, and a local server to log in with.
pub struct Login {
    _directory: tempfile::TempDir,
    pub credentials: PackageCredentials,
    pub server: AuthorizationServer,
}

impl Login {
    pub fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let credentials = PackageCredentials::in_config_root(directory.path(), "sample").unwrap();
        Self {
            _directory: directory,
            credentials,
            server: AuthorizationServer::start(),
        }
    }

    pub fn auth(&self) -> PackageAuth {
        PackageAuth::OAuth2 {
            issuers: vec![OAuthIssuer {
                label: "Local".into(),
                issuer: self.server.issuer().to_owned(),
            }],
            scopes: Vec::new(),
        }
    }

    pub async fn start(&self) -> Result<DeviceLogin, LoginError> {
        self.start_with(&[]).await
    }

    pub async fn start_with(&self, scopes: &[String]) -> Result<DeviceLogin, LoginError> {
        DeviceLogin::start(http(), &self.credentials, self.server.issuer(), scopes).await
    }

    /// Starts a login and waits for it to end.
    pub async fn run(&self) -> Result<(), LoginError> {
        self.start()
            .await?
            .finish(&CancellationToken::new(), |_| {})
            .await
    }

    pub fn saved(&self) -> Value {
        serde_json::from_slice(&fs::read(self.credentials.oauth_token_path()).unwrap()).unwrap()
    }
}

/// A client as a caller configures one: no redirects.
pub fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .unwrap()
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

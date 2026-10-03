use std::{
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicBool},
    time::{Duration, Instant},
};

use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use ring::{
    rand::SystemRandom,
    signature::{RSA_PKCS1_SHA256, RsaKeyPair},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::{net::TcpListener, sync::mpsc};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{
    BrowserLogin, BrowserSession,
    provider_store::{ProviderStore, read_private_json},
};
use crate::{
    model::{
        ModelConfig, ModelError, ModelRequestContext, ModelRequestStage, credential_fingerprint,
        credentials,
    },
    network::{HttpPolicy, build_http_client},
    private_fs::unix_time_milliseconds,
};

const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform https://www.googleapis.com/auth/generative-language.retriever";
const REFRESH_SKEW_MS: u64 = 60_000;

#[derive(Clone)]
pub struct GoogleAuth {
    root: PathBuf,
    token_url: String,
}

#[derive(Deserialize, Serialize)]
struct State {
    generation: Uuid,
    credentials: Option<OAuthCredentials>,
}

#[derive(Deserialize, Serialize)]
struct OAuthCredentials {
    identity: Uuid,
    access_token: String,
    refresh_token: String,
    expires_at_ms: u64,
    client_id: String,
    client_secret: String,
    quota_project_id: String,
}

#[derive(Deserialize)]
struct ServiceAccount {
    #[serde(rename = "type")]
    kind: String,
    project_id: String,
    client_email: String,
    private_key: String,
    token_uri: String,
}

#[derive(Deserialize, Serialize)]
struct ServiceToken {
    identity: String,
    token: String,
    expires_at_ms: u64,
}

pub struct GoogleAccess {
    pub token: String,
    pub identity: String,
    pub project: String,
    pub base_url: String,
}

pub struct GoogleLogin {
    pub browser: BrowserLogin,
    generation: Uuid,
    client_id: String,
    client_secret: String,
    project: String,
}

impl GoogleAuth {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            token_url: TOKEN_URL.into(),
        }
    }
    pub fn with_loopback_test_token_endpoint(mut self, endpoint: &str) -> Result<Self, ModelError> {
        let url = reqwest::Url::parse(endpoint)
            .map_err(|_| configuration("invalid Google test token endpoint"))?;
        if !cfg!(debug_assertions)
            || url.scheme() != "http"
            || !crate::network::is_loopback(&url)
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(configuration(
                "Google test token endpoint must use loopback HTTP",
            ));
        }
        self.token_url = endpoint.into();
        Ok(self)
    }
    fn store(&self) -> ProviderStore {
        ProviderStore::new(&self.root, "google-oauth")
    }

    pub fn has_saved_login(&self) -> Result<bool, ModelError> {
        Ok(self
            .store()
            .read::<State>()?
            .is_some_and(|state| state.credentials.is_some()))
    }

    pub fn service_account_configured(&self) -> Result<bool, ModelError> {
        let Some(path) = setting(&self.root, "GOOGLE_APPLICATION_CREDENTIALS")? else {
            return Ok(false);
        };
        let account: ServiceAccount = read_private_json(Path::new(&path))?
            .ok_or_else(|| configuration("Google service-account file is missing"))?;
        validate_service_account(&account)?;
        Ok(true)
    }

    pub fn service_account_path(&self) -> Result<Option<String>, ModelError> {
        setting(&self.root, "GOOGLE_APPLICATION_CREDENTIALS")
    }

    pub fn save_service_account_path(&self, path: &str) -> Result<(), ModelError> {
        if !Path::new(path).is_absolute() {
            return Err(configuration(
                "Enter an absolute path to the service-account JSON file",
            ));
        }
        let account: ServiceAccount = read_private_json(Path::new(path))
            .map_err(|_| configuration("Choose a readable service-account JSON file with private permissions (0600) and no symbolic links"))?
            .ok_or_else(|| configuration("Google service-account file is missing"))?;
        validate_service_account(&account)?;
        service_assertion(&account)?;
        credentials::save_api_key(&self.root, "GOOGLE_APPLICATION_CREDENTIALS", path)
            .map_err(|error| configuration(&error.to_string()))
    }

    pub async fn start_browser_login(
        &self,
        cancel: &CancellationToken,
    ) -> Result<GoogleLogin, ModelError> {
        let client_id = setting(&self.root, "GOOGLE_OAUTH_CLIENT_ID")?.ok_or_else(|| configuration("Set GOOGLE_OAUTH_CLIENT_ID and GOOGLE_OAUTH_CLIENT_SECRET for your own Google installed-app client before signing in"))?;
        let client_secret =
            setting(&self.root, "GOOGLE_OAUTH_CLIENT_SECRET")?.ok_or_else(|| {
                configuration(
                    "GOOGLE_OAUTH_CLIENT_SECRET is required for your Google installed-app client",
                )
            })?;
        let project = setting(&self.root, "GOOGLE_CLOUD_PROJECT")?
            .ok_or_else(|| configuration("GOOGLE_CLOUD_PROJECT is required for OAuth billing"))?;
        validate_identifier(&project)?;
        let listener = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(ModelError::Cancelled),
            result = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)) => result.map_err(|_| configuration("Google OAuth callback port is unavailable"))?,
        };
        let callback_uri = format!(
            "http://localhost:{}/auth/callback",
            listener
                .local_addr()
                .map_err(|_| configuration("Google OAuth callback unavailable"))?
                .port()
        );
        let verifier = super::new_pkce_verifier();
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let state = Uuid::new_v4().simple().to_string();
        let mut url = reqwest::Url::parse("https://accounts.google.com/o/oauth2/v2/auth")
            .expect("Google authorization URL");
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &client_id)
            .append_pair("redirect_uri", &callback_uri)
            .append_pair("scope", SCOPE)
            .append_pair("access_type", "offline")
            .append_pair("prompt", "consent")
            .append_pair("code_challenge_method", "S256")
            .append_pair("code_challenge", &challenge)
            .append_pair("state", &state);
        let store = self.store();
        let lock = store.lock(cancel).await?;
        let generation = Uuid::new_v4();
        let credentials = store.read::<State>()?.and_then(|state| state.credentials);
        store.save(&State {
            generation,
            credentials,
        })?;
        drop(lock);
        let (sender, receiver) = mpsc::unbounded_channel();
        let stop = CancellationToken::new();
        let completed = Arc::new(AtomicBool::new(false));
        let task = tokio::spawn(super::serve_browser_callbacks(
            listener,
            callback_uri.clone(),
            state.clone(),
            sender.clone(),
            completed.clone(),
            stop.clone(),
        ));
        Ok(GoogleLogin {
            generation,
            client_id,
            client_secret,
            project,
            browser: BrowserLogin {
                generation,
                authorization_url: url.into(),
                callback_uri,
                session: BrowserSession {
                    verifier,
                    started_at: Instant::now(),
                    callback_codes: receiver,
                    callback_sender: sender,
                    callback_task: task,
                    stop,
                    completed,
                    state,
                },
            },
        })
    }

    pub async fn finish_browser_login(
        &self,
        mut login: GoogleLogin,
        cancel: &CancellationToken,
    ) -> Result<String, ModelError> {
        let code = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(ModelError::Cancelled),
            () = tokio::time::sleep(Duration::from_secs(15 * 60)) => return Err(configuration("Google OAuth authorization expired")),
            code = login.browser.session.callback_codes.recv() => code.ok_or_else(|| configuration("Google OAuth callback ended"))?.map_err(|_| configuration("Google OAuth authorization denied or invalid callback"))?,
        };
        let value = token_request(
            &self.token_url,
            &[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("client_id", &login.client_id),
                ("client_secret", &login.client_secret),
                ("redirect_uri", &login.browser.callback_uri),
                ("code_verifier", &login.browser.session.verifier),
            ],
            cancel,
        )
        .await?;
        let credentials = OAuthCredentials {
            identity: Uuid::new_v4(),
            access_token: token_field(&value, "access_token")?,
            refresh_token: token_field(&value, "refresh_token")?,
            expires_at_ms: expiry(&value)?,
            client_id: login.client_id.clone(),
            client_secret: login.client_secret.clone(),
            quota_project_id: login.project.clone(),
        };
        let store = self.store();
        let _lock = store.lock(cancel).await?;
        if store
            .read::<State>()?
            .is_none_or(|state| state.generation != login.generation)
        {
            return Err(configuration("Google login was superseded or signed out"));
        }
        let identity = credentials.identity.to_string();
        store.save(&State {
            generation: Uuid::new_v4(),
            credentials: Some(credentials),
        })?;
        Ok(identity)
    }

    pub async fn import_authorized_user(
        &self,
        path: &Path,
        cancel: &CancellationToken,
    ) -> Result<(), ModelError> {
        let value: Value = read_private_json(path)?
            .ok_or_else(|| configuration("Google OAuth import file is missing"))?;
        if value["type"] != "authorized_user" {
            return Err(configuration(
                "Google OAuth import requires an authorized_user credential file",
            ));
        }
        let project = value["quota_project_id"]
            .as_str()
            .map(str::to_owned)
            .or(setting(&self.root, "GOOGLE_CLOUD_PROJECT")?)
            .ok_or_else(|| configuration("Google OAuth quota project is required"))?;
        validate_identifier(&project)?;
        let credentials = OAuthCredentials {
            identity: Uuid::new_v4(),
            access_token: String::new(),
            refresh_token: token_field(&value, "refresh_token")?,
            expires_at_ms: 0,
            client_id: token_field(&value, "client_id")?,
            client_secret: token_field(&value, "client_secret")?,
            quota_project_id: project,
        };
        let store = self.store();
        let _lock = store.lock(cancel).await?;
        store.save(&State {
            generation: Uuid::new_v4(),
            credentials: Some(credentials),
        })
    }

    pub async fn logout(&self, cancel: &CancellationToken) -> Result<(), ModelError> {
        let store = self.store();
        let _lock = store.lock(cancel).await?;
        store.save(&State {
            generation: Uuid::new_v4(),
            credentials: None,
        })
    }

    pub async fn resolve(
        &self,
        config: &ModelConfig,
        cancel: &CancellationToken,
        context: Option<&ModelRequestContext>,
    ) -> Result<GoogleAccess, ModelError> {
        let (token, identity, project) = if config.auth_source
            == crate::model::ModelAuthSource::GoogleCloud
        {
            let path = setting(&self.root, "GOOGLE_APPLICATION_CREDENTIALS")?.ok_or_else(|| {
                configuration(
                    "Set GOOGLE_APPLICATION_CREDENTIALS to a private service-account JSON file",
                )
            })?;
            let account: ServiceAccount = read_private_json(Path::new(&path))?
                .ok_or_else(|| configuration("Google service-account file is missing"))?;
            validate_service_account(&account)?;
            let identity = credential_fingerprint(
                format!("{}:{}", account.client_email, account.private_key).as_bytes(),
            );
            let project =
                setting(&self.root, "GOOGLE_CLOUD_PROJECT")?.unwrap_or(account.project_id.clone());
            validate_identifier(&project)?;
            let store = ProviderStore::new(&self.root, "google-service-token");
            let _lock = store.lock(cancel).await?;
            if let Some(cached) = store.read::<ServiceToken>()?.filter(|cached| {
                cached.identity == identity
                    && cached.expires_at_ms
                        > unix_time_milliseconds().saturating_add(REFRESH_SKEW_MS)
            }) {
                (cached.token, identity, project)
            } else {
                begin_refresh(context)?;
                let assertion = service_assertion(&account)?;
                let value = token_request(
                    &self.token_url,
                    &[
                        ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
                        ("assertion", &assertion),
                    ],
                    cancel,
                )
                .await?;
                let token = token_field(&value, "access_token")?;
                store.save(&ServiceToken {
                    identity: identity.clone(),
                    token: token.clone(),
                    expires_at_ms: expiry(&value)?,
                })?;
                (token, identity, project)
            }
        } else {
            let store = self.store();
            let _lock = store.lock(cancel).await?;
            let mut state = store
                .read::<State>()?
                .ok_or_else(|| configuration("Google OAuth is not logged in; use /provider"))?;
            let credential = state
                .credentials
                .as_mut()
                .ok_or_else(|| configuration("Google OAuth is not logged in; use /provider"))?;
            if credential.expires_at_ms <= unix_time_milliseconds().saturating_add(REFRESH_SKEW_MS)
            {
                begin_refresh(context)?;
                let value = token_request(
                    &self.token_url,
                    &[
                        ("grant_type", "refresh_token"),
                        ("refresh_token", &credential.refresh_token),
                        ("client_id", &credential.client_id),
                        ("client_secret", &credential.client_secret),
                    ],
                    cancel,
                )
                .await?;
                credential.access_token = token_field(&value, "access_token")?;
                if value.get("refresh_token").is_some() {
                    credential.refresh_token = token_field(&value, "refresh_token")?;
                }
                credential.expires_at_ms = expiry(&value)?;
                store.save(&state)?;
            }
            let credential = state.credentials.as_ref().expect("checked credentials");
            (
                credential.access_token.clone(),
                credential.identity.to_string(),
                credential.quota_project_id.clone(),
            )
        };
        let testing = cfg!(debug_assertions)
            && reqwest::Url::parse(&config.base_url)
                .is_ok_and(|url| url.scheme() == "http" && crate::network::is_loopback(&url));
        let base_url = if config.provider_id.starts_with("google-vertex") && !testing {
            let location =
                setting(&self.root, "GOOGLE_CLOUD_LOCATION")?.unwrap_or_else(|| "global".into());
            validate_identifier(&location)?;
            let host = if location == "global" {
                "aiplatform.googleapis.com".to_owned()
            } else {
                format!("{location}-aiplatform.googleapis.com")
            };
            format!("https://{host}/v1/projects/{project}/locations/{location}/publishers/google")
        } else {
            config.base_url.clone()
        };
        Ok(GoogleAccess {
            token,
            identity,
            project,
            base_url,
        })
    }
}

pub(super) fn setting(root: &Path, name: &str) -> Result<Option<String>, ModelError> {
    let saved = credentials::load_api_key(root, name)
        .map_err(|_| configuration("Google setting could not be read safely"))?;
    Ok(saved
        .or_else(|| std::env::var(name).ok())
        .filter(|value| !value.trim().is_empty()))
}

fn validate_identifier(value: &str) -> Result<(), ModelError> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
    {
        return Err(configuration("invalid Google project or location"));
    }
    Ok(())
}

fn validate_service_account(account: &ServiceAccount) -> Result<(), ModelError> {
    if account.kind != "service_account"
        || account.token_uri != TOKEN_URL
        || account.client_email.is_empty()
    {
        return Err(configuration(
            "Google service-account credentials require the official token origin",
        ));
    }
    validate_identifier(&account.project_id)
}

fn service_assertion(account: &ServiceAccount) -> Result<String, ModelError> {
    let pem = account
        .private_key
        .strip_prefix("-----BEGIN PRIVATE KEY-----")
        .and_then(|value| value.trim().strip_suffix("-----END PRIVATE KEY-----"))
        .ok_or_else(|| configuration("Google private key must be PKCS8 PEM"))?;
    let der = STANDARD
        .decode(
            pem.chars()
                .filter(|character| !character.is_whitespace())
                .collect::<String>(),
        )
        .map_err(|_| configuration("invalid Google private key"))?;
    let key =
        RsaKeyPair::from_pkcs8(&der).map_err(|_| configuration("invalid Google private key"))?;
    let now = unix_time_milliseconds() / 1000;
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
    let claims = URL_SAFE_NO_PAD.encode(json!({"iss": account.client_email, "scope": "https://www.googleapis.com/auth/cloud-platform", "aud": TOKEN_URL, "iat": now, "exp": now + 3600}).to_string());
    let signing_input = format!("{header}.{claims}");
    let mut signature = vec![0; key.public().modulus_len()];
    key.sign(
        &RSA_PKCS1_SHA256,
        &SystemRandom::new(),
        signing_input.as_bytes(),
        &mut signature,
    )
    .map_err(|_| configuration("Google assertion signing failed"))?;
    Ok(format!(
        "{signing_input}.{}",
        URL_SAFE_NO_PAD.encode(signature)
    ))
}

pub(super) fn begin_refresh(context: Option<&ModelRequestContext>) -> Result<(), ModelError> {
    if context.is_some_and(|context| !context.try_begin_attempt()) {
        return Err(ModelError::RequestBudgetExhausted {
            stage: ModelRequestStage::Credentials,
        });
    }
    Ok(())
}

pub(super) fn token_field(value: &Value, name: &str) -> Result<String, ModelError> {
    value[name]
        .as_str()
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| configuration("invalid provider authorization response"))
}

pub(super) fn expiry(value: &Value) -> Result<u64, ModelError> {
    let seconds = value["expires_in"]
        .as_u64()
        .filter(|seconds| *seconds > 0)
        .ok_or_else(|| configuration("invalid provider token expiry"))?;
    unix_time_milliseconds()
        .checked_add(
            seconds
                .checked_mul(1000)
                .ok_or_else(|| configuration("invalid provider token expiry"))?,
        )
        .ok_or_else(|| configuration("invalid provider token expiry"))
}

pub(super) async fn token_request(
    url: &str,
    form: &[(&str, &str)],
    cancel: &CancellationToken,
) -> Result<Value, ModelError> {
    let url = reqwest::Url::parse(url)
        .map_err(|_| configuration("invalid provider authorization origin"))?;
    let mut policy = HttpPolicy::new(reqwest::redirect::Policy::none());
    policy.timeout = Some(Duration::from_secs(15));
    let (client, _) = build_http_client(&url, policy).map_err(|_| ModelError::ConnectionAt {
        stage: ModelRequestStage::Credentials,
    })?;
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(ModelError::Cancelled),
        result = async {
            let mut response = client.post(url).form(form).send().await.map_err(|_| ModelError::ConnectionAt { stage: ModelRequestStage::Credentials })?;
            let status = response.status().as_u16();
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(|_| ModelError::ConnectionAt { stage: ModelRequestStage::Credentials })? {
                if bytes.len().saturating_add(chunk.len()) > 64 * 1024 { return Err(configuration("provider authorization response too large")); }
                bytes.extend_from_slice(&chunk);
            }
            let value: Value = serde_json::from_slice(&bytes).map_err(|_| configuration("invalid provider authorization response"))?;
            if !(200..300).contains(&status) && value["error"].as_str().is_none() { return Err(ModelError::Http { status }.at_stage(ModelRequestStage::Credentials)); }
            Ok(value)
        } => result,
    }
}

fn configuration(message: &str) -> ModelError {
    ModelError::Configuration(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        auth::provider_test_support::{issuer, root},
        model::cards::ModelCatalog,
    };

    #[test]
    fn service_account_path_is_validated_before_saving_and_never_copies_credentials() {
        use std::os::unix::fs::PermissionsExt;

        let root = root();
        credentials::save_api_key(root.path(), "OTHER_PROVIDER_KEY", "preserved").unwrap();
        let auth = GoogleAuth::new(root.path());
        let source = root.path().join("service account.json");
        let account = json!({
            "type": "service_account",
            "project_id": "fixture-project",
            "client_email": "fixture@fixture-project.iam.gserviceaccount.com",
            "private_key": include_str!("../../tests/fixtures/google-test-private-key.pem"),
            "token_uri": TOKEN_URL
        });
        let bytes = serde_json::to_vec(&account).unwrap();
        std::fs::write(&source, &bytes).unwrap();
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o600)).unwrap();
        auth.save_service_account_path(source.to_str().unwrap())
            .unwrap();
        assert!(auth.service_account_configured().unwrap());
        assert_eq!(
            auth.service_account_path().unwrap().as_deref(),
            source.to_str()
        );
        let saved = std::fs::read(root.path().join(".env")).unwrap();
        assert_eq!(std::fs::read(&source).unwrap(), bytes);
        assert!(!root.path().join("auth").exists());
        assert!(!String::from_utf8_lossy(&saved).contains("PRIVATE KEY"));
        assert_eq!(
            credentials::load_api_key(root.path(), "OTHER_PROVIDER_KEY")
                .unwrap()
                .as_deref(),
            Some("preserved")
        );

        assert!(auth.save_service_account_path("relative.json").is_err());
        assert!(
            auth.save_service_account_path(root.path().join("missing.json").to_str().unwrap())
                .is_err()
        );
        let link = root.path().join("symlink.json");
        std::os::unix::fs::symlink(&source, &link).unwrap();
        assert!(
            auth.save_service_account_path(link.to_str().unwrap())
                .is_err()
        );
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            auth.save_service_account_path(source.to_str().unwrap())
                .is_err()
        );
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o600)).unwrap();
        for (field, value) in [
            ("type", "authorized_user"),
            ("token_uri", "https://untrusted.example/token"),
            ("project_id", "invalid/project"),
            ("private_key", "invalid private key"),
        ] {
            let mut invalid = account.clone();
            invalid[field] = json!(value);
            std::fs::write(&source, serde_json::to_vec(&invalid).unwrap()).unwrap();
            assert!(
                auth.save_service_account_path(source.to_str().unwrap())
                    .is_err()
            );
            assert_eq!(std::fs::read(root.path().join(".env")).unwrap(), saved);
        }
    }

    #[tokio::test]
    async fn own_client_pkce_login_refresh_and_logout_use_independent_credentials() {
        let root = root();
        for (name, value) in [
            ("GOOGLE_OAUTH_CLIENT_ID", "own-installed-client"),
            ("GOOGLE_OAUTH_CLIENT_SECRET", "own-client-secret"),
            ("GOOGLE_CLOUD_PROJECT", "fixture-project"),
        ] {
            credentials::save_api_key(root.path(), name, value).unwrap();
        }
        let (origin, mut captured) = issuer(vec![
            (
                200,
                json!({"access_token":"initial", "refresh_token":"refresh", "expires_in":30}),
            ),
            (200, json!({"access_token":"renewed", "expires_in":3600})),
        ])
        .await;
        let auth = GoogleAuth::new(root.path())
            .with_loopback_test_token_endpoint(&format!("{origin}/token"))
            .unwrap();
        let cancel = CancellationToken::new();
        let login = auth.start_browser_login(&cancel).await.unwrap();
        let url = reqwest::Url::parse(&login.browser.authorization_url).unwrap();
        let fields = url
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(fields["client_id"], "own-installed-client");
        assert_eq!(fields["code_challenge_method"], "S256");
        assert!(fields["scope"].contains("cloud-platform"));
        let callback = login.browser.manual_callback();
        assert!(
            callback
                .submit(&format!(
                    "{}?code=fixture-code&state=wrong-state",
                    login.browser.callback_uri
                ))
                .is_err()
        );
        assert!(
            callback
                .submit(&format!(
                    "https://attacker.example/auth/callback?code=fixture-code&state={}",
                    fields["state"]
                ))
                .is_err()
        );
        callback
            .submit(&format!(
                "{}?code=fixture-code&state={}",
                login.browser.callback_uri, fields["state"]
            ))
            .unwrap();
        let identity = auth.finish_browser_login(login, &cancel).await.unwrap();
        let request = captured.recv().await.unwrap();
        assert!(request.contains("grant_type=authorization_code"));
        assert!(request.contains("code_verifier="));
        let config = ModelCatalog::defaults()
            .get("google-vertex-oauth", "gemini-3.8-flash")
            .unwrap();
        let access = auth.resolve(&config, &cancel, None).await.unwrap();
        assert_eq!(identity, access.identity);
        assert_eq!(access.token, "renewed");
        assert_eq!(
            access.base_url,
            "https://aiplatform.googleapis.com/v1/projects/fixture-project/locations/global/publishers/google"
        );
        assert!(
            captured
                .recv()
                .await
                .unwrap()
                .contains("refresh_token=refresh")
        );
        auth.logout(&cancel).await.unwrap();
        assert!(!auth.has_saved_login().unwrap());
        assert!(auth.resolve(&config, &cancel, None).await.is_err());
    }

    #[tokio::test]
    async fn late_browser_exchange_after_logout_cannot_restore_google_login() {
        let root = root();
        for (name, value) in [
            ("GOOGLE_OAUTH_CLIENT_ID", "client"),
            ("GOOGLE_OAUTH_CLIENT_SECRET", "secret"),
            ("GOOGLE_CLOUD_PROJECT", "project"),
        ] {
            credentials::save_api_key(root.path(), name, value).unwrap();
        }
        let (origin, _captured) = issuer(vec![(
            200,
            json!({"access_token":"initial", "refresh_token":"refresh", "expires_in":3600}),
        )])
        .await;
        let auth = GoogleAuth::new(root.path())
            .with_loopback_test_token_endpoint(&origin)
            .unwrap();
        let cancel = CancellationToken::new();
        let login = auth.start_browser_login(&cancel).await.unwrap();
        login
            .browser
            .session
            .callback_sender
            .send(Ok("code".into()))
            .unwrap();
        auth.logout(&cancel).await.unwrap();
        assert!(auth.finish_browser_login(login, &cancel).await.is_err());
        assert!(!auth.has_saved_login().unwrap());
    }

    #[tokio::test]
    async fn import_rejects_symlinks_and_token_errors_do_not_expose_remote_body() {
        let root = root();
        let source = root.path().join("user.json");
        std::fs::write(&source, json!({"type":"authorized_user", "client_id":"client", "client_secret":"secret", "refresh_token":"refresh", "quota_project_id":"project"}).to_string()).unwrap();
        use std::os::unix::fs::{PermissionsExt, symlink};
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = root.path().join("linked.json");
        symlink(&source, &link).unwrap();
        let (origin, _captured) = issuer(vec![(
            400,
            json!({"error":"invalid_grant", "error_description":"fixture-remote-secret"}),
        )])
        .await;
        let auth = GoogleAuth::new(root.path())
            .with_loopback_test_token_endpoint(&origin)
            .unwrap();
        let cancel = CancellationToken::new();
        assert!(auth.import_authorized_user(&link, &cancel).await.is_err());
        auth.import_authorized_user(&source, &cancel).await.unwrap();
        let config = ModelCatalog::defaults()
            .get("google-gemini-oauth", "gemini-3.8-flash")
            .unwrap();
        let error = auth.resolve(&config, &cancel, None).await.err().unwrap();
        assert!(!format!("{error:?}").contains("fixture-remote-secret"));
        assert!(auth.has_saved_login().unwrap());
        cancel.cancel();
        assert!(matches!(
            auth.resolve(&config, &cancel, None).await,
            Err(ModelError::Cancelled)
        ));
    }
}

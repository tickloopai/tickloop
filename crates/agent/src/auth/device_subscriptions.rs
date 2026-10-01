use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{
    google::{begin_refresh, expiry, token_field, token_request},
    provider_store::ProviderStore,
};
use crate::{
    model::{ModelAuthSource, ModelError, ModelRequestContext},
    private_fs::unix_time_milliseconds,
};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceSubscription {
    Kimi,
    Xai,
}

impl DeviceSubscription {
    pub fn from_auth(source: ModelAuthSource) -> Option<Self> {
        match source {
            ModelAuthSource::KimiOAuth => Some(Self::Kimi),
            ModelAuthSource::XaiOAuth => Some(Self::Xai),
            _ => None,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Kimi => "Kimi Code",
            Self::Xai => "xAI Grok",
        }
    }
    fn slug(self) -> &'static str {
        match self {
            Self::Kimi => "kimi-code-oauth",
            Self::Xai => "xai-oauth",
        }
    }
    fn client_id(self) -> &'static str {
        match self {
            Self::Kimi => "17e5f671-d194-4dfb-9706-5516cb48c098",
            Self::Xai => "b1a00492-073a-47ea-816f-4c329264a828",
        }
    }
    fn issuer(self) -> &'static str {
        match self {
            Self::Kimi => "https://auth.kimi.com",
            Self::Xai => "https://auth.x.ai",
        }
    }
    fn device_path(self) -> &'static str {
        match self {
            Self::Kimi => "/api/oauth/device_authorization",
            Self::Xai => "/oauth2/device/code",
        }
    }
    fn token_path(self) -> &'static str {
        match self {
            Self::Kimi => "/api/oauth/token",
            Self::Xai => "/oauth2/token",
        }
    }
    pub fn endpoint(self) -> &'static str {
        match self {
            Self::Kimi => "https://api.kimi.com/coding/v1",
            Self::Xai => "https://cli-chat-proxy.grok.com/v1",
        }
    }
    pub fn consent_notice(self) -> &'static str {
        match self {
            Self::Kimi => {
                "The consent page names the registered Kimi Code client. Tick Loop is an independent client; subscription model access remains subject to your account."
            }
            Self::Xai => {
                "The consent page uses the registered Grok client. This connection uses cli-chat-proxy.grok.com with its required token-auth headers; api.x.ai API keys are a separate route."
            }
        }
    }
}

#[derive(Clone)]
pub struct SubscriptionAuth {
    root: PathBuf,
    provider: DeviceSubscription,
    issuer: String,
}

#[derive(Deserialize, Serialize)]
struct State {
    provider: DeviceSubscription,
    generation: Uuid,
    credentials: Option<Credentials>,
}

#[derive(Deserialize, Serialize)]
struct Credentials {
    token: String,
    refresh_token: String,
    expires_at_ms: u64,
    identity: Uuid,
}

pub struct SubscriptionAccess {
    pub token: String,
    pub identity: Uuid,
}

pub struct DeviceLogin {
    pub user_code: String,
    pub verification_uri: String,
    device_code: String,
    deadline: Instant,
    interval: Duration,
    generation: Uuid,
}

impl SubscriptionAuth {
    pub fn new(root: impl Into<PathBuf>, provider: DeviceSubscription) -> Self {
        Self {
            root: root.into(),
            provider,
            issuer: provider.issuer().into(),
        }
    }

    pub fn with_loopback_test_issuer(mut self, issuer: &str) -> Result<Self, ModelError> {
        let url = reqwest::Url::parse(issuer).map_err(|_| auth_error("invalid test issuer"))?;
        if !cfg!(debug_assertions)
            || url.scheme() != "http"
            || !crate::network::is_loopback(&url)
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(auth_error("test issuer must be a loopback HTTP origin"));
        }
        self.issuer = issuer.trim_end_matches('/').into();
        Ok(self)
    }

    fn store(&self) -> ProviderStore {
        ProviderStore::new(&self.root, self.provider.slug())
    }
    fn read(&self) -> Result<Option<State>, ModelError> {
        let state = self.store().read::<State>()?;
        if state.as_ref().is_some_and(|state| {
            state.provider != self.provider
                || state.credentials.as_ref().is_some_and(|credentials| {
                    credentials.token.is_empty()
                        || credentials.refresh_token.is_empty()
                        || credentials.expires_at_ms == 0
                })
        }) {
            return Err(auth_error("invalid subscription credential file"));
        }
        Ok(state)
    }

    pub fn has_saved_login(&self) -> Result<bool, ModelError> {
        Ok(self
            .read()?
            .is_some_and(|state| state.credentials.is_some()))
    }

    pub async fn start_device_login(
        &self,
        cancel: &CancellationToken,
    ) -> Result<DeviceLogin, ModelError> {
        let store = self.store();
        let lock = store.lock(cancel).await?;
        let generation = Uuid::new_v4();
        store.save(&State {
            provider: self.provider,
            generation,
            credentials: self.read()?.and_then(|state| state.credentials),
        })?;
        drop(lock);
        let mut form = vec![("client_id", self.provider.client_id())];
        if self.provider == DeviceSubscription::Xai {
            form.push((
                "scope",
                "openid profile email offline_access grok-cli:access api:access",
            ));
        }
        let value = token_request(
            &format!("{}{}", self.issuer, self.provider.device_path()),
            &form,
            cancel,
        )
        .await?;
        let verification_uri = value["verification_uri_complete"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or(token_field(&value, "verification_uri")?);
        let url = reqwest::Url::parse(&verification_uri)
            .map_err(|_| auth_error("invalid subscription verification URL"))?;
        let trusted = url.scheme() == "https"
            && url.username().is_empty()
            && url.password().is_none()
            && match self.provider {
                DeviceSubscription::Kimi => matches!(
                    url.host_str(),
                    Some("auth.kimi.com" | "www.kimi.com" | "kimi.com")
                ),
                DeviceSubscription::Xai => matches!(
                    url.host_str(),
                    Some("auth.x.ai" | "accounts.x.ai" | "grok.com")
                ),
            };
        let testing = self.issuer.starts_with("http://")
            && url.origin()
                == reqwest::Url::parse(&self.issuer)
                    .expect("validated issuer")
                    .origin();
        if !trusted && !testing {
            return Err(auth_error("untrusted subscription verification origin"));
        }
        let seconds = value["expires_in"]
            .as_u64()
            .filter(|seconds| *seconds > 0)
            .ok_or_else(|| auth_error("invalid device expiry"))?
            .min(15 * 60);
        let interval = value["interval"].as_u64().unwrap_or(5).clamp(1, 60);
        Ok(DeviceLogin {
            user_code: token_field(&value, "user_code")?,
            verification_uri,
            device_code: token_field(&value, "device_code")?,
            deadline: Instant::now() + Duration::from_secs(seconds),
            interval: Duration::from_secs(interval),
            generation,
        })
    }

    pub async fn finish_device_login(
        &self,
        mut login: DeviceLogin,
        cancel: &CancellationToken,
    ) -> Result<SubscriptionAccess, ModelError> {
        loop {
            if Instant::now() >= login.deadline {
                return Err(auth_error("subscription device authorization expired"));
            }
            tokio::select! {
                biased;
                () = cancel.cancelled() => return Err(ModelError::Cancelled),
                () = tokio::time::sleep_until(tokio::time::Instant::from_std(login.deadline)) => return Err(auth_error("subscription device authorization expired")),
                () = tokio::time::sleep(login.interval) => {},
            }
            let value = token_request(
                &format!("{}{}", self.issuer, self.provider.token_path()),
                &[
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                    ("client_id", self.provider.client_id()),
                    ("device_code", &login.device_code),
                ],
                cancel,
            )
            .await?;
            match value["error"].as_str() {
                Some("authorization_pending") => continue,
                Some("slow_down") => {
                    login.interval = login.interval.saturating_add(Duration::from_secs(5));
                    continue;
                }
                Some("access_denied") => {
                    return Err(auth_error("subscription authorization denied"));
                }
                Some("expired_token") => {
                    return Err(auth_error("subscription authorization expired"));
                }
                Some(_) => return Err(auth_error("subscription authorization failed")),
                None => {}
            }
            let credential = parse_credentials(&value, None, Uuid::new_v4())?;
            let store = self.store();
            let _lock = store.lock(cancel).await?;
            if self
                .read()?
                .is_none_or(|state| state.generation != login.generation)
            {
                return Err(auth_error(
                    "subscription login was superseded or signed out",
                ));
            }
            let access = SubscriptionAccess {
                token: credential.token.clone(),
                identity: credential.identity,
            };
            store.save(&State {
                provider: self.provider,
                generation: Uuid::new_v4(),
                credentials: Some(credential),
            })?;
            return Ok(access);
        }
    }

    pub async fn resolve(
        &self,
        cancel: &CancellationToken,
        context: Option<&ModelRequestContext>,
    ) -> Result<SubscriptionAccess, ModelError> {
        let store = self.store();
        let _lock = store.lock(cancel).await?;
        let mut state = self
            .read()?
            .ok_or_else(|| auth_error("subscription is not logged in; use /provider"))?;
        let credential = state
            .credentials
            .as_ref()
            .ok_or_else(|| auth_error("subscription is not logged in; use /provider"))?;
        if credential.expires_at_ms <= unix_time_milliseconds().saturating_add(60_000) {
            begin_refresh(context)?;
            let value = token_request(
                &format!("{}{}", self.issuer, self.provider.token_path()),
                &[
                    ("grant_type", "refresh_token"),
                    ("client_id", self.provider.client_id()),
                    ("refresh_token", &credential.refresh_token),
                ],
                cancel,
            )
            .await?;
            if value.get("error").is_some() {
                return Err(auth_error(
                    "subscription refresh failed; sign in again from /provider",
                ));
            }
            state.credentials = Some(parse_credentials(
                &value,
                Some(&credential.refresh_token),
                credential.identity,
            )?);
            store.save(&state)?;
        }
        let credential = state.credentials.expect("checked credentials");
        Ok(SubscriptionAccess {
            token: credential.token,
            identity: credential.identity,
        })
    }

    pub async fn logout(&self, cancel: &CancellationToken) -> Result<(), ModelError> {
        let store = self.store();
        let _lock = store.lock(cancel).await?;
        store.save(&State {
            provider: self.provider,
            generation: Uuid::new_v4(),
            credentials: None,
        })
    }
}

fn parse_credentials(
    value: &Value,
    previous_refresh: Option<&str>,
    identity: Uuid,
) -> Result<Credentials, ModelError> {
    let refresh_token = if value.get("refresh_token").is_none() {
        previous_refresh
            .map(str::to_owned)
            .ok_or_else(|| auth_error("missing subscription refresh token"))?
    } else {
        token_field(value, "refresh_token")?
    };
    Ok(Credentials {
        token: token_field(value, "access_token")?,
        refresh_token,
        expires_at_ms: expiry(value)?,
        identity,
    })
}

fn auth_error(message: &str) -> ModelError {
    ModelError::Configuration(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::provider_test_support::{issuer, root};
    use serde_json::json;

    fn login(generation: Uuid) -> DeviceLogin {
        DeviceLogin {
            user_code: "fixture-code".into(),
            verification_uri: "https://auth.kimi.com".into(),
            device_code: "fixture-device".into(),
            deadline: Instant::now() + Duration::from_secs(5),
            interval: Duration::from_millis(1),
            generation,
        }
    }

    #[tokio::test]
    async fn pending_poll_refresh_rotation_and_logout_keep_identity_and_private_storage() {
        let root = root();
        let (origin, mut captured) = issuer(vec![
            (400, json!({"error":"authorization_pending"})),
            (
                200,
                json!({"access_token":"initial","refresh_token":"refresh-1","expires_in":30}),
            ),
            (
                200,
                json!({"access_token":"renewed","refresh_token":"refresh-2","expires_in":3600}),
            ),
        ])
        .await;
        let auth = SubscriptionAuth::new(root.path(), DeviceSubscription::Kimi)
            .with_loopback_test_issuer(&origin)
            .unwrap();
        let generation = Uuid::new_v4();
        auth.store()
            .save(&State {
                provider: DeviceSubscription::Kimi,
                generation,
                credentials: None,
            })
            .unwrap();
        let cancel = CancellationToken::new();
        let initial = auth
            .finish_device_login(login(generation), &cancel)
            .await
            .unwrap();
        assert!(
            captured
                .recv()
                .await
                .unwrap()
                .contains("device_code=fixture-device")
        );
        assert!(
            captured
                .recv()
                .await
                .unwrap()
                .contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code")
        );
        let refreshed = auth.resolve(&cancel, None).await.unwrap();
        assert_eq!(initial.identity, refreshed.identity);
        assert_eq!(refreshed.token, "renewed");
        assert!(
            captured
                .recv()
                .await
                .unwrap()
                .contains("refresh_token=refresh-1")
        );
        assert_eq!(
            auth.read()
                .unwrap()
                .unwrap()
                .credentials
                .unwrap()
                .refresh_token,
            "refresh-2"
        );
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(root.path().join("auth/kimi-code-oauth.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        auth.logout(&cancel).await.unwrap();
        assert!(!auth.has_saved_login().unwrap());
        assert!(auth.resolve(&cancel, None).await.is_err());
    }

    #[tokio::test]
    async fn signed_out_pending_login_cannot_restore_credentials() {
        let root = root();
        let (origin, _captured) = issuer(vec![(
            200,
            json!({"access_token":"initial","refresh_token":"refresh","expires_in":3600}),
        )])
        .await;
        let auth = SubscriptionAuth::new(root.path(), DeviceSubscription::Xai)
            .with_loopback_test_issuer(&origin)
            .unwrap();
        let generation = Uuid::new_v4();
        auth.store()
            .save(&State {
                provider: DeviceSubscription::Xai,
                generation,
                credentials: None,
            })
            .unwrap();
        let cancel = CancellationToken::new();
        auth.logout(&cancel).await.unwrap();
        assert!(
            auth.finish_device_login(login(generation), &cancel)
                .await
                .is_err()
        );
        assert!(!auth.has_saved_login().unwrap());
    }

    #[tokio::test]
    async fn cancellation_and_untrusted_device_origin_fail_closed() {
        let root = root();
        let (origin, mut captured) = issuer(vec![(200, json!({"device_code":"fixture", "user_code":"code", "verification_uri":"https://attacker.example/consent", "expires_in":300}))]).await;
        let auth = SubscriptionAuth::new(root.path(), DeviceSubscription::Xai)
            .with_loopback_test_issuer(&origin)
            .unwrap();
        let cancel = CancellationToken::new();
        assert!(auth.start_device_login(&cancel).await.is_err());
        let request = captured.recv().await.unwrap();
        assert!(request.starts_with("POST /oauth2/device/code"));
        assert!(request.contains("api%3Aaccess"));
        cancel.cancel();
        assert!(matches!(
            auth.finish_device_login(login(Uuid::new_v4()), &cancel)
                .await,
            Err(ModelError::Cancelled)
        ));
        assert!(!auth.has_saved_login().unwrap());
        assert!(
            SubscriptionAuth::new(root.path(), DeviceSubscription::Kimi)
                .with_loopback_test_issuer("https://attacker.example")
                .is_err()
        );
    }

    #[tokio::test]
    async fn concurrent_refresh_is_single_and_omitted_rotation_preserves_refresh_token() {
        let root = root();
        let (origin, mut captured) = issuer(vec![(
            200,
            json!({"access_token":"renewed", "expires_in":3600}),
        )])
        .await;
        let auth = SubscriptionAuth::new(root.path(), DeviceSubscription::Kimi)
            .with_loopback_test_issuer(&origin)
            .unwrap();
        let identity = Uuid::new_v4();
        auth.store()
            .save(&State {
                provider: DeviceSubscription::Kimi,
                generation: Uuid::new_v4(),
                credentials: Some(Credentials {
                    token: "old".into(),
                    refresh_token: "refresh".into(),
                    expires_at_ms: 1,
                    identity,
                }),
            })
            .unwrap();
        let cancel = CancellationToken::new();
        let (first, second) =
            tokio::join!(auth.resolve(&cancel, None), auth.resolve(&cancel, None));
        assert_eq!(first.unwrap().identity, identity);
        assert_eq!(second.unwrap().identity, identity);
        assert!(captured.recv().await.is_some());
        assert!(captured.recv().await.is_none());
        assert_eq!(
            auth.read()
                .unwrap()
                .unwrap()
                .credentials
                .unwrap()
                .refresh_token,
            "refresh"
        );
    }
    #[tokio::test]
    async fn slowdown_wait_and_expired_grant_stop_without_saving_login() {
        let root = root();
        let (origin, mut captured) = issuer(vec![(400, json!({"error":"slow_down"}))]).await;
        let auth = SubscriptionAuth::new(root.path(), DeviceSubscription::Kimi)
            .with_loopback_test_issuer(&origin)
            .unwrap();
        let generation = Uuid::new_v4();
        auth.store()
            .save(&State {
                provider: DeviceSubscription::Kimi,
                generation,
                credentials: None,
            })
            .unwrap();
        let cancel = CancellationToken::new();
        let worker = auth.clone();
        let task_cancel = cancel.clone();
        let task = tokio::spawn(async move {
            worker
                .finish_device_login(login(generation), &task_cancel)
                .await
        });
        captured.recv().await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        cancel.cancel();
        assert!(matches!(task.await.unwrap(), Err(ModelError::Cancelled)));
        assert!(!auth.has_saved_login().unwrap());
        let mut expired = login(generation);
        expired.deadline = Instant::now();
        assert!(
            auth.finish_device_login(expired, &CancellationToken::new())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn refresh_uses_shared_request_budget_before_sending_credentials() {
        let root = root();
        let auth = SubscriptionAuth::new(root.path(), DeviceSubscription::Kimi);
        auth.store()
            .save(&State {
                provider: DeviceSubscription::Kimi,
                generation: Uuid::new_v4(),
                credentials: Some(Credentials {
                    token: "old".into(),
                    refresh_token: "refresh".into(),
                    expires_at_ms: 1,
                    identity: Uuid::new_v4(),
                }),
            })
            .unwrap();
        let budget = ModelRequestContext::new(0);
        assert!(matches!(
            auth.resolve(&CancellationToken::new(), Some(&budget)).await,
            Err(ModelError::RequestBudgetExhausted {
                stage: crate::model::ModelRequestStage::Credentials
            })
        ));
        assert!(auth.has_saved_login().unwrap());
    }
}

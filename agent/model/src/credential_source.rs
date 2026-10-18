use std::path::{Path, PathBuf};

use async_trait::async_trait;
use reqwest::header::{HeaderName, HeaderValue};
use tickloop_agent_protocol::{ModelAuthSource, ModelConfig};
use tokio_util::sync::CancellationToken;

use crate::{
    ModelError, ModelRequestContext, ModelRequestStage, api_key,
    auth::{
        copilot::CopilotAuth,
        device_subscriptions::{DeviceSubscription, SubscriptionAuth},
        google::GoogleAuth,
        minimax::{MiniMaxAuth, MiniMaxAuthError, MiniMaxRegion},
    },
    copilot, credential_fingerprint,
};

/// The attempt budget one model request shares with its credential refreshes and resends.
#[derive(Clone, Copy)]
pub struct AttemptGate<'a> {
    context: Option<&'a ModelRequestContext>,
}

impl<'a> AttemptGate<'a> {
    /// `None` leaves the request without an attempt budget.
    pub fn new(context: Option<&'a ModelRequestContext>) -> Self {
        Self { context }
    }

    pub fn try_begin_attempt(&self) -> bool {
        self.context
            .is_none_or(ModelRequestContext::try_begin_attempt)
    }

    pub(crate) fn context(&self) -> Option<&'a ModelRequestContext> {
        self.context
    }
}

pub enum Credential {
    /// The service takes no credential, such as a loopback fixture with no API key setting.
    Anonymous,
    Resolved(ResolvedCredential),
}

impl Credential {
    pub fn identity(&self) -> Option<&str> {
        match self {
            Self::Anonymous => None,
            Self::Resolved(credential) => Some(&credential.identity),
        }
    }

    pub(crate) fn secret(&self) -> Option<&HeaderValue> {
        match self {
            Self::Anonymous => None,
            Self::Resolved(credential) => Some(credential.secret()),
        }
    }
}

#[derive(Clone)]
pub struct ResolvedCredential {
    secret: HeaderValue,
    /// Recorded as `PrivateHistorySource.credential_identity`; it stays the same while the
    /// secret rotates, so private history produced under it remains usable.
    pub identity: String,
    /// Replaces the configured service address for this credential, such as the regional
    /// address a sign-in assigns.
    pub base_override: Option<String>,
    pub extra_headers: Vec<(HeaderName, HeaderValue)>,
}

impl ResolvedCredential {
    pub fn new(mut secret: HeaderValue, identity: String) -> Self {
        secret.set_sensitive(true);
        Self {
            secret,
            identity,
            base_override: None,
            extra_headers: Vec::new(),
        }
    }

    /// Always marked sensitive, so it never appears in header debug output.
    pub fn secret(&self) -> &HeaderValue {
        &self.secret
    }
}

pub enum RejectionOutcome {
    /// The client resends once with this credential if the gate allows; it must keep the
    /// rejected credential's identity, because the request already carries history for it.
    Refreshed(ResolvedCredential),
    Final,
}

#[async_trait]
pub trait CredentialSource: Send + Sync {
    /// A refresh that needs its own request first passes `gate`.
    async fn resolve(
        &self,
        config: &ModelConfig,
        gate: AttemptGate<'_>,
        cancel: &CancellationToken,
    ) -> Result<Credential, ModelError>;

    /// The service answered 401 to `credential`.
    async fn rejected(
        &self,
        _config: &ModelConfig,
        _credential: &ResolvedCredential,
        _gate: AttemptGate<'_>,
        _cancel: &CancellationToken,
    ) -> Result<RejectionOutcome, ModelError> {
        Ok(RejectionOutcome::Final)
    }
}

/// Saved keys, provider sign-ins and subscriptions under the Agent state directory, then the
/// process environment for API keys.
pub struct LocalCredentials {
    state_root: Option<PathBuf>,
}

impl LocalCredentials {
    pub fn new(state_root: Option<PathBuf>) -> Self {
        Self { state_root }
    }

    fn state_root(&self, requirement: &str) -> Result<&Path, ModelError> {
        self.state_root
            .as_deref()
            .ok_or_else(|| ModelError::Configuration(requirement.into()))
    }
}

fn credential_header(token: &str, invalid: &str) -> Result<HeaderValue, ModelError> {
    HeaderValue::from_str(token).map_err(|_| ModelError::Configuration(invalid.into()))
}

#[async_trait]
impl CredentialSource for LocalCredentials {
    async fn resolve(
        &self,
        config: &ModelConfig,
        gate: AttemptGate<'_>,
        cancel: &CancellationToken,
    ) -> Result<Credential, ModelError> {
        if let Some(region) = MiniMaxRegion::from_auth(config.auth_source) {
            let root = self.state_root("MiniMax OAuth requires an Agent state directory")?;
            let credentials = MiniMaxAuth::new(root, region)
                .resolve_credentials_with_refresh_gate(cancel, || gate.try_begin_attempt())
                .await
                .map_err(|error| match error {
                    MiniMaxAuthError::Cancelled => ModelError::Cancelled,
                    MiniMaxAuthError::NotLoggedIn => ModelError::MiniMaxNotLoggedIn,
                    MiniMaxAuthError::RefreshFailed => ModelError::MiniMaxAuthentication,
                    MiniMaxAuthError::BudgetExhausted => ModelError::RequestBudgetExhausted {
                        stage: ModelRequestStage::Credentials,
                    },
                    MiniMaxAuthError::Network => ModelError::ConnectionAt {
                        stage: ModelRequestStage::Credentials,
                    },
                    MiniMaxAuthError::Http(status) => {
                        ModelError::Http { status }.at_stage(ModelRequestStage::Credentials)
                    }
                    _ => ModelError::Configuration(error.to_string()),
                })?;
            let secret = credential_header(
                &credentials.access_token,
                "invalid MiniMax credential header",
            )?;
            return Ok(Credential::Resolved(ResolvedCredential::new(
                secret,
                credentials.identity.to_string(),
            )));
        }
        if config.auth_source == ModelAuthSource::GithubCopilot {
            let root =
                self.state_root("GitHub Copilot sign-in requires an Agent state directory")?;
            let access = CopilotAuth::new(root)
                .resolve_access_with_refresh_gate(cancel, || gate.try_begin_attempt())
                .await
                .map_err(copilot::credential_error)?;
            let secret =
                credential_header(&access.token, "invalid GitHub Copilot credential header")?;
            let mut credential = ResolvedCredential::new(secret, access.identity.to_string());
            credential.base_override = access.api_base;
            return Ok(Credential::Resolved(credential));
        }
        if matches!(
            config.auth_source,
            ModelAuthSource::GoogleCloud | ModelAuthSource::GoogleOAuth
        ) {
            let root =
                self.state_root("Google authentication requires an Agent state directory")?;
            let access = GoogleAuth::new(root)
                .resolve(config, cancel, gate.context())
                .await?;
            let secret = credential_header(&access.token, "invalid Google credential header")?;
            let identity = credential_fingerprint(
                format!("{}:{}", access.identity, access.base_url).as_bytes(),
            );
            let project = HeaderValue::from_str(&access.project)
                .map_err(|_| ModelError::Configuration("invalid Google project header".into()))?;
            let mut credential = ResolvedCredential::new(secret, identity);
            credential.base_override = Some(access.base_url);
            credential.extra_headers =
                vec![(HeaderName::from_static("x-goog-user-project"), project)];
            return Ok(Credential::Resolved(credential));
        }
        if let Some(provider) = DeviceSubscription::from_auth(config.auth_source) {
            let root =
                self.state_root("Subscription authentication requires an Agent state directory")?;
            let access = SubscriptionAuth::new(root, provider)
                .resolve(cancel, gate.context())
                .await?;
            let secret =
                credential_header(&access.token, "invalid subscription credential header")?;
            return Ok(Credential::Resolved(ResolvedCredential::new(
                secret,
                access.identity.to_string(),
            )));
        }
        Ok(match api_key(config, self.state_root.as_deref())? {
            Some(key) => {
                let identity = credential_fingerprint(key.as_bytes());
                Credential::Resolved(ResolvedCredential::new(key, identity))
            }
            None => Credential::Anonymous,
        })
    }
}

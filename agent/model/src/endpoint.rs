use std::time::Duration;

use reqwest::{
    Url,
    header::{AUTHORIZATION, HeaderName, HeaderValue, USER_AGENT},
};
use tickloop_agent_base::network::{HttpPolicy, NetworkRoute, build_http_client, is_loopback};
use tickloop_agent_protocol::{Adapter, ModelAuthSource, ModelConfig, Protocol};

use crate::{Credential, ModelError, XAI_GROK_CLIENT_VERSION};

/// Where one request goes and the headers that carry its credential.
pub struct ModelRoute {
    /// The request path for the adapter is appended to this address.
    pub base_url: Url,
    pub headers: Vec<(HeaderName, HeaderValue)>,
}

pub trait ModelEndpoint: Send + Sync {
    /// The HTTP client a model client keeps for all its requests.
    fn http_client(
        &self,
        config: &ModelConfig,
    ) -> Result<(reqwest::Client, NetworkRoute), ModelError>;

    fn route(
        &self,
        config: &ModelConfig,
        credential: &Credential,
    ) -> Result<ModelRoute, ModelError>;
}

fn model_http_client(url: &Url) -> Result<(reqwest::Client, NetworkRoute), ModelError> {
    let mut policy = HttpPolicy::new(reqwest::redirect::Policy::none());
    policy.connect_timeout = Some(Duration::from_secs(15));
    policy.read_timeout = Some(Duration::from_secs(120));
    build_http_client(url, policy).map_err(|_| ModelError::Connection)
}

fn vendor_base<'a>(config: &'a ModelConfig, credential: &'a Credential) -> &'a str {
    match credential {
        Credential::Resolved(credential) => credential.base_override.as_deref(),
        Credential::Anonymous => None,
    }
    .unwrap_or(&config.base_url)
}

fn bearer(secret: &HeaderValue) -> Result<HeaderValue, ModelError> {
    let secret = secret
        .to_str()
        .map_err(|_| ModelError::Configuration("API key must be ASCII".into()))?;
    let mut value = HeaderValue::from_str(&format!("Bearer {secret}"))
        .map_err(|_| ModelError::Configuration("invalid API key header".into()))?;
    value.set_sensitive(true);
    Ok(value)
}

fn invalid_endpoint() -> ModelError {
    ModelError::Configuration("invalid model endpoint".into())
}

/// Sends requests straight to the model vendor named by the configuration.
pub struct VendorEndpoint;

impl ModelEndpoint for VendorEndpoint {
    fn http_client(
        &self,
        config: &ModelConfig,
    ) -> Result<(reqwest::Client, NetworkRoute), ModelError> {
        let url = Url::parse(&config.base_url).map_err(|_| {
            ModelError::Configuration("service address must be an HTTP(S) URL".into())
        })?;
        model_http_client(&url)
    }

    fn route(
        &self,
        config: &ModelConfig,
        credential: &Credential,
    ) -> Result<ModelRoute, ModelError> {
        let base_url =
            Url::parse(vendor_base(config, credential)).map_err(|_| invalid_endpoint())?;
        let mut headers = Vec::new();
        if let Credential::Resolved(resolved) = credential {
            let api_key = config.auth_source == ModelAuthSource::ApiKeyEnv;
            headers.push(match config.protocol {
                Protocol::Google if api_key => (
                    HeaderName::from_static("x-goog-api-key"),
                    resolved.secret().clone(),
                ),
                Protocol::Messages if api_key && config.adapter() != Adapter::MinimaxMessages => (
                    HeaderName::from_static("x-api-key"),
                    resolved.secret().clone(),
                ),
                _ => (AUTHORIZATION, bearer(resolved.secret())?),
            });
            headers.extend(resolved.extra_headers.iter().cloned());
        }
        if config.auth_source == ModelAuthSource::XaiOAuth {
            let model = HeaderValue::from_str(&config.model)
                .map_err(|_| ModelError::Configuration("invalid service model ID".into()))?;
            headers.extend([
                (
                    HeaderName::from_static("x-xai-token-auth"),
                    HeaderValue::from_static("xai-grok-cli"),
                ),
                (
                    HeaderName::from_static("x-grok-client-version"),
                    HeaderValue::from_static(XAI_GROK_CLIENT_VERSION),
                ),
                (
                    USER_AGENT,
                    HeaderValue::from_str(&format!("xai-grok-workspace/{XAI_GROK_CLIENT_VERSION}"))
                        .expect("the xAI client version is a valid header"),
                ),
                (HeaderName::from_static("x-grok-model-override"), model),
            ]);
        }
        Ok(ModelRoute { base_url, headers })
    }
}

/// HTTPS without credentials, query or fragment; HTTP only for loopback.
pub(crate) fn is_permitted_service_url(url: &Url) -> bool {
    (url.scheme() == "https" || url.scheme() == "http" && is_loopback(url))
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
}

/// Sends requests through a model proxy at `{base}/{provider_id}{vendor path}`, keeping the
/// vendor address's path so the proxy can forward each request to the same vendor operation.
/// The credential travels as a Bearer token; the proxy attaches the vendor's own credential
/// and headers.
pub struct ProxyEndpoint {
    base: Url,
}

impl ProxyEndpoint {
    pub fn new(base: Url) -> Result<Self, ModelError> {
        if !is_permitted_service_url(&base) {
            return Err(ModelError::Configuration(
                "the model proxy needs an HTTPS address without credentials, query or fragment; HTTP is allowed only for loopback".into(),
            ));
        }
        Ok(Self { base })
    }
}

impl ModelEndpoint for ProxyEndpoint {
    fn http_client(
        &self,
        _config: &ModelConfig,
    ) -> Result<(reqwest::Client, NetworkRoute), ModelError> {
        model_http_client(&self.base)
    }

    fn route(
        &self,
        config: &ModelConfig,
        credential: &Credential,
    ) -> Result<ModelRoute, ModelError> {
        let provider_id = &config.provider_id;
        if !provider_id
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
            || !provider_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
        {
            return Err(ModelError::Configuration(
                "the model proxy needs a Provider ID".into(),
            ));
        }
        let vendor = Url::parse(vendor_base(config, credential)).map_err(|_| invalid_endpoint())?;
        let base_url = Url::parse(&format!(
            "{}/{provider_id}{}",
            self.base.as_str().trim_end_matches('/'),
            vendor.path().trim_end_matches('/')
        ))
        .map_err(|_| invalid_endpoint())?;
        let mut headers = Vec::new();
        if let Credential::Resolved(resolved) = credential {
            headers.push((AUTHORIZATION, bearer(resolved.secret())?));
            headers.extend(resolved.extra_headers.iter().cloned());
        }
        Ok(ModelRoute { base_url, headers })
    }
}

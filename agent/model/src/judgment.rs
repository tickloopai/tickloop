//! Typed judgments, independent of conversation models and caller decision policy.

use std::{collections::BTreeMap, path::PathBuf, time::Duration};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::{
    ModelError,
    credentials::{CredentialError, api_key_status, load_api_key},
};
use tickloop_agent_base::network::{HttpPolicy, build_http_client, is_loopback};
use tickloop_agent_protocol::{ApiKeyStatus, Usage};

pub const JEV_API_KEY_ENV: &str = "TYPESAFE_API_KEY";
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const PROBABILITY_TOLERANCE: f64 = 0.001;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct JevConfig {
    pub endpoint: String,
    pub model: String,
}

impl Default for JevConfig {
    fn default() -> Self {
        Self {
            endpoint: "https://api.typesafe.ai/v1/systemone".into(),
            model: "jev-1.13.0".into(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum Question {
    Choice {
        instructions: Value,
        criteria: BTreeMap<String, Value>,
    },
    Score {
        instructions: Value,
        criteria: Vec<Value>,
    },
    Noul {
        instructions: Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        criteria: Option<BTreeMap<String, Value>>,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct EvaluationRequest {
    pub state: Value,
    pub questions: BTreeMap<String, Question>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Answer {
    Choice {
        choice: String,
        confidence: f64,
        probabilities: BTreeMap<String, f64>,
    },
    Score {
        score: f64,
        confidence: f64,
        probabilities: BTreeMap<String, f64>,
        legend: BTreeMap<String, Value>,
    },
    Noul {
        noul: f64,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Evaluation {
    pub model: String,
    pub answers: BTreeMap<String, Answer>,
    pub usage: Usage,
}

#[derive(Debug, thiserror::Error)]
#[error("{error}")]
pub struct JudgmentError {
    #[source]
    pub error: ModelError,
    pub usage: Option<Usage>,
}

impl From<ModelError> for JudgmentError {
    fn from(error: ModelError) -> Self {
        Self { error, usage: None }
    }
}

impl EvaluationRequest {
    pub fn validate(&self) -> Result<(), ModelError> {
        let valid_description = |value: &Value| match value {
            Value::String(text) => !text.trim().is_empty(),
            Value::Object(object) => !object.is_empty(),
            Value::Array(array) => !array.is_empty(),
            _ => false,
        };
        if !valid_description(&self.state) || self.questions.is_empty() {
            return Err(ModelError::Configuration(
                "judgment needs state and questions".into(),
            ));
        }
        for (id, question) in &self.questions {
            let valid = !id.trim().is_empty()
                && match question {
                    Question::Choice {
                        instructions,
                        criteria,
                    } => {
                        valid_description(instructions)
                            && (2..=255).contains(&criteria.len())
                            && criteria.iter().all(|(key, value)| {
                                !key.trim().is_empty()
                                    && (value.is_null() || valid_description(value))
                            })
                    }
                    Question::Score {
                        instructions,
                        criteria,
                    } => {
                        valid_description(instructions)
                            && (2..=10).contains(&criteria.len())
                            && criteria.iter().all(valid_description)
                    }
                    Question::Noul {
                        instructions,
                        criteria,
                    } => {
                        valid_description(instructions)
                            && criteria.as_ref().is_none_or(|criteria| {
                                criteria.iter().all(|(key, value)| {
                                    matches!(key.as_str(), "true" | "false")
                                        && valid_description(value)
                                })
                            })
                    }
                };
            if !valid {
                return Err(ModelError::Configuration(
                    "invalid typed judgment question".into(),
                ));
            }
        }
        Ok(())
    }
}

impl Evaluation {
    /// Validate against the exact request before a caller uses any answer.
    pub fn validate(&self, request: &EvaluationRequest) -> Result<(), ModelError> {
        let invalid = || ModelError::InvalidResponse("invalid typed judgment answers".into());
        if self.model.is_empty()
            || self.model.len() > 128
            || !self
                .model
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-._/".contains(&byte))
            || !self.answers.keys().eq(request.questions.keys())
        {
            return Err(invalid());
        }
        let unit_interval = |value: f64| value.is_finite() && (0.0..=1.0).contains(&value);
        let valid_distribution = |probabilities: &BTreeMap<String, f64>| {
            probabilities.values().all(|value| unit_interval(*value))
                && (probabilities.values().sum::<f64>() - 1.0).abs() <= PROBABILITY_TOLERANCE
        };
        for (id, question) in &request.questions {
            let valid = match (question, &self.answers[id]) {
                (
                    Question::Choice { criteria, .. },
                    Answer::Choice {
                        choice,
                        confidence,
                        probabilities,
                    },
                ) => {
                    unit_interval(*confidence)
                        && criteria.keys().eq(probabilities.keys())
                        && valid_distribution(probabilities)
                        && probabilities.get(choice).is_some_and(|selected| {
                            probabilities.values().all(|value| value <= selected)
                        })
                }
                (
                    Question::Score { criteria, .. },
                    Answer::Score {
                        score,
                        confidence,
                        probabilities,
                        legend,
                    },
                ) => {
                    let keys: BTreeMap<_, _> = (0..criteria.len())
                        .map(|index| (index.to_string(), ()))
                        .collect();
                    unit_interval(*confidence)
                        && score.is_finite()
                        && keys.keys().eq(probabilities.keys())
                        && keys.keys().eq(legend.keys())
                        && valid_distribution(probabilities)
                        && (*score
                            - probabilities
                                .iter()
                                .map(|(key, value)| key.parse::<f64>().unwrap_or_default() * value)
                                .sum::<f64>())
                        .abs()
                            <= PROBABILITY_TOLERANCE
                }
                (Question::Noul { .. }, Answer::Noul { noul }) => unit_interval(*noul),
                _ => false,
            };
            if !valid {
                let detail = match &self.answers[id] {
                    Answer::Choice {
                        choice,
                        confidence,
                        probabilities,
                    } => format!(
                        "choice confidence={confidence}, total probability={}, selected probability={:?}, maximum probability={}",
                        probabilities.values().sum::<f64>(),
                        probabilities.get(choice),
                        probabilities.values().copied().fold(0.0_f64, f64::max)
                    ),
                    Answer::Score {
                        score,
                        confidence,
                        probabilities,
                        ..
                    } => format!(
                        "score={score}, confidence={confidence}, total probability={}",
                        probabilities.values().sum::<f64>()
                    ),
                    Answer::Noul { noul } => format!("noul={noul}"),
                };
                return Err(ModelError::InvalidResponse(format!(
                    "invalid typed judgment answer ({detail})"
                )));
            }
        }
        Ok(())
    }
}

#[async_trait]
pub trait JudgmentClient: Send + Sync {
    fn model(&self) -> &str;
    /// One attempt. Callers own retries, confidence thresholds, and decisions.
    async fn evaluate(
        &self,
        request: &EvaluationRequest,
        cancel: CancellationToken,
    ) -> Result<Evaluation, JudgmentError>;
}

pub struct JevClient {
    config: JevConfig,
    credential_root: Option<PathBuf>,
    bound_authorization: Option<reqwest::header::HeaderValue>,
    connection_version: Option<JevConnectionVersion>,
    http: reqwest::Client,
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct JevConnectionVersion([u8; 32]);

impl JevClient {
    pub fn new(config: JevConfig, credential_root: Option<PathBuf>) -> Result<Self, ModelError> {
        let endpoint = reqwest::Url::parse(&config.endpoint)
            .map_err(|_| ModelError::Configuration("invalid Jev endpoint".into()))?;
        if config.model.trim().is_empty()
            || endpoint.host_str().is_none()
            || !(endpoint.scheme() == "https"
                || (endpoint.scheme() == "http" && is_loopback(&endpoint)))
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
        {
            return Err(ModelError::Configuration(
                "Jev requires a model and an HTTPS endpoint without URL credentials".into(),
            ));
        }
        let mut policy = HttpPolicy::new(reqwest::redirect::Policy::none());
        policy.timeout = Some(Duration::from_secs(30));
        policy.connect_timeout = Some(Duration::from_secs(10));
        let (http, _) = build_http_client(&endpoint, policy).map_err(|_| ModelError::Connection)?;
        Ok(Self {
            config,
            credential_root,
            bound_authorization: None,
            connection_version: None,
            http,
        })
    }

    /// Bind the service configuration and credential once for an entire approval.
    pub fn bind(config: JevConfig, credential_root: Option<PathBuf>) -> Result<Self, ModelError> {
        let mut client = Self::new(config, credential_root)?;
        let authorization = authorization_header(client.credential_root.as_deref())?;
        let mut digest = Sha256::new();
        digest.update(client.config.endpoint.as_bytes());
        digest.update([0]);
        digest.update(client.config.model.as_bytes());
        digest.update([0]);
        digest.update(authorization.as_bytes());
        client.connection_version = Some(JevConnectionVersion(digest.finalize().into()));
        client.bound_authorization = Some(authorization);
        Ok(client)
    }

    pub fn connection_version(&self) -> Option<JevConnectionVersion> {
        self.connection_version
    }

    async fn request(&self, request: &EvaluationRequest) -> Result<Evaluation, ModelError> {
        request.validate()?;
        let authorization = match &self.bound_authorization {
            Some(authorization) => authorization.clone(),
            None => authorization_header(self.credential_root.as_deref())?,
        };
        let body = serde_json::json!({"model": self.config.model, "state": request.state, "questions": request.questions});
        let mut response = self
            .http
            .post(&self.config.endpoint)
            .header(reqwest::header::AUTHORIZATION, authorization)
            .json(&body)
            .send()
            .await
            .map_err(http_error)?;
        let status = response.status();
        if !status.is_success() {
            return Err(match status.as_u16() {
                401 => ModelError::Authentication,
                413 => ModelError::ResponseRejected(crate::ModelResponseRejection::ContextLimit),
                status => ModelError::HttpResponse {
                    status,
                    description: String::new(),
                    retry_after: response
                        .headers()
                        .get(reqwest::header::RETRY_AFTER)
                        .and_then(|value| value.to_str().ok())
                        .and_then(|value| value.parse::<u64>().ok())
                        .map(Duration::from_secs),
                    should_retry: None,
                },
            });
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(http_error)? {
            if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                return Err(ModelError::InvalidResponse(
                    "Jev response exceeds size limit".into(),
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        // Keep reported usage even if a typed answer fails validation at the caller.
        serde_json::from_slice(&bytes)
            .map_err(|_| ModelError::InvalidResponse("invalid Jev response".into()))
    }
}

/// Jev key status from the credentials under `root`, or from the environment without one.
pub fn jev_key_status_in(root: Option<&std::path::Path>) -> Result<ApiKeyStatus, CredentialError> {
    if let Some(root) = root {
        return api_key_status(root, JEV_API_KEY_ENV);
    }
    Ok(std::env::var(JEV_API_KEY_ENV)
        .ok()
        .filter(|value| {
            !value.trim().is_empty()
                && reqwest::header::HeaderValue::from_str(&format!("Bearer {value}")).is_ok()
        })
        .map(|_| ApiKeyStatus::Environment)
        .unwrap_or(ApiKeyStatus::Missing))
}

fn authorization_header(
    root: Option<&std::path::Path>,
) -> Result<reqwest::header::HeaderValue, ModelError> {
    let saved_key = root
        .map(|root| load_api_key(root, JEV_API_KEY_ENV))
        .transpose()
        .map_err(|error| ModelError::Configuration(error.to_string()))?
        .flatten();
    let key = saved_key
        .or_else(|| std::env::var(JEV_API_KEY_ENV).ok())
        .filter(|key| !key.trim().is_empty())
        .ok_or_else(|| {
            ModelError::Configuration(
                "Jev key missing; open /permissions → Auto mode or set TYPESAFE_API_KEY".into(),
            )
        })?;
    let mut authorization = reqwest::header::HeaderValue::from_str(&format!("Bearer {key}"))
        .map_err(|_| ModelError::Configuration("invalid Jev API key".into()))?;
    authorization.set_sensitive(true);
    Ok(authorization)
}

#[async_trait]
impl JudgmentClient for JevClient {
    fn model(&self) -> &str {
        &self.config.model
    }

    async fn evaluate(
        &self,
        request: &EvaluationRequest,
        cancel: CancellationToken,
    ) -> Result<Evaluation, JudgmentError> {
        let evaluation = tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(ModelError::Cancelled),
            response = self.request(request) => response,
        }?;
        evaluation
            .validate(request)
            .map_err(|error| JudgmentError {
                error,
                usage: Some(evaluation.usage.clone()),
            })?;
        Ok(evaluation)
    }
}

fn http_error(error: reqwest::Error) -> ModelError {
    if error.is_timeout() {
        ModelError::TimedOut
    } else {
        ModelError::Connection
    }
}

use std::{fmt, time::Duration};

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ProviderFailureClass {
    UsageLimit,
    UsageOrPlan,
    Billing,
    Authentication,
    RateLimit,
    ModelUnavailable,
    ServerError,
    Refused,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProviderHttpFailure {
    pub provider_id: String,
    pub provider_name: String,
    pub status: u16,
    pub class: ProviderFailureClass,
    pub message: String,
    pub hint: String,
    pub retry_after: Option<Duration>,
    pub retryable: bool,
}

impl ProviderHttpFailure {
    pub fn summary(&self) -> String {
        let label = match self.class {
            ProviderFailureClass::UsageLimit => "usage limit",
            ProviderFailureClass::UsageOrPlan => "usage / plan limit",
            ProviderFailureClass::Billing => "billing",
            ProviderFailureClass::Authentication => "credentials invalid",
            ProviderFailureClass::RateLimit => "rate limited",
            ProviderFailureClass::ModelUnavailable => "model / plan access",
            ProviderFailureClass::ServerError => "server error",
            ProviderFailureClass::Refused => "request refused",
        };
        format!(
            "{} · HTTP {} · {label} · /provider",
            self.provider_name, self.status
        )
    }
}

impl fmt::Display for ProviderHttpFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} · HTTP {}: {}\n{}",
            self.provider_name, self.status, self.message, self.hint
        )
    }
}

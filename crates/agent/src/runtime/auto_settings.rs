use std::path::Path;

use crate::judgment::JEV_API_KEY_ENV;
use crate::model::credentials::{ApiKeyStatus, api_key_status, save_api_key};
use crate::settings::{AutoSettings, AutoSettingsRevision};

use super::{Agent, AgentError};

/// Jev key status from the credentials under `root`, or from the environment without one.
pub(crate) fn jev_key_status_in(root: Option<&Path>) -> Result<ApiKeyStatus, AgentError> {
    if let Some(root) = root {
        return api_key_status(root, JEV_API_KEY_ENV)
            .map_err(|error| AgentError::Storage(error.to_string()));
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

impl Agent {
    pub(super) async fn auto_settings_snapshot(
        &self,
    ) -> Result<(AutoSettings, AutoSettingsRevision), AgentError> {
        let Some(settings) = self.inner.settings.clone() else {
            return Ok((AutoSettings::default(), AutoSettingsRevision::default()));
        };
        tokio::task::spawn_blocking(move || settings.auto_settings_with_revision())
            .await
            .map_err(|error| AgentError::Storage(error.to_string()))?
            .map_err(|error| AgentError::Storage(error.to_string()))
    }

    pub async fn jev_key_status(&self) -> Result<ApiKeyStatus, AgentError> {
        let root = self
            .inner
            .settings
            .as_ref()
            .and_then(crate::settings::UserSettings::root)
            .map(ToOwned::to_owned);
        tokio::task::spawn_blocking(move || jev_key_status_in(root.as_deref()))
            .await
            .map_err(|error| AgentError::Storage(error.to_string()))?
    }

    pub async fn save_jev_key(&self, value: &str) -> Result<ApiKeyStatus, AgentError> {
        let root = self
            .inner
            .settings
            .as_ref()
            .and_then(crate::settings::UserSettings::root)
            .ok_or_else(|| AgentError::Invalid("Jev key storage is unavailable".into()))?
            .to_owned();
        let value = value.to_owned();
        tokio::task::spawn_blocking(move || save_api_key(&root, JEV_API_KEY_ENV, &value))
            .await
            .map_err(|error| AgentError::Storage(error.to_string()))?
            .map_err(|error| AgentError::Storage(error.to_string()))?;
        self.inner.auto_health.lock().await.reset();
        Ok(ApiKeyStatus::Saved)
    }
}

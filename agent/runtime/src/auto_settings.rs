use tickloop_agent_model::credentials::save_api_key;
use tickloop_agent_model::judgment::{JEV_API_KEY_ENV, jev_key_status_in};
use tickloop_agent_policy::{AutoSettings, settings::empty_auto_settings_revision};

use super::{Agent, AgentError};
use tickloop_agent_protocol::{ApiKeyStatus, AutoSettingsRevision};

impl Agent {
    pub(super) async fn auto_settings_snapshot(
        &self,
    ) -> Result<(AutoSettings, AutoSettingsRevision), AgentError> {
        let Some(settings) = self.inner.settings.clone() else {
            return Ok((AutoSettings::default(), empty_auto_settings_revision()));
        };
        tokio::task::spawn_blocking(move || settings.read(None).auto)
            .await
            .map_err(|error| AgentError::Storage(error.to_string()))?
            .map_err(|error| AgentError::Storage(error.to_string()))
    }

    pub async fn jev_key_status(&self) -> Result<ApiKeyStatus, AgentError> {
        let root = self.inner.config_root.as_deref().map(ToOwned::to_owned);
        tokio::task::spawn_blocking(move || jev_key_status_in(root.as_deref()))
            .await
            .map_err(|error| AgentError::Storage(error.to_string()))?
            .map_err(|error| AgentError::Storage(error.to_string()))
    }

    pub async fn save_jev_key(&self, value: &str) -> Result<ApiKeyStatus, AgentError> {
        let root = self
            .inner
            .config_root
            .as_deref()
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

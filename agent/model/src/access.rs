//! Current access catalog: for each Provider it declares the stable model series offered in
//! ordinary selection and the concrete Model Card each series currently recommends. Cards stay
//! in the definition catalog after a series moves on, so saved selections and history keep
//! resolving to the exact model they used.

use std::{collections::HashSet, sync::OnceLock};

use serde::{Deserialize, Serialize};

use super::cards::{ModelCatalog, valid_provider_id};
use tickloop_agent_protocol::ModelConfig;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderAccess {
    pub provider: String,
    /// Legacy grouping metadata; when present it must match the connection vendor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// Plan or billing condition shown before choosing a model of this Provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<String>,
    pub series: Vec<SeriesBinding>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SeriesBinding {
    /// Stable series identity, kept when the recommended card changes.
    pub id: String,
    pub name: String,
    /// Version of the recommended card as shown next to the series name.
    pub version: String,
    /// Model Card `id` in the same Provider.
    pub model: String,
    /// Keeps model families adjacent before ordering their capability tiers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family_order: Option<u16>,
    /// Maintained capability order for selection; lower values appear first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability_order: Option<u16>,
}

impl SeriesBinding {
    pub fn label(&self) -> String {
        format!("{} ({})", self.name, self.version)
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AccessCatalog {
    providers: Vec<ProviderAccess>,
}

impl AccessCatalog {
    pub fn from_yaml(files: &[&str]) -> Result<Self, String> {
        let providers = files
            .iter()
            .map(|file| {
                serde_yaml_ng::from_str(file)
                    .map_err(|error| format!("invalid access catalog YAML: {error}"))
            })
            .collect::<Result<_, _>>()?;
        Ok(Self { providers })
    }

    pub fn defaults() -> &'static Self {
        static DEFAULTS: OnceLock<AccessCatalog> = OnceLock::new();
        DEFAULTS.get_or_init(|| {
            let catalog = Self::from_yaml(&[
                include_str!("access/deepseek-api.yaml"),
                include_str!("access/openai-api.yaml"),
                include_str!("access/openai-codex.yaml"),
                include_str!("access/minimax-cn-api.yaml"),
                include_str!("access/minimax-global-api.yaml"),
                include_str!("access/minimax-cn-oauth.yaml"),
                include_str!("access/minimax-global-oauth.yaml"),
                include_str!("access/zhipu-cn-api.yaml"),
                include_str!("access/zhipu-cn-coding.yaml"),
                include_str!("access/zai-global-api.yaml"),
                include_str!("access/zai-global-coding.yaml"),
                include_str!("access/github-copilot.yaml"),
                include_str!("access/anthropic-api.yaml"),
                include_str!("access/kimi-cn-api.yaml"),
                include_str!("access/kimi-global-api.yaml"),
                include_str!("access/kimi-code-oauth.yaml"),
                include_str!("access/alibaba-cn-api.yaml"),
                include_str!("access/google-gemini-api.yaml"),
                include_str!("access/google-gemini-oauth.yaml"),
                include_str!("access/google-vertex-service-account.yaml"),
                include_str!("access/google-vertex-oauth.yaml"),
                include_str!("access/xai-api.yaml"),
                include_str!("access/xai-oauth.yaml"),
            ])
            .expect("bundled access catalog YAML is valid");
            catalog
                .validate(&ModelCatalog::defaults())
                .expect("bundled access catalog references valid Model Cards");
            catalog
        })
    }

    /// Every binding must resolve to a valid Model Card of the same Provider, so a series can
    /// never point at a missing card, another connection or an endpoint-less adapter.
    pub fn validate(&self, models: &ModelCatalog) -> Result<(), String> {
        let mut providers = HashSet::new();
        for access in &self.providers {
            let location = format!("access[{}]", access.provider);
            if !valid_provider_id(&access.provider) || !providers.insert(&access.provider) {
                return Err(format!("{location}: invalid or duplicate provider"));
            }
            if [&access.group, &access.notice]
                .into_iter()
                .any(|text| text.as_ref().is_some_and(|text| text.trim().is_empty()))
            {
                return Err(format!("{location}: group and notice must not be empty"));
            }
            if let Some(provider) = models
                .providers
                .iter()
                .find(|provider| provider.id == access.provider)
                && let Some(connection) = &provider.connection
                && access
                    .group
                    .as_deref()
                    .is_some_and(|group| group != connection.vendor)
            {
                return Err(format!(
                    "{location}: legacy group must match connection vendor"
                ));
            }
            if access.series.is_empty() {
                return Err(format!(
                    "{location}.series: at least one series is required"
                ));
            }
            let mut series_ids = HashSet::new();
            let mut series_names = HashSet::new();
            let mut bound_models = HashSet::new();
            for series in &access.series {
                let location = format!("{location}.series[{}]", series.id);
                if !valid_provider_id(&series.id)
                    || series.name.trim().is_empty()
                    || series.version.trim().is_empty()
                {
                    return Err(format!(
                        "{location}: id must be a safe lowercase slug; name and version are required"
                    ));
                }
                if !series_ids.insert(&series.id) || !series_names.insert(&series.name) {
                    return Err(format!("{location}: duplicate series"));
                }
                if !bound_models.insert(
                    models
                        .canonical_model_id(&access.provider, &series.model)
                        .unwrap_or(&series.model),
                ) {
                    return Err(format!(
                        "{location}.model: `{}` is already bound to another series",
                        series.model
                    ));
                }
                if models.get(&access.provider, &series.model).is_none() {
                    return Err(format!(
                        "{location}.model: no valid Model Card `{}` in this Provider",
                        series.model
                    ));
                }
            }
        }
        Ok(())
    }

    pub fn provider(&self, provider_id: &str) -> Option<&ProviderAccess> {
        self.providers
            .iter()
            .find(|access| access.provider == provider_id)
    }

    pub fn series_for(&self, profile: &ModelConfig) -> Option<&SeriesBinding> {
        self.provider(&profile.provider_id)?
            .series
            .iter()
            .find(|series| {
                super::cards::builtin_canonical_model_id(&profile.provider_id, &series.model)
                    .unwrap_or(&series.model)
                    == super::cards::builtin_canonical_model_id(
                        &profile.provider_id,
                        &profile.model_id,
                    )
                    .unwrap_or(&profile.model_id)
            })
    }

    /// The first series is the Provider's default recommendation.
    pub fn recommended_model(&self, provider_id: &str) -> Option<&str> {
        self.provider(provider_id)?
            .series
            .first()
            .map(|series| series.model.as_str())
    }

    /// For Providers governed by this catalog, ordinary selection offers bound cards and cards
    /// the user added under new ids; retired built-in cards stay loadable but are not offered,
    /// even after a user override. `None` leaves other Providers to their existing rule.
    pub fn ordinary_candidate(
        &self,
        profile: &ModelConfig,
        defaults: &ModelCatalog,
    ) -> Option<bool> {
        self.provider(&profile.provider_id)?;
        Some(
            self.series_for(profile).is_some()
                || defaults
                    .get(&profile.provider_id, &profile.model_id)
                    .is_none(),
        )
    }

    pub fn capability_order(&self, profile: &ModelConfig) -> Option<u16> {
        self.series_for(profile)?.capability_order
    }

    pub fn series_position(&self, profile: &ModelConfig) -> Option<usize> {
        self.provider(&profile.provider_id)?
            .series
            .iter()
            .position(|series| {
                super::cards::builtin_canonical_model_id(&profile.provider_id, &series.model)
                    .unwrap_or(&series.model)
                    == super::cards::builtin_canonical_model_id(
                        &profile.provider_id,
                        &profile.model_id,
                    )
                    .unwrap_or(&profile.model_id)
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_bindings_resolve_to_concrete_cards() {
        let catalog = AccessCatalog::defaults();
        let models = ModelCatalog::defaults();
        for provider in ["zhipu-cn-api", "zai-global-api"] {
            let access = catalog.provider(provider).unwrap();
            assert!(access.group.is_none());
            assert_eq!(
                super::super::cards::provider_group(provider),
                Some("Zhipu GLM")
            );
            let resolved = access
                .series
                .iter()
                .map(|series| {
                    let card = models.get(provider, &series.model).unwrap();
                    (series.label(), card.name)
                })
                .collect::<Vec<_>>();
            assert_eq!(
                resolved,
                [
                    ("GLM (5.3)".to_string(), "GLM-5.3".to_string()),
                    ("GLM Flash (5.3)".to_string(), "GLM-5.3-Flash".to_string()),
                    ("GLM FlashX (5.3)".to_string(), "GLM-5.3-FlashX".to_string()),
                ]
            );
            assert_eq!(catalog.recommended_model(provider), Some("glm-5.3"));
        }
        assert!(catalog.provider("deepseek-api").is_some());
    }
    #[test]
    fn legacy_primary_alias_bindings_share_the_canonical_series_and_reject_duplicates() {
        let models = ModelCatalog::defaults();
        let catalog = AccessCatalog::from_yaml(&["provider: openai-api\nseries:\n  - {id: sol, name: Sol, version: '6.1', model: gpt-6.1-sol-responses}\n"]).unwrap();
        catalog.validate(&models).unwrap();
        let profile = models.get("openai-api", "gpt-6.1-sol").unwrap();
        assert_eq!(catalog.series_for(&profile).unwrap().id, "sol");
        assert_eq!(catalog.series_position(&profile), Some(0));
        let duplicate = AccessCatalog::from_yaml(&["provider: openai-api\nseries:\n  - {id: sol, name: Sol, version: '6.1', model: gpt-6.1-sol-responses}\n  - {id: other, name: Other, version: '6.1', model: gpt-6.1-sol}\n"]).unwrap();
        assert!(duplicate.validate(&models).is_err());
    }
}

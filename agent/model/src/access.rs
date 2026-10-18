//! Selection policies projected from the bundled Provider definitions. Series recommend concrete
//! cards for ordinary selection; older cards remain available to fixed selections and history.

use std::{collections::HashSet, sync::OnceLock};

use serde::{Deserialize, Serialize};

use super::cards::{ModelCatalog, valid_provider_id};
use tickloop_agent_protocol::ModelConfig;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderSelection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auxiliary_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<String>,
    pub series: Vec<SeriesBinding>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProviderAccess {
    pub provider: String,
    pub auxiliary_model: Option<String>,
    pub notice: Option<String>,
    pub series: Vec<SeriesBinding>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SeriesBinding {
    /// Stable series identity, kept when the recommended card changes.
    pub id: String,
    pub name: String,
    /// Model Card `id` in the same Provider.
    pub model: String,
    /// Keeps model families adjacent before ordering their capability tiers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family_order: Option<u16>,
    /// Maintained capability order for selection; lower values appear first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability_order: Option<u16>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AccessCatalog {
    providers: Vec<ProviderAccess>,
}

impl AccessCatalog {
    pub fn from_catalog(models: &ModelCatalog) -> Self {
        Self {
            providers: models
                .providers
                .iter()
                .filter_map(|provider| {
                    let selection = provider.selection.as_ref()?;
                    Some(ProviderAccess {
                        provider: provider.id.clone(),
                        auxiliary_model: selection.auxiliary_model.clone(),
                        notice: selection.notice.clone(),
                        series: selection.series.clone(),
                    })
                })
                .collect(),
        }
    }

    pub fn defaults() -> &'static Self {
        static DEFAULTS: OnceLock<AccessCatalog> = OnceLock::new();
        DEFAULTS.get_or_init(|| {
            let models = ModelCatalog::defaults();
            let catalog = Self::from_catalog(&models);
            catalog
                .validate(&models)
                .expect("bundled Provider selections reference valid Model Cards");
            catalog
        })
    }

    /// Every binding must resolve to a valid Model Card of the same Provider, so a series can
    /// never point at a missing card, another connection or an endpoint-less adapter.
    pub fn validate(&self, models: &ModelCatalog) -> Result<(), String> {
        let mut providers = HashSet::new();
        for access in &self.providers {
            let location = format!("providers[{}].selection", access.provider);
            if !valid_provider_id(&access.provider) || !providers.insert(&access.provider) {
                return Err(format!("{location}: invalid or duplicate provider"));
            }
            if access
                .notice
                .as_ref()
                .is_some_and(|notice| notice.trim().is_empty())
            {
                return Err(format!("{location}: notice must not be empty"));
            }
            if access.series.is_empty() {
                return Err(format!(
                    "{location}.series: at least one series is required"
                ));
            }
            if let Some(model) = &access.auxiliary_model
                && models.get(&access.provider, model).is_none()
            {
                return Err(format!(
                    "{location}.auxiliary_model: no valid Model Card `{model}` in this Provider"
                ));
            }
            let mut series_ids = HashSet::new();
            let mut series_names = HashSet::new();
            let mut bound_models = HashSet::new();
            for series in &access.series {
                let location = format!("{location}.series[{}]", series.id);
                if !valid_provider_id(&series.id) || series.name.trim().is_empty() {
                    return Err(format!(
                        "{location}: id must be a safe lowercase slug; name is required"
                    ));
                }
                if !series_ids.insert(&series.id) || !series_names.insert(&series.name) {
                    return Err(format!("{location}: duplicate series"));
                }
                if !bound_models.insert(&series.model) {
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
            .find(|series| series.model == profile.model_id)
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
            .position(|series| series.model == profile.model_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_bindings_resolve_to_concrete_cards() {
        let catalog = AccessCatalog::defaults();
        let models = ModelCatalog::defaults();
        models.validate().unwrap();
        assert_eq!(catalog.providers.len(), models.providers.len());
        assert!(
            catalog
                .providers
                .iter()
                .all(|provider| provider.auxiliary_model.is_some())
        );
        for provider in ["zhipu-cn-api", "zai-global-api"] {
            let access = catalog.provider(provider).unwrap();
            assert_eq!(
                super::super::cards::provider_group(provider),
                Some("Zhipu GLM")
            );
            let resolved = access
                .series
                .iter()
                .map(|series| {
                    let card = models.get(provider, &series.model).unwrap();
                    (series.name.clone(), card.name)
                })
                .collect::<Vec<_>>();
            assert_eq!(
                resolved,
                [
                    ("GLM".to_string(), "GLM-5.3".to_string()),
                    ("GLM Flash".to_string(), "GLM-5.3-Flash".to_string()),
                    ("GLM FlashX".to_string(), "GLM-5.3-FlashX".to_string()),
                    ("GLM Air".to_string(), "GLM-4.5-Air".to_string()),
                    ("GLM Flash 30B".to_string(), "GLM-4.7-Flash".to_string()),
                ]
            );
            assert_eq!(catalog.recommended_model(provider), Some("glm-5.3"));
        }
        assert!(catalog.provider("deepseek-api").is_some());
    }
}

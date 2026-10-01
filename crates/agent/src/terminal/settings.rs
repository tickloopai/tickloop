#[cfg(test)]
use tickloop_agent::model::cards::upgrade_saved_legacy_builtin_model;
use tickloop_agent::{
    model::{ModelConfig, Protocol, cards::built_in_model_profiles},
    permissions::ExecutionMode,
};

#[cfg(test)]
const LEGACY_DEEPSEEK_MAX_OUTPUT_TOKENS: u32 = 4096;

#[cfg(test)]
pub(super) fn upgrade_legacy_builtin_model(mut config: ModelConfig) -> ModelConfig {
    config = upgrade_saved_legacy_builtin_model(config);
    config
}

#[cfg(test)]
pub(super) fn model_choices(current: &ModelConfig) -> Vec<ModelConfig> {
    model_choices_with_profiles(current, built_in_model_profiles())
}

pub(super) fn model_choices_with_profiles(
    current: &ModelConfig,
    profiles: Vec<ModelConfig>,
) -> Vec<ModelConfig> {
    let defaults = tickloop_agent::model::cards::ModelCatalog::defaults();
    if tickloop_agent::model::access::AccessCatalog::defaults()
        .ordinary_candidate(current, &defaults)
        == Some(false)
    {
        return profiles;
    }
    let mut choices = profiles;
    if let Some(index) = choices.iter().position(|profile| {
        profile.provider_id == current.provider_id && profile.model_id == current.model_id
    }) {
        choices[index] = current.clone();
    } else {
        choices.push(current.clone());
    }
    choices
}

pub(super) fn model_label(config: &ModelConfig) -> String {
    let protocol = match config.protocol {
        Protocol::Messages => "Messages",
        Protocol::Responses => "Responses",
        Protocol::ChatCompletions => "OpenAI Chat Completions",
        Protocol::Google => "Google API",
        Protocol::SystemOne => "System One",
    };
    if !config.provider_id.is_empty() {
        let provider = if config.provider_name.is_empty() {
            &config.provider_id
        } else {
            &config.provider_name
        };
        return format!(
            "{provider} · {} ({}) · {protocol}",
            config.name, config.model
        );
    }
    let label = format!("{} · {protocol}", config.model);
    if built_in_model_profiles().contains(config) {
        label
    } else if config.name.is_empty() || config.name == config.model {
        format!("{label} · custom profile")
    } else {
        format!("{label} · {} (custom profile)", config.name)
    }
}

pub(super) fn execution_modes() -> Vec<(ExecutionMode, String)> {
    [
        (
            ExecutionMode::Auto,
            "Auto · Use LLM or Judgment review; ask when uncertain",
        ),
        (
            ExecutionMode::AcceptEdits,
            "Ask · Allow workspace edits; ask before protected actions",
        ),
        (
            ExecutionMode::FullAccess,
            "Full Access · Run ordinary actions without approval prompts",
        ),
    ]
    .into_iter()
    .map(|(mode, label)| (mode, label.to_owned()))
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preferred_profiles_keep_visible_current_and_hide_legacy_adapter_duplicates() {
        for source in [
            include_str!("../../examples/deepseek-config.json"),
            include_str!("../../examples/deepseek-responses-config.json"),
            include_str!("../../examples/deepseek-chat-config.json"),
            include_str!("../../examples/deepseek-v4-pro-config.json"),
        ] {
            let current: ModelConfig = tickloop_agent::model::cards::migrate_legacy_model(
                serde_json::from_str(source).unwrap(),
            );
            let choices = model_choices(&current);
            assert_eq!(choices.len(), built_in_model_profiles().len());
            if tickloop_agent::model::cards::is_default_visible(&current) {
                assert!(choices.contains(&current));
                assert_eq!(choices.iter().filter(|value| **value == current).count(), 1);
            } else {
                assert_eq!(choices, built_in_model_profiles());
                assert!(!choices.contains(&current));
            }
            assert!(
                choices
                    .iter()
                    .filter(|model| model.provider_id == "deepseek-api")
                    .all(|model| model.adapter()
                        == tickloop_agent::model::cards::Adapter::DeepseekChatCompletions)
            );
        }
    }

    #[test]
    fn default_deepseek_chat_completions_profile_matches_the_example_and_accepts_images() {
        let mut example = tickloop_agent::model::cards::migrate_legacy_model(
            serde_json::from_str(include_str!("../../examples/deepseek-chat-config.json")).unwrap(),
        );
        let default = tickloop_agent::model::cards::ModelCatalog::defaults()
            .get("deepseek-api", "deepseek-flash-chat-completions")
            .unwrap();
        assert_eq!(default.connection(), example.connection());
        example.connection = default.connection.clone();
        assert_eq!(default, example);
        assert!(default.accepts_images());
    }

    #[test]
    fn current_model_keeps_its_position_and_runtime_overrides() {
        let profiles = built_in_model_profiles();
        let mut current = profiles.last().unwrap().clone();
        current.max_output_tokens += 1;
        let choices = model_choices_with_profiles(&current, profiles.clone());
        assert_eq!(choices.len(), profiles.len());
        assert_eq!(choices[..choices.len() - 1], profiles[..profiles.len() - 1]);
        assert_eq!(choices.last(), Some(&current));
    }

    #[test]
    fn custom_profile_preserves_endpoint_credentials_and_limits_without_relabeling() {
        let mut current = built_in_model_profiles().remove(0);
        current.base_url = "https://user:secret@gateway.example/anthropic?token=secret".into();
        current.api_key_env = "PRIVATE_GATEWAY_TOKEN".into();
        current.max_output_tokens = 2048;
        current.provider_id.clear();
        current.model_id.clear();
        current.card = None;
        let choices = model_choices(&current);
        assert_eq!(choices.len(), built_in_model_profiles().len() + 1);
        assert!(choices.contains(&current));
        assert_eq!(choices[..choices.len() - 1], built_in_model_profiles());
        let label = model_label(&current);
        assert!(label.contains("custom profile"));
        assert!(label.contains("deepseek-flash"));
        assert!(label.contains("OpenAI Chat Completions"));
        assert!(!label.contains("secret"));
        assert!(!label.contains("PRIVATE_GATEWAY_TOKEN"));
        assert_ne!(label, model_label(&choices[1]));
    }

    #[test]
    fn upgrades_only_saved_legacy_builtin_profiles() {
        for mut legacy in built_in_model_profiles() {
            if legacy.provider_id != "deepseek-api" {
                continue;
            }
            legacy.provider_id.clear();
            legacy.model_id.clear();
            legacy.card = None;
            legacy.max_output_tokens = LEGACY_DEEPSEEK_MAX_OUTPUT_TOKENS;
            let upgraded = upgrade_legacy_builtin_model(legacy.clone());
            assert_eq!(upgraded.max_output_tokens, 64_000);

            legacy.name.push_str(" custom");
            let custom = upgrade_legacy_builtin_model(legacy.clone());
            assert_eq!(custom.name, legacy.name);
            assert_eq!(custom.model, legacy.model);
            assert_eq!(custom.base_url, legacy.base_url);
            assert_eq!(custom.api_key_env, legacy.api_key_env);
            assert_eq!(custom.max_output_tokens, legacy.max_output_tokens);
            assert_eq!(custom.protocol, legacy.protocol);
        }
    }

    #[test]
    fn model_label_uses_configured_provider_display_name() {
        let mut model = built_in_model_profiles().remove(0);
        model.provider_id = "private-gateway".into();
        model.provider_name = "Research Gateway".into();
        let label = model_label(&model);
        assert!(label.starts_with("Research Gateway ·"));
        assert!(label.contains("deepseek-flash"));
    }
}

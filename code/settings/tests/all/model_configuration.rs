use std::collections::BTreeMap;

use serde_json::json;
use tickloop_code_settings::UserSettings;
use tickloop_harness::model::{
    ModelConfigExt,
    cards::{
        ModelCatalog, ModelSelection, ProviderDefinition, migrate_legacy_model,
        model_for_persistent_selection,
    },
};
use tickloop_harness::protocol::{
    Adapter, ModelAuthSource, ModelCard, ModelConfig, ProviderPatch, ThinkingControl,
};

fn custom_provider(id: &str) -> ProviderDefinition {
    ProviderDefinition {
        selection: None,
        connection: None,
        id: id.into(),
        display_name: format!("{id} gateway"),
        auth: ModelAuthSource::ApiKeyEnv,
        api_key_env: "GATEWAY_API_KEY".into(),
        endpoints: BTreeMap::from([(Adapter::Responses, "http://127.0.0.1:3001".into())]),
        prompt_cache: None,
        context_token_ratios: Default::default(),
        models: vec![ModelCard {
            id: "assistant".into(),
            display_name: "Gateway Assistant".into(),
            adapter: Adapter::Responses,
            max_output_tokens: 4096,
            context_window_tokens: Some(32_000),
            thinking: ThinkingControl::Toggle,
            adaptive_by_default: false,
            effort_levels: vec![tickloop_harness::protocol::ReasoningEffort::High],
            input_modalities: vec![tickloop_harness::protocol::InputModality::Text],
            thinking_budgets: None,
            temperature_control: None,
            thinking_off: None,
            native_tool_search: None,
        }],
    }
}

fn custom_patch(id: &str) -> ProviderPatch {
    serde_json::from_value(serde_json::to_value(custom_provider(id)).unwrap()).unwrap()
}

#[test]
fn unknown_legacy_api_model_gets_a_stable_card_without_invented_capabilities() {
    let legacy: ModelConfig = serde_json::from_value(json!({
        "name": "Research model",
        "protocol": "messages",
        "base_url": "https://gateway.example/messages",
        "model": "research-v2",
        "api_key_env": "RESEARCH_KEY",
        "max_output_tokens": 2048
    }))
    .unwrap();
    let selected = model_for_persistent_selection(legacy.clone());
    assert_eq!(selected, model_for_persistent_selection(selected.clone()));
    assert_eq!(selected, model_for_persistent_selection(legacy));
    assert!(selected.provider_id.starts_with("legacy-api-"));
    assert_eq!(selected.base_url, "https://gateway.example/messages");
    assert_eq!(selected.api_key_env, "RESEARCH_KEY");
    assert_eq!(selected.max_output_tokens, 2048);
    let card = selected.card.unwrap();
    assert_eq!(card.adapter, Adapter::Messages);
    assert_eq!(card.context_window_tokens, None);
    assert_eq!(card.thinking, ThinkingControl::Unsupported);
    assert!(card.effort_levels.is_empty());
}

#[test]
fn credentialless_api_provider_requires_only_loopback_endpoints() {
    let mut local = custom_provider("local-gateway");
    local.api_key_env.clear();
    ModelCatalog::from_providers(vec![local.clone()]).unwrap();
    local.endpoints.insert(
        Adapter::Responses,
        "https://gateway.example/responses".into(),
    );
    let error = ModelCatalog::from_providers(vec![local])
        .unwrap_err()
        .to_string();
    assert!(error.contains("api_key_env"), "{error}");
}

#[test]
fn builtin_deepseek_models_accept_images_only_where_verified() {
    let catalog = ModelCatalog::defaults();
    for (model_id, accepts_images) in [
        ("deepseek-flash", true),
        ("deepseek-v4-pro", false),
        ("deepseek-v4-flash", false),
    ] {
        let model = catalog.get("deepseek-api", model_id).unwrap();
        assert_eq!(model.accepts_images(), accepts_images, "{model_id}");
    }
}

#[test]
fn card_id_is_the_service_identity_on_each_connection() {
    let catalog = ModelCatalog::defaults();
    catalog.validate().unwrap();
    let model = catalog.get("deepseek-api", "deepseek-flash").unwrap();
    assert_eq!(model.model_id, model.model);
    assert_eq!(
        model.protocol,
        tickloop_harness::protocol::Protocol::ChatCompletions
    );
    assert_eq!(model.provider_id, "deepseek-api");
    assert!(
        catalog
            .get("deepseek-api", "deepseek-flash-messages")
            .is_none()
    );
    assert!(
        catalog
            .get("deepseek-api", "deepseek-flash-responses")
            .is_none()
    );
}

#[test]
fn invalid_provider_fields_are_rejected_with_location() {
    let mut provider = serde_json::to_value(custom_provider("gateway")).unwrap();
    provider["models"][0]["private_response_id"] = json!("inject");
    let error = serde_json::from_value::<ProviderDefinition>(provider)
        .unwrap_err()
        .to_string();
    assert!(error.contains("private_response_id"));
    let mut provider = custom_provider("gateway");
    provider.auth = ModelAuthSource::CodexSubscription;
    let error = ModelCatalog::from_providers(vec![provider]).unwrap_err();
    assert!(error.contains("providers[gateway].auth"));
}

#[test]
fn explicit_default_selection_resolves_from_catalog() {
    let catalog = ModelCatalog::defaults()
        .with_providers(vec![custom_patch("gateway")])
        .unwrap();
    let selection: ModelSelection =
        serde_json::from_value(json!({"provider":"gateway","model":"assistant"})).unwrap();
    assert_eq!(
        catalog
            .get(&selection.provider, &selection.model)
            .unwrap()
            .name,
        "Gateway Assistant"
    );
}

#[test]
fn saved_kimi_k3_selection_loads_the_current_context_window() {
    let root = tempfile::tempdir().unwrap();
    for (model_id, window) in [("k3", 1_048_576), ("k3-256k", 262_144)] {
        std::fs::write(
            root.path().join("settings.json"),
            json!({"defaultModel":{"provider":"kimi-code-oauth","model":model_id}}).to_string(),
        )
        .unwrap();
        let mut settings = UserSettings::in_root(root.path()).load();
        let selected = settings.configured_model().unwrap().unwrap();
        assert_eq!(selected.model, model_id);
        assert_eq!(selected.maximum_context_window_tokens(), Some(window));
        assert!(settings.warnings().is_empty());
    }
}

#[test]
fn saved_legacy_builtin_uses_version_owned_limit_with_one_notice() {
    let mut legacy: ModelConfig =
        serde_json::from_str(tickloop_harness::test_support::legacy_models::DEEPSEEK_CHAT_CONFIG)
            .unwrap();
    legacy.max_output_tokens = 4096;
    let explicit = migrate_legacy_model(legacy.clone());
    assert_eq!(explicit.max_output_tokens, 4096);
    assert_eq!(
        explicit.card.as_ref().unwrap().adapter,
        Adapter::DeepseekChatCompletions
    );
    assert_ne!(explicit.provider_id, "deepseek-api");

    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("settings.json"),
        json!({"model":legacy}).to_string(),
    )
    .unwrap();
    let mut saved = UserSettings::in_root(root.path()).load();
    let selected = saved.configured_model().unwrap().unwrap();
    assert_eq!(selected.max_output_tokens, 64_000);
    assert_eq!(selected.provider_id, "deepseek-api");
    assert_eq!(saved.warnings().len(), 1);
    assert!(saved.warnings()[0].contains("built-in"));
}

#[test]
fn custom_legacy_deepseek_messages_is_migrated_without_losing_public_fields() {
    let mut legacy: ModelConfig =
        serde_json::from_str(tickloop_harness::test_support::legacy_models::DEEPSEEK_CONFIG)
            .unwrap();
    legacy.name = "Research access".into();
    legacy.api_key_env = "RESEARCH_DEEPSEEK_KEY".into();
    legacy.max_output_tokens = 8192;
    let migrated = migrate_legacy_model(legacy.clone());
    assert_ne!(migrated.provider_id, "deepseek-api");
    assert_eq!(migrated.name, legacy.name);
    assert_eq!(migrated.api_key_env, legacy.api_key_env);
    assert_eq!(migrated.base_url, legacy.base_url);
    assert_eq!(migrated.max_output_tokens, legacy.max_output_tokens);
    assert_eq!(
        migrated.card.as_ref().unwrap().adapter,
        Adapter::DeepseekMessages
    );
    assert_eq!(migrated.card.as_ref().unwrap().context_window_tokens, None);
    assert_eq!(migrate_legacy_model(migrated.clone()), migrated);
}

#[test]
fn unknown_legacy_deepseek_model_gets_dialect_without_invented_capabilities() {
    let mut legacy: ModelConfig =
        serde_json::from_str(tickloop_harness::test_support::legacy_models::DEEPSEEK_CONFIG)
            .unwrap();
    legacy.model = "future-private-model".into();
    legacy.name = "Private model".into();
    let migrated = migrate_legacy_model(legacy);
    let card = migrated.card.unwrap();
    assert_eq!(card.adapter, Adapter::DeepseekMessages);
    assert_eq!(card.context_window_tokens, None);
    assert_eq!(card.thinking, ThinkingControl::Unsupported);
    assert!(card.effort_levels.is_empty());
}

#[test]
fn custom_legacy_deepseek_responses_does_not_invent_retired_route_capabilities() {
    let mut legacy: ModelConfig = serde_json::from_str(
        tickloop_harness::test_support::legacy_models::DEEPSEEK_RESPONSES_CONFIG,
    )
    .unwrap();
    legacy.name = "Research Responses".into();
    legacy.max_output_tokens = 8192;
    let migrated = migrate_legacy_model(legacy.clone());
    assert_eq!(migrated.name, legacy.name);
    assert_eq!(migrated.max_output_tokens, 8192);
    assert_eq!(migrated.base_url, legacy.base_url);
    assert_ne!(migrated.provider_id, "deepseek-api");
    let card = migrated.card.as_ref().unwrap();
    assert_eq!(card.adapter, Adapter::Responses);
    assert_eq!(card.context_window_tokens, None);
    assert!(card.effort_levels.is_empty());
    assert_eq!(migrate_legacy_model(migrated.clone()), migrated);
}

#[test]
fn custom_legacy_codex_keeps_fixed_adapter_and_known_capabilities() {
    let mut legacy = ModelCatalog::defaults()
        .get("openai-codex", "gpt-5.6-sol")
        .unwrap();
    legacy.name = "Personal Codex".into();
    legacy.max_output_tokens = 16_384;
    legacy.provider_id.clear();
    legacy.provider_name.clear();
    legacy.model_id.clear();
    legacy.card = None;
    let migrated = migrate_legacy_model(legacy.clone());
    assert_eq!(migrated.name, legacy.name);
    assert_eq!(migrated.max_output_tokens, legacy.max_output_tokens);
    assert_eq!(migrated.base_url, legacy.base_url);
    assert_ne!(migrated.provider_id, "openai-codex");
    let card = migrated.card.as_ref().unwrap();
    assert_eq!(card.adapter, Adapter::CodexResponses);
    assert_eq!(card.context_window_tokens, Some(872_000));
    assert!(!card.effort_levels.is_empty());
    assert_eq!(migrate_legacy_model(migrated.clone()), migrated);
}

#[test]
fn every_bundled_model_has_one_service_identity() {
    let catalog = ModelCatalog::defaults();
    for provider in &catalog.providers {
        for card in &provider.models {
            assert_eq!(catalog.get(&provider.id, &card.id).unwrap().model, card.id);
        }
    }
}

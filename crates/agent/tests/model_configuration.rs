use std::{collections::BTreeMap, sync::Arc, thread};

use serde_json::json;
use tickloop_agent::{
    model::{
        ModelAuthSource, ModelConfig,
        cards::{
            Adapter, ModelCard, ModelCatalog, ModelCatalogFile, ModelSelection, ProviderDefinition,
            ProviderPatch, ThinkingControl, migrate_legacy_model, model_for_persistent_selection,
        },
    },
    settings::UserSettings,
};

fn custom_provider(id: &str) -> ProviderDefinition {
    ProviderDefinition {
        connection: None,
        id: id.into(),
        display_name: format!("{id} gateway"),
        auth: ModelAuthSource::ApiKeyEnv,
        api_key_env: "GATEWAY_API_KEY".into(),
        endpoints: BTreeMap::from([(Adapter::Responses, "http://127.0.0.1:3001".into())]),
        models: vec![ModelCard {
            id: "assistant-responses".into(),
            service_model_id: "assistant".into(),
            display_name: "Gateway Assistant".into(),
            adapter: Adapter::Responses,
            max_output_tokens: 4096,
            context_window_tokens: Some(32_000),
            thinking: ThinkingControl::Toggle,
            adaptive_by_default: false,
            effort_levels: vec![tickloop_agent::model::ReasoningEffort::High],
            input_modalities: vec![tickloop_agent::model::InputModality::Text],
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
        ("deepseek-flash-messages", true),
        ("deepseek-flash-chat-completions", true),
        ("deepseek-flash-responses", false),
        ("deepseek-v4-pro-messages", false),
        ("deepseek-v4-pro-chat-completions", false),
        ("deepseek-v4-flash-messages", false),
        ("deepseek-v4-flash-chat-completions", false),
    ] {
        let model = catalog.get("deepseek-api", model_id).unwrap();
        assert_eq!(model.accepts_images(), accepts_images, "{model_id}");
    }
}

#[test]
fn provider_model_identity_and_service_id_are_distinct() {
    let catalog = ModelCatalog::defaults();
    catalog.validate().unwrap();
    let messages = catalog
        .get("deepseek-api", "deepseek-flash-messages")
        .unwrap();
    let responses = catalog
        .get("deepseek-api", "deepseek-flash-responses")
        .unwrap();
    assert_eq!(messages.model, responses.model);
    assert_ne!(messages.model_id, responses.model_id);
    assert_ne!(messages.protocol, responses.protocol);
    assert_eq!(messages.provider_id, "deepseek-api");
    assert_eq!(
        catalog
            .get("openai-codex", "gpt-5.6-sol-responses")
            .unwrap()
            .provider_id,
        "openai-codex"
    );
}

#[test]
fn custom_provider_survives_settings_save_and_restore_without_losing_unrelated_keys() {
    let root = tempfile::tempdir().unwrap();
    let settings = UserSettings::in_root(root.path());
    std::fs::write(
        root.path().join("settings.json"),
        r#"{"futureSetting":{"keep":true}}"#,
    )
    .unwrap();
    settings.save_provider(custom_patch("gateway")).unwrap();
    let selected = settings
        .model_catalog()
        .unwrap()
        .get("gateway", "assistant-responses")
        .unwrap();
    assert_eq!(selected.model, "assistant");
    assert_eq!(selected.card.unwrap().context_window_tokens, Some(32_000));
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.path().join("settings.json")).unwrap()).unwrap();
    assert_eq!(saved["futureSetting"], json!({"keep":true}));
    assert!(saved.get("providers").is_none());
    let yaml = std::fs::read_to_string(root.path().join("providers/gateway.yaml")).unwrap();
    assert_eq!(
        serde_yaml_ng::from_str::<ProviderPatch>(&yaml).unwrap().id,
        "gateway"
    );
    settings.restore_provider_default("gateway").unwrap();
    assert!(
        settings
            .model_catalog()
            .unwrap()
            .get("gateway", "assistant-responses")
            .is_none()
    );
    assert_eq!(
        settings
            .model_catalog()
            .unwrap()
            .get("deepseek-api", "deepseek-flash-messages")
            .unwrap()
            .model,
        "deepseek-flash"
    );
}

#[test]
fn separate_process_style_writers_keep_both_providers() {
    let root = tempfile::tempdir().unwrap();
    let settings = Arc::new(UserSettings::in_root(root.path()));
    let workers: Vec<_> = ["first", "second"]
        .into_iter()
        .map(|id| {
            let settings = settings.clone();
            thread::spawn(move || settings.save_provider(custom_patch(id)).unwrap())
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }
    let catalog = settings.model_catalog().unwrap();
    assert!(catalog.get("first", "assistant-responses").is_some());
    assert!(catalog.get("second", "assistant-responses").is_some());
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
fn rejected_provider_save_preserves_the_last_valid_settings() {
    let root = tempfile::tempdir().unwrap();
    let settings = UserSettings::in_root(root.path());
    settings.save_provider(custom_patch("gateway")).unwrap();
    let before = std::fs::read(root.path().join("providers/gateway.yaml")).unwrap();
    let mut invalid = custom_patch("other-gateway");
    invalid.auth = Some(ModelAuthSource::CodexSubscription);
    let error = settings.save_provider(invalid).unwrap_err().to_string();
    assert!(error.contains("providers[other-gateway].auth"), "{error}");
    assert_eq!(
        std::fs::read(root.path().join("providers/gateway.yaml")).unwrap(),
        before
    );

    let mut saved: serde_yaml_ng::Value = serde_yaml_ng::from_slice(&before).unwrap();
    saved["models"][0]["private_response_id"] = serde_yaml_ng::Value::String("inject".into());
    std::fs::write(
        root.path().join("providers/gateway.yaml"),
        serde_yaml_ng::to_string(&saved).unwrap(),
    )
    .unwrap();
    let error = settings.load().configured_model().unwrap_err();
    assert!(error.contains("private_response_id"), "{error}");

    std::fs::write(root.path().join("settings.json"), "{broken").unwrap();
    let error = settings.load().configured_model().unwrap_err();
    assert!(error.contains("not valid JSON"), "{error}");
}

#[test]
fn explicit_default_selection_resolves_from_catalog() {
    let catalog = ModelCatalog::defaults()
        .with_providers(vec![custom_patch("gateway")])
        .unwrap();
    let selection: ModelSelection =
        serde_json::from_value(json!({"provider":"gateway","model":"assistant-responses"}))
            .unwrap();
    assert_eq!(
        catalog
            .get(&selection.provider, &selection.model)
            .unwrap()
            .name,
        "Gateway Assistant"
    );
}

#[test]
fn documented_provider_file_selects_a_valid_model() {
    let selected = ModelCatalog::selected_provider_yaml(include_str!(
        "../examples/providers/deepseek-api.yaml"
    ))
    .unwrap();
    assert_eq!(selected.provider_id, "deepseek-api");
    let codex = ModelCatalog::selected_provider_yaml(include_str!(
        "../examples/providers/openai-codex.yaml"
    ))
    .unwrap();
    assert_eq!(codex.provider_id, "openai-codex");
    assert_eq!(selected.model_id, selected.card.as_ref().unwrap().id);
}

#[test]
fn one_builtin_capability_override_keeps_sibling_models_and_provider_defaults() {
    let root = tempfile::tempdir().unwrap();
    let settings = UserSettings::in_root(root.path());
    let patch: ProviderPatch = serde_json::from_value(json!({
        "id": "deepseek-api",
        "models": [{"id": "deepseek-flash-messages", "context_window_tokens": 64_000}]
    }))
    .unwrap();
    settings.save_provider(patch).unwrap();
    let saved: ProviderPatch = serde_yaml_ng::from_slice(
        &std::fs::read(root.path().join("providers/deepseek-api.yaml")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        saved.models.as_ref().unwrap()[0].context_window_tokens,
        Some(Some(64_000))
    );
    let catalog = settings.model_catalog().unwrap();
    let changed = catalog
        .get("deepseek-api", "deepseek-flash-messages")
        .unwrap();
    let sibling = catalog
        .get("deepseek-api", "deepseek-flash-responses")
        .unwrap();
    assert_eq!(
        changed.card.as_ref().unwrap().context_window_tokens,
        Some(64_000)
    );
    assert_eq!(
        sibling.card.as_ref().unwrap().context_window_tokens,
        Some(1_000_000)
    );
    assert_eq!(changed.api_key_env, "DEEPSEEK_API_KEY");
    let patch: ProviderPatch = serde_json::from_value(json!({
        "id": "deepseek-api",
        "models": [{"id": "deepseek-flash-messages", "max_output_tokens": 8192}]
    }))
    .unwrap();
    settings.save_provider(patch).unwrap();
    let changed = settings
        .model_catalog()
        .unwrap()
        .get("deepseek-api", "deepseek-flash-messages")
        .unwrap();
    assert_eq!(
        changed.card.as_ref().unwrap().context_window_tokens,
        Some(64_000)
    );
    assert_eq!(changed.max_output_tokens, 8192);
    settings
        .restore_model_default("deepseek-api", "deepseek-flash-messages")
        .unwrap();
    let restored = settings
        .model_catalog()
        .unwrap()
        .get("deepseek-api", "deepseek-flash-messages")
        .unwrap();
    assert_eq!(
        restored.card.as_ref().unwrap().context_window_tokens,
        Some(1_000_000)
    );
    assert_eq!(restored.max_output_tokens, 64_000);
}

#[test]
fn saved_legacy_builtin_upgrade_does_not_change_explicit_custom_limit() {
    let mut legacy: ModelConfig =
        serde_json::from_str(include_str!("../examples/deepseek-config.json")).unwrap();
    legacy.max_output_tokens = 4096;
    let explicit = migrate_legacy_model(legacy.clone());
    assert_eq!(explicit.max_output_tokens, 4096);
    assert_eq!(
        explicit.card.as_ref().unwrap().adapter,
        Adapter::DeepseekMessages
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
    assert!(saved.warnings().is_empty());
}

#[test]
fn custom_legacy_deepseek_messages_is_migrated_without_losing_public_fields() {
    let mut legacy: ModelConfig =
        serde_json::from_str(include_str!("../examples/deepseek-config.json")).unwrap();
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
    assert_eq!(
        migrated.card.as_ref().unwrap().context_window_tokens,
        Some(1_000_000)
    );
    assert_eq!(migrate_legacy_model(migrated.clone()), migrated);
}

#[test]
fn unknown_legacy_deepseek_model_gets_dialect_without_invented_capabilities() {
    let mut legacy: ModelConfig =
        serde_json::from_str(include_str!("../examples/deepseek-config.json")).unwrap();
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
fn custom_legacy_deepseek_responses_keeps_known_capabilities() {
    let mut legacy: ModelConfig =
        serde_json::from_str(include_str!("../examples/deepseek-responses-config.json")).unwrap();
    legacy.name = "Research Responses".into();
    legacy.max_output_tokens = 8192;
    let migrated = migrate_legacy_model(legacy.clone());
    assert_eq!(migrated.name, legacy.name);
    assert_eq!(migrated.max_output_tokens, 8192);
    assert_eq!(migrated.base_url, legacy.base_url);
    assert_ne!(migrated.provider_id, "deepseek-api");
    let card = migrated.card.as_ref().unwrap();
    assert_eq!(card.adapter, Adapter::Responses);
    assert_eq!(card.context_window_tokens, Some(1_000_000));
    assert!(!card.effort_levels.is_empty());
    assert_eq!(migrate_legacy_model(migrated.clone()), migrated);
}

#[test]
fn custom_legacy_codex_keeps_fixed_adapter_and_known_capabilities() {
    let mut legacy = ModelCatalog::defaults()
        .get("openai-codex", "gpt-5.6-sol-responses")
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
    assert_eq!(card.context_window_tokens, Some(272_000));
    assert!(!card.effort_levels.is_empty());
    assert_eq!(migrate_legacy_model(migrated.clone()), migrated);
}

#[test]
fn invalid_explicit_default_is_reported_even_when_legacy_choice_exists() {
    let root = tempfile::tempdir().unwrap();
    let legacy: ModelConfig =
        serde_json::from_str(include_str!("../examples/deepseek-config.json")).unwrap();
    std::fs::write(
        root.path().join("settings.json"),
        json!({
            "model": legacy,
            "defaultModel": {"provider":"missing","model":"assistant"}
        })
        .to_string(),
    )
    .unwrap();
    let error = UserSettings::in_root(root.path())
        .load()
        .configured_model()
        .unwrap_err();
    assert!(error.contains("defaultModel"), "{error}");
    assert!(error.contains("missing/assistant"), "{error}");
}

#[test]
fn provider_yaml_overrides_legacy_settings_and_reset_does_not_revive_it() {
    let root = tempfile::tempdir().unwrap();
    let settings = UserSettings::in_root(root.path());
    std::fs::write(
        root.path().join("settings.json"),
        json!({
            "providers": [{"id":"deepseek-api", "display_name":"Old Label"}]
        })
        .to_string(),
    )
    .unwrap();
    std::fs::create_dir(root.path().join("providers")).unwrap();
    std::fs::write(
        root.path().join("providers/deepseek-api.yaml"),
        "id: deepseek-api\ndisplay_name: New Label\n",
    )
    .unwrap();
    assert_eq!(
        settings
            .model_catalog()
            .unwrap()
            .get("deepseek-api", "deepseek-flash-messages")
            .unwrap()
            .provider_name,
        "New Label"
    );
    settings.restore_provider_default("deepseek-api").unwrap();
    assert_eq!(
        settings
            .model_catalog()
            .unwrap()
            .get("deepseek-api", "deepseek-flash-messages")
            .unwrap()
            .provider_name,
        "DeepSeek API"
    );
    assert!(!root.path().join("providers/deepseek-api.yaml").exists());
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.path().join("settings.json")).unwrap()).unwrap();
    assert!(saved["providers"].as_array().unwrap().is_empty());
}

#[test]
fn partial_yaml_preserves_legacy_endpoint_and_sibling_models() {
    let root = tempfile::tempdir().unwrap();
    let settings = UserSettings::in_root(root.path());
    std::fs::write(
        root.path().join("settings.json"),
        json!({
            "providers": [{
                "id":"deepseek-api",
                "api_key_env":"OLD_DEEPSEEK_KEY",
                "endpoints":{"messages":"https://old.example/messages"},
                "models":[{"id":"deepseek-flash-responses","max_output_tokens":8192}]
            }]
        })
        .to_string(),
    )
    .unwrap();
    std::fs::create_dir(root.path().join("providers")).unwrap();
    std::fs::write(root.path().join("providers/deepseek-api.yaml"),
        "id: deepseek-api\nmodels:\n  - id: deepseek-flash-messages\n    max_output_tokens: 16384\n").unwrap();
    let catalog = settings.model_catalog().unwrap();
    let selected = catalog
        .get("deepseek-api", "deepseek-flash-messages")
        .unwrap();
    assert_eq!(selected.base_url, "https://old.example/messages");
    assert_eq!(selected.api_key_env, "OLD_DEEPSEEK_KEY");
    assert_eq!(selected.max_output_tokens, 16384);
    assert_eq!(
        catalog
            .get("deepseek-api", "deepseek-flash-responses")
            .unwrap()
            .max_output_tokens,
        8192
    );
}

#[test]
fn provider_yaml_rejects_unknown_fields_duplicate_models_and_bad_file_names() {
    let root = tempfile::tempdir().unwrap();
    let settings = UserSettings::in_root(root.path());
    std::fs::create_dir(root.path().join("providers")).unwrap();
    let path = root.path().join("providers/deepseek-api.yaml");
    std::fs::write(&path, "id: deepseek-api\nmodels:\n  - id: deepseek-flash-messages\n    unknown_capability: true\n").unwrap();
    assert!(
        settings
            .model_catalog()
            .unwrap_err()
            .contains("unknown_capability")
    );
    std::fs::write(&path, "id: deepseek-api\nmodels:\n  - id: deepseek-flash-messages\n  - id: deepseek-flash-messages\n").unwrap();
    assert!(
        settings
            .model_catalog()
            .unwrap_err()
            .contains("duplicate model id")
    );
    std::fs::write(&path, "id: another-provider\n").unwrap();
    assert!(
        settings
            .model_catalog()
            .unwrap_err()
            .contains("file name must match")
    );
    std::fs::remove_file(&path).unwrap();
    std::fs::write(root.path().join("providers/Unsafe.yaml"), "id: Unsafe\n").unwrap();
    assert!(
        settings
            .model_catalog()
            .unwrap_err()
            .contains("invalid provider file name")
    );
}

#[test]
fn explicit_provider_yaml_requires_selection_when_models_are_ambiguous() {
    let single = ModelCatalog::selected_provider_yaml(
        "id: deepseek-api\nmodels:\n  - id: deepseek-flash-responses\n",
    )
    .unwrap();
    assert_eq!(single.model_id, "deepseek-flash-responses");
    let error = ModelCatalog::selected_provider_yaml("id: deepseek-api\nmodels:\n  - id: deepseek-flash-messages\n  - id: deepseek-flash-responses\n").unwrap_err();
    assert!(error.contains("default_model"));
    let selected = ModelCatalog::selected_provider_yaml("id: deepseek-api\ndefault_model: deepseek-flash-responses\nmodels:\n  - id: deepseek-flash-messages\n  - id: deepseek-flash-responses\n").unwrap();
    assert_eq!(selected.model_id, "deepseek-flash-responses");
}

#[test]
fn yaml_null_capability_is_unknown_and_provider_default_does_not_replace_saved_choice() {
    let root = tempfile::tempdir().unwrap();
    let settings = UserSettings::in_root(root.path());
    std::fs::write(
        root.path().join("settings.json"),
        json!({
            "defaultModel":{"provider":"deepseek-api","model":"deepseek-flash-responses"}
        })
        .to_string(),
    )
    .unwrap();
    std::fs::create_dir(root.path().join("providers")).unwrap();
    std::fs::write(root.path().join("providers/deepseek-api.yaml"),
        "id: deepseek-api\ndefault_model: deepseek-flash-messages\nmodels:\n  - id: deepseek-flash-messages\n    context_window_tokens: null\n").unwrap();
    let catalog = settings.model_catalog().unwrap();
    assert_eq!(
        catalog
            .get("deepseek-api", "deepseek-flash-messages")
            .unwrap()
            .card
            .unwrap()
            .context_window_tokens,
        None
    );
    assert_eq!(
        settings
            .load()
            .configured_model()
            .unwrap()
            .unwrap()
            .model_id,
        "deepseek-flash-responses"
    );
}

#[test]
fn same_provider_concurrent_field_updates_are_merged() {
    let root = tempfile::tempdir().unwrap();
    let settings = Arc::new(UserSettings::in_root(root.path()));
    let workers: Vec<_> = [
        json!({"id":"deepseek-api", "display_name":"DeepSeek Custom"}),
        json!({"id":"deepseek-api", "models":[{"id":"deepseek-flash-messages", "max_output_tokens":8192}]}),
    ].into_iter().map(|value| {
        let settings = settings.clone();
        thread::spawn(move || settings.save_provider(serde_json::from_value(value).unwrap()).unwrap())
    }).collect();
    for worker in workers {
        worker.join().unwrap();
    }
    let selected = settings
        .model_catalog()
        .unwrap()
        .get("deepseek-api", "deepseek-flash-messages")
        .unwrap();
    assert_eq!(selected.provider_name, "DeepSeek Custom");
    assert_eq!(selected.max_output_tokens, 8192);
}

#[test]
fn failed_provider_write_preserves_existing_yaml() {
    let root = tempfile::tempdir().unwrap();
    let settings = UserSettings::in_root(root.path());
    settings
        .save_provider(
            serde_json::from_value(json!({"id":"deepseek-api", "display_name":"Saved"})).unwrap(),
        )
        .unwrap();
    let path = root.path().join("providers/deepseek-api.yaml");
    let before = std::fs::read(&path).unwrap();
    let mut invalid = custom_patch("deepseek-api");
    invalid.auth = Some(ModelAuthSource::CodexSubscription);
    assert!(settings.save_provider(invalid).is_err());
    assert_eq!(std::fs::read(path).unwrap(), before);
}

#[test]
fn failed_yaml_reset_preserves_legacy_and_yaml_overrides() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().unwrap();
    let settings = UserSettings::in_root(root.path());
    std::fs::write(root.path().join("settings.json"), json!({
        "providers":[{"id":"deepseek-api","models":[{"id":"deepseek-flash-messages","max_output_tokens":8192}]}]
    }).to_string()).unwrap();
    let patch: ProviderPatch = serde_json::from_value(json!({
        "id":"deepseek-api","models":[{"id":"deepseek-flash-messages","max_output_tokens":16384}]
    }))
    .unwrap();
    settings.save_provider(patch).unwrap();
    let yaml_path = root.path().join("providers/deepseek-api.yaml");
    let yaml_before = std::fs::read(&yaml_path).unwrap();
    let settings_before = std::fs::read(root.path().join("settings.json")).unwrap();
    let directory = root.path().join("providers");
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o500)).unwrap();
    let result = settings.restore_model_default("deepseek-api", "deepseek-flash-messages");
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(result.is_err());
    assert_eq!(std::fs::read(yaml_path).unwrap(), yaml_before);
    assert_eq!(
        std::fs::read(root.path().join("settings.json")).unwrap(),
        settings_before
    );
}

#[test]
fn reset_rejects_removing_the_saved_custom_selection() {
    let root = tempfile::tempdir().unwrap();
    let settings = UserSettings::in_root(root.path());
    let mut provider = custom_provider("gateway");
    let mut sibling = provider.models[0].clone();
    sibling.id = "second-responses".into();
    sibling.service_model_id = "second".into();
    sibling.display_name = "Second Assistant".into();
    provider.models.push(sibling);
    settings
        .save_provider(serde_json::from_value(serde_json::to_value(provider).unwrap()).unwrap())
        .unwrap();
    std::fs::write(
        root.path().join("settings.json"),
        json!({
            "defaultModel":{"provider":"gateway","model":"assistant-responses"}
        })
        .to_string(),
    )
    .unwrap();
    let yaml_path = root.path().join("providers/gateway.yaml");
    let yaml_before = std::fs::read(&yaml_path).unwrap();
    let settings_before = std::fs::read(root.path().join("settings.json")).unwrap();
    assert!(
        settings
            .restore_model_default("gateway", "assistant-responses")
            .unwrap_err()
            .to_string()
            .contains("select another model first")
    );
    assert!(
        settings
            .restore_provider_default("gateway")
            .unwrap_err()
            .to_string()
            .contains("select another model first")
    );
    assert_eq!(std::fs::read(yaml_path).unwrap(), yaml_before);
    assert_eq!(
        std::fs::read(root.path().join("settings.json")).unwrap(),
        settings_before
    );
    assert_eq!(
        UserSettings::in_root(root.path())
            .load()
            .configured_model()
            .unwrap()
            .unwrap()
            .model_id,
        "assistant-responses"
    );
}

#[test]
fn legacy_json_provider_file_remains_readable() {
    let yaml = include_str!("../examples/providers/deepseek-api.yaml");
    let selected = ModelCatalog::selected_provider_yaml(yaml).unwrap();
    let provider = ModelCatalog::defaults()
        .providers
        .into_iter()
        .find(|provider| provider.id == selected.provider_id)
        .unwrap();
    let legacy = ModelCatalogFile {
        providers: vec![provider],
        default_model: ModelSelection {
            provider: selected.provider_id,
            model: selected.model_id,
        },
    };
    assert_eq!(legacy.selected_model().unwrap().model, selected.model);
}

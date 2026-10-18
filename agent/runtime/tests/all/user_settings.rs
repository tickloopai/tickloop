use std::{os::unix::fs::MetadataExt, path::Path, sync::Arc};

use serde_json::json;
use tickloop_agent_base::output_style::OutputStyle;
use tickloop_agent_config::{DisplayPreferences, UserSettings};
use tickloop_agent_model::cards::model_for_persistent_selection;
use tickloop_agent_protocol::{
    ChatId, Content, Decision, EventKind, ModelChange, ModelConfig, PermissionRule,
    PermissionScope, ReasoningEffort,
};
use tickloop_agent_runtime::Agent;
use tickloop_agent_test_support::{ScriptedModel, call, config, next_event, text};
use tickloop_agent_tools::permissions::permission_rule_candidates;
use tokio::sync::Barrier;
use uuid::Uuid;

fn selected_model() -> ModelConfig {
    tickloop_agent_model::cards::ModelCatalog::defaults()
        .get("deepseek-api", "deepseek-v4-pro")
        .unwrap()
}

#[test]
fn prompt_suggestions_default_on_and_can_be_disabled_without_losing_settings() {
    let directory = tempfile::tempdir().unwrap();
    let settings = UserSettings::in_root(directory.path());
    assert!(settings.load().prompt_suggestion_enabled());
    std::fs::write(
        directory.path().join("settings.json"),
        r#"{"futureSetting": 42}"#,
    )
    .unwrap();
    settings.save_prompt_suggestion_enabled(false).unwrap();
    assert!(!settings.load().prompt_suggestion_enabled());
    assert_eq!(settings_json(directory.path())["futureSetting"], 42);
    settings.save_prompt_suggestion_enabled(true).unwrap();
    assert!(settings.load().prompt_suggestion_enabled());
}

#[test]
fn display_preferences_round_trip_without_replacing_unrelated_settings() {
    let directory = tempfile::tempdir().unwrap();
    let settings = UserSettings::in_root(directory.path());
    std::fs::write(
        directory.path().join("settings.json"),
        r#"{"cleanupPeriodDays": 7, "futureSetting": true}"#,
    )
    .unwrap();

    settings
        .save_display_preferences(DisplayPreferences {
            prefers_reduced_motion: true,
        })
        .unwrap();

    assert_eq!(
        settings.display_preferences(),
        DisplayPreferences {
            prefers_reduced_motion: true,
        }
    );
    assert_eq!(settings_json(directory.path())["cleanupPeriodDays"], 7);
    assert_eq!(settings_json(directory.path())["futureSetting"], true);
    assert!(
        settings_json(directory.path())
            .get("statusLinePosition")
            .is_none()
    );
}

#[test]
fn legacy_status_position_is_silently_ignored_and_preserved_on_display_save() {
    let directory = tempfile::tempdir().unwrap();
    let settings = UserSettings::in_root(directory.path());
    for legacy in [
        json!("left"),
        json!("center"),
        json!(12),
        json!(false),
        json!(null),
        json!({}),
    ] {
        let original = json!({"statusLinePosition": legacy, "futureSetting": {"keep": true}});
        std::fs::write(directory.path().join("settings.json"), original.to_string()).unwrap();
        let mut loaded = settings.load();
        assert_eq!(loaded.display_preferences(), DisplayPreferences::default());
        assert!(loaded.warnings().is_empty(), "{:?}", loaded.warnings());
        assert_eq!(settings_json(directory.path()), original);
        settings
            .save_display_preferences(DisplayPreferences {
                prefers_reduced_motion: true,
            })
            .unwrap();
        let saved = settings_json(directory.path());
        assert_eq!(saved["statusLinePosition"], legacy);
        assert_eq!(saved["futureSetting"], original["futureSetting"]);
    }
}

/// Tickloop has one glyph set; a `glyphSet` an earlier version saved is read
/// past without a warning, as any key Tickloop does not use.
#[test]
fn a_glyph_set_an_earlier_version_saved_is_silently_ignored() {
    let directory = tempfile::tempdir().unwrap();
    let settings = UserSettings::in_root(directory.path());
    for legacy in [json!("unicode"), json!("private-use"), json!(3)] {
        std::fs::write(
            directory.path().join("settings.json"),
            json!({"glyphSet": legacy, "prefersReducedMotion": true}).to_string(),
        )
        .unwrap();
        let mut loaded = settings.load();
        assert_eq!(
            loaded.display_preferences(),
            DisplayPreferences {
                prefers_reduced_motion: true,
            }
        );
        assert!(loaded.warnings().is_empty(), "{:?}", loaded.warnings());
    }
}

#[test]
fn output_style_round_trips_and_an_unknown_value_falls_back_with_a_warning() {
    let directory = tempfile::tempdir().unwrap();
    let settings = UserSettings::in_root(directory.path());
    assert_eq!(settings.output_style(), OutputStyle::Standard);
    settings.save_output_style(OutputStyle::Detailed).unwrap();
    assert_eq!(settings.output_style(), OutputStyle::Detailed);
    assert_eq!(settings_json(directory.path())["outputStyle"], "detailed");

    std::fs::write(
        directory.path().join("settings.json"),
        r#"{"outputStyle": "loud"}"#,
    )
    .unwrap();
    let mut loaded = settings.load();
    assert_eq!(loaded.output_style(), OutputStyle::Standard);
    assert_eq!(loaded.warnings().len(), 1);
}

#[test]
fn reasoning_effort_round_trips_without_replacing_other_settings() {
    let directory = tempfile::tempdir().unwrap();
    let settings = UserSettings::in_root(directory.path());
    std::fs::write(
        directory.path().join("settings.json"),
        r#"{"futureSetting":true}"#,
    )
    .unwrap();
    settings
        .save_reasoning_effort(Some(ReasoningEffort::Max))
        .unwrap();
    assert_eq!(settings.reasoning_effort(), Some(ReasoningEffort::Max));
    assert_eq!(settings_json(directory.path())["effortLevel"], "max");
    assert_eq!(settings_json(directory.path())["futureSetting"], true);
    settings.save_reasoning_effort(None).unwrap();
    assert!(settings_json(directory.path()).get("effortLevel").is_none());
}

#[tokio::test]
async fn selected_effort_reaches_requests_and_new_chats() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let workdir = directory.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    let mut model_config = tickloop_agent_model::cards::ModelCatalog::defaults()
        .get("deepseek-api", "deepseek-flash")
        .unwrap();
    model_config.api_key_env = "PATH".into();
    let model = Arc::new(ScriptedModel::new(vec![text("first"), text("second")]));
    let agent = Agent::open_embedded(root.clone(), model_config, model.clone()).unwrap();
    let first = agent.create_chat(workdir.clone()).await.unwrap();
    agent
        .set_reasoning_effort(first, Some(ReasoningEffort::Max))
        .await
        .unwrap();
    run_turn(&agent, first, "First", PermissionScope::Once).await;
    let second = agent.create_chat(workdir).await.unwrap();
    assert_eq!(
        agent.snapshot(second).await.unwrap().reasoning_effort,
        Some(ReasoningEffort::Max)
    );
    run_turn(&agent, second, "Second", PermissionScope::Once).await;
    assert!(
        model
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|request| request.reasoning_effort == Some(ReasoningEffort::Max))
    );
    assert_eq!(settings_json(&root)["effortLevel"], "max");

    let unsupported =
        Agent::open_embedded(root, config(), Arc::new(ScriptedModel::new(Vec::new()))).unwrap();
    let other_chat = unsupported
        .create_chat(directory.path().to_path_buf())
        .await
        .unwrap();
    assert_eq!(
        unsupported
            .snapshot(other_chat)
            .await
            .unwrap()
            .reasoning_effort,
        None
    );
    assert!(
        unsupported
            .user_settings_warnings(other_chat)
            .iter()
            .any(|warning| warning.contains("`effortLevel` max is unsupported"))
    );
}

#[tokio::test]
async fn model_and_thinking_selection_saves_together_and_failed_save_changes_neither() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let workdir = directory.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    let original = model_for_persistent_selection(config());
    let agent = Agent::open_embedded(
        root.clone(),
        original.clone(),
        Arc::new(ScriptedModel::new(Vec::new())),
    )
    .unwrap();
    let chat = agent.create_chat(workdir.clone()).await.unwrap();
    let selected = tickloop_agent_model::cards::ModelCatalog::defaults()
        .get("deepseek-api", "deepseek-flash")
        .unwrap();
    let settings_path = root.join("settings.json");
    let invalid = b"{\"defaultModel\":";
    std::fs::write(&settings_path, invalid).unwrap();
    assert!(
        agent
            .set_model_with_thinking(
                chat,
                selected.clone(),
                Arc::new(ScriptedModel::new(Vec::new())),
                Some(ReasoningEffort::High),
            )
            .await
            .is_err()
    );
    assert_eq!(std::fs::read(&settings_path).unwrap(), invalid);
    assert!(!root.join("providers/deepseek-api.yaml").exists());
    let unchanged = agent.snapshot(chat).await.unwrap();
    assert_eq!(unchanged.model, original);
    assert_eq!(unchanged.reasoning_effort, None);
    assert!(unchanged.thinking_enabled);
    assert!(unchanged.pending_model.is_none());
    let fresh = agent.create_chat(workdir).await.unwrap();
    assert_eq!(agent.snapshot(fresh).await.unwrap().model, original);

    std::fs::write(&settings_path, b"{\"futureSetting\":true}").unwrap();
    agent
        .set_model_with_thinking(
            chat,
            selected.clone(),
            Arc::new(ScriptedModel::new(Vec::new())),
            Some(ReasoningEffort::High),
        )
        .await
        .unwrap();
    let saved = settings_json(&root);
    assert_eq!(saved["defaultModel"]["provider"], selected.provider_id);
    assert_eq!(saved["defaultModel"]["model"], selected.model_id);
    assert_eq!(saved["effortLevel"], "high");
    assert_eq!(saved["alwaysThinkingEnabled"], true);
    assert_eq!(saved["futureSetting"], true);

    let before_unsupported = std::fs::read(&settings_path).unwrap();
    assert!(
        agent
            .set_model_with_thinking(
                chat,
                selected.clone(),
                Arc::new(ScriptedModel::new(Vec::new())),
                Some(ReasoningEffort::Medium),
            )
            .await
            .is_err()
    );
    assert_eq!(std::fs::read(&settings_path).unwrap(), before_unsupported);
    assert_eq!(
        agent.snapshot(chat).await.unwrap().reasoning_effort,
        Some(ReasoningEffort::High)
    );

    agent
        .set_model_with_thinking(
            chat,
            selected.clone(),
            Arc::new(ScriptedModel::new(Vec::new())),
            Some(ReasoningEffort::Off),
        )
        .await
        .unwrap();
    assert_eq!(settings_json(&root)["effortLevel"], "off");
    assert_eq!(settings_json(&root)["alwaysThinkingEnabled"], false);
    let disabled = agent.snapshot(chat).await.unwrap();
    assert_eq!(disabled.reasoning_effort, None);
    assert!(!disabled.thinking_enabled);

    agent
        .set_model_with_thinking(
            chat,
            selected,
            Arc::new(ScriptedModel::new(Vec::new())),
            None,
        )
        .await
        .unwrap();
    let saved = settings_json(&root);
    assert!(saved.get("effortLevel").is_none());
    assert_eq!(saved["alwaysThinkingEnabled"], true);
    let automatic = agent.snapshot(chat).await.unwrap();
    assert_eq!(automatic.reasoning_effort, None);
    assert!(automatic.thinking_enabled);
    agent.shutdown().await;
}

fn system_prompt_of(request: &tickloop_agent_model::ModelRequest) -> String {
    request
        .messages
        .iter()
        .filter(|message| message.role == tickloop_agent_protocol::Role::System)
        .flat_map(|message| &message.content)
        .filter_map(|content| match content {
            Content::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

/// A saved `outputStyle` applies to subsequently opened Chats without changing existing ones.
#[tokio::test]
async fn a_saved_output_style_applies_to_chats_opened_afterwards() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let workdir = directory.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    let model = Arc::new(ScriptedModel::new(vec![
        text("existing"),
        text("opened after"),
    ]));
    let agent = Agent::open_embedded(root.clone(), config(), model.clone()).unwrap();
    let existing = agent.create_chat(workdir.clone()).await.unwrap();
    UserSettings::in_root(&root)
        .save_output_style(OutputStyle::Concise)
        .unwrap();
    let opened_after = agent.create_chat(workdir).await.unwrap();

    run_turn(&agent, existing, "hello", PermissionScope::Once).await;
    run_turn(&agent, opened_after, "hello", PermissionScope::Once).await;

    let requests = model.requests.lock().unwrap();
    assert!(!system_prompt_of(&requests[0]).contains("# Output Style:"));
    assert!(
        system_prompt_of(&requests[1]).contains("# Output Style: Concise\n# Concise Style Active")
    );
}

#[test]
fn unsupported_legacy_models_are_ignored_with_one_notice() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("settings.json"),
        json!({"model": {
            "name": "Old",
            "protocol": "messages",
            "base_url": "https://api.example.com/anthropic",
            "model": "old-model",
            "api_key_env": "OLD_API_KEY",
            "max_output_tokens": 2048
        }})
        .to_string(),
    )
    .unwrap();

    let mut loaded = UserSettings::in_root(directory.path()).load();
    assert!(loaded.configured_model().unwrap().is_none());
    assert_eq!(loaded.into_warnings().len(), 1);
}

fn rule(tool: &str, persistent: bool) -> PermissionRule {
    PermissionRule {
        id: Uuid::new_v4(),
        decision: Decision::Allow,
        tool: tool.into(),
        specifier: None,
        input: None,
        loop_id: None,
        persistent,
    }
}

/// 运行一轮对话；遇到权限请求时按 `scope` 批准。返回这一轮是否请求过权限。
async fn run_turn(agent: &Agent, chat: ChatId, prompt: &str, scope: PermissionScope) -> bool {
    let mut events = agent.subscribe();
    let receipt = agent.submit(chat, None, prompt.into()).await.unwrap();
    let mut asked = false;
    loop {
        let event = next_event(&mut events).await;
        if event.run_id != receipt.run_id {
            continue;
        }
        match event.kind {
            EventKind::PermissionRequested { request } => {
                asked = true;
                agent
                    .answer_permission_with_scope(chat, request.id, true, scope)
                    .await
                    .unwrap();
            }
            EventKind::Completed { .. } => return asked,
            EventKind::Failed(error) => panic!("turn failed: {error}"),
            _ => {}
        }
    }
}

fn tools_of(rules: &[PermissionRule]) -> Vec<String> {
    let mut tools = rules
        .iter()
        .map(|rule| rule.tool.clone())
        .collect::<Vec<_>>();
    tools.sort();
    tools
}

fn settings_json(root: &Path) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(root.join("settings.json")).unwrap()).unwrap()
}

/// Built-in model and persistent rules apply to new and resumed Chats
/// across restarts; session rules stay local, and protected tools still require admission.
#[tokio::test]
async fn builtin_model_and_rules_apply_after_restart_without_context_tuning() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap().join("root");
    let workdir = directory.path().join("work");
    let other_project = directory.path().join("other");
    std::fs::create_dir_all(&workdir).unwrap();
    std::fs::create_dir_all(&other_project).unwrap();
    tickloop_agent_model::credentials::save_api_key(
        &root,
        &selected_model().api_key_env,
        "fixture-settings-key",
    )
    .unwrap();
    let shell = json!({"command":"env sh -c 'printf persisted'"});
    let first = Agent::open_embedded(
        root.clone(),
        config(),
        Arc::new(ScriptedModel::new(Vec::new())),
    )
    .unwrap();
    let chat = first.create_chat(workdir.clone()).await.unwrap();
    let selected_client = Arc::new(ScriptedModel::new(vec![
        call("persist", "Bash", shell.clone()),
        text("done"),
    ]));
    assert_eq!(
        first
            .set_model(chat, selected_model(), selected_client)
            .await
            .unwrap(),
        ModelChange::Applied
    );
    assert!(first.set_context_window(chat, 64_000).await.is_err());
    first
        .set_permission_rules(chat, vec![rule("Read", true), rule("Glob", false)])
        .await
        .unwrap();
    assert!(run_turn(&first, chat, "Run the command", PermissionScope::Persistent).await);

    let fresh = first.create_chat(workdir.clone()).await.unwrap();
    let fresh = first.snapshot(fresh).await.unwrap();
    assert_eq!(fresh.model, selected_model());
    assert_eq!(fresh.context_window_tokens, None);
    assert_eq!(tools_of(&fresh.policy.rules), ["Bash", "Read"]);
    first.shutdown().await;

    let settings = UserSettings::in_root(&root);
    assert_eq!(settings.model(), Some(selected_model()));
    assert_eq!(settings.context_window_tokens(), None);
    assert_eq!(
        tools_of(&settings.permission_rules(&workdir.canonicalize().unwrap())),
        ["Bash", "Read"]
    );

    let second = Agent::open_embedded(
        root.clone(),
        settings.model().unwrap(),
        Arc::new(ScriptedModel::new(vec![
            call("persist-again", "Bash", shell.clone()),
            text("done again"),
        ])),
    )
    .unwrap();
    let restarted = second.create_chat(workdir.clone()).await.unwrap();
    let snapshot = second.snapshot(restarted).await.unwrap();
    assert_eq!(snapshot.model, selected_model());
    assert_eq!(snapshot.context_window_tokens, None);
    assert_eq!(tools_of(&snapshot.policy.rules), ["Bash", "Read"]);
    assert!(
        !run_turn(&second, restarted, "Run it again", PermissionScope::Once).await,
        "the persisted exact Shell rule did not apply after restart"
    );

    let resumed = second.resume_chat(chat).await.unwrap();
    assert_eq!(resumed.model, selected_model());
    assert_eq!(resumed.context_window_tokens, None);
    assert_eq!(tools_of(&resumed.policy.rules), ["Bash", "Read"]);

    let elsewhere = second.create_chat(other_project).await.unwrap();
    assert!(
        second
            .snapshot(elsewhere)
            .await
            .unwrap()
            .policy
            .rules
            .is_empty(),
        "persistent rules belong to the project they were added in"
    );

    let kept = snapshot
        .policy
        .rules
        .into_iter()
        .filter(|rule| rule.tool != "Read")
        .collect::<Vec<_>>();
    second.set_permission_rules(restarted, kept).await.unwrap();
    assert_eq!(
        tools_of(&settings.permission_rules(&workdir.canonicalize().unwrap())),
        ["Bash"]
    );
    second.shutdown().await;
}

#[tokio::test]
async fn a_saved_shell_rule_applies_immediately_to_the_current_session() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let workdir = directory.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    let input = json!({"command":"env sh -c 'printf allowed >> trace.txt'"});
    let model = Arc::new(ScriptedModel::new(vec![
        call("first", "Bash", input.clone()),
        text("first done"),
        call("second", "Bash", input.clone()),
        text("second done"),
    ]));
    let agent = Agent::open_embedded(root.clone(), config(), model).unwrap();
    agent.install_own_tickhost();
    let first = agent.create_chat(workdir.clone()).await.unwrap();
    let second = agent.create_chat(workdir.clone()).await.unwrap();

    let mut events = agent.subscribe();
    let first_run = agent
        .submit(first, None, "Run the exact command".into())
        .await
        .unwrap();
    loop {
        let event = next_event(&mut events).await;
        if event.run_id != first_run.run_id {
            continue;
        }
        match event.kind {
            EventKind::PermissionRequested { request } => {
                let exact = permission_rule_candidates(&request.name, &request.input)
                    .into_iter()
                    .next()
                    .unwrap();
                agent
                    .answer_permission_with_rule(first, request.id, exact.specifier)
                    .await
                    .unwrap();
            }
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("first command failed: {error}"),
            _ => {}
        }
    }

    let second_run = agent
        .submit(second, None, "Run the exact command again".into())
        .await
        .unwrap();
    loop {
        let event = next_event(&mut events).await;
        if event.run_id != second_run.run_id {
            continue;
        }
        match event.kind {
            EventKind::PermissionRequested { .. } => {
                panic!("the saved rule did not reach the current session")
            }
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("second command failed: {error}"),
            _ => {}
        }
    }

    assert_eq!(
        std::fs::read_to_string(workdir.join("trace.txt")).unwrap(),
        "allowedallowed"
    );
    let saved = UserSettings::in_root(&root).permission_rules(&workdir.canonicalize().unwrap());
    assert_eq!(saved.len(), 1);
    assert_eq!(
        saved[0].specifier.as_deref(),
        Some("env sh -c 'printf allowed >> trace.txt'")
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn a_saved_shell_prefix_rule_applies_in_current_and_future_sessions() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let workdir = directory.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    std::fs::write(workdir.join("source.txt"), "first\nsecond\n").unwrap();
    let first_input = json!({"command":"head -n 1 source.txt"});
    let next_input = json!({"command":"head -n 2 source.txt"});
    let model = Arc::new(ScriptedModel::new(vec![
        call("first", "Bash", first_input),
        text("first done"),
        call("second", "Bash", next_input.clone()),
        text("second done"),
    ]));
    let agent = Agent::open_embedded(root.clone(), config(), model).unwrap();
    let chat = agent.create_chat(workdir.clone()).await.unwrap();
    let mut events = agent.subscribe();

    let first_run = agent
        .submit(chat, None, "Read one line".into())
        .await
        .unwrap();
    loop {
        let event = next_event(&mut events).await;
        if event.run_id != first_run.run_id {
            continue;
        }
        match event.kind {
            EventKind::PermissionRequested { request } => {
                let prefix = permission_rule_candidates(&request.name, &request.input)
                    .into_iter()
                    .last()
                    .unwrap();
                assert_eq!(prefix.specifier, "head *");
                agent
                    .answer_permission_with_rule(chat, request.id, prefix.specifier)
                    .await
                    .unwrap();
            }
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("first command failed: {error}"),
            _ => {}
        }
    }

    let current_session = agent
        .submit(chat, None, "Read two lines".into())
        .await
        .unwrap();
    loop {
        let event = next_event(&mut events).await;
        if event.run_id != current_session.run_id {
            continue;
        }
        match event.kind {
            EventKind::PermissionRequested { .. } => {
                panic!("head * did not apply in the current session")
            }
            EventKind::Completed { .. } => break,
            EventKind::Failed(error) => panic!("current-session command failed: {error}"),
            _ => {}
        }
    }
    agent.shutdown().await;

    let restarted_model = Arc::new(ScriptedModel::new(vec![
        call("third", "Bash", next_input),
        text("third done"),
    ]));
    let restarted = Agent::open_embedded(root, config(), restarted_model).unwrap();
    let restarted_chat = restarted.create_chat(workdir).await.unwrap();
    assert!(
        !run_turn(
            &restarted,
            restarted_chat,
            "Read two lines after restart",
            PermissionScope::Once,
        )
        .await,
        "head * did not apply in a future session"
    );
    restarted.shutdown().await;
}

/// 启动参数指定的模型优先于设置文件：Agent 使用调用方给出的配置创建新 Chat。
#[tokio::test]
async fn an_explicit_host_model_takes_precedence_over_the_saved_selection() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let workdir = directory.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    let first = Agent::open_embedded(
        root.clone(),
        config(),
        Arc::new(ScriptedModel::new(Vec::new())),
    )
    .unwrap();
    let chat = first.create_chat(workdir.clone()).await.unwrap();
    first
        .set_model(
            chat,
            selected_model(),
            Arc::new(ScriptedModel::new(Vec::new())),
        )
        .await
        .unwrap();
    first.shutdown().await;
    assert_eq!(UserSettings::in_root(&root).model(), Some(selected_model()));

    let explicit = model_for_persistent_selection(ModelConfig {
        provider: tickloop_agent_protocol::ModelProvider::Api,
        auth_source: tickloop_agent_protocol::ModelAuthSource::ApiKeyEnv,
        name: "explicit".into(),
        model: "explicit-model".into(),
        ..config()
    });
    let second = Agent::open_embedded(
        root.clone(),
        explicit.clone(),
        Arc::new(ScriptedModel::new(Vec::new())),
    )
    .unwrap();
    let chat = second.create_chat(workdir).await.unwrap();
    assert_eq!(second.snapshot(chat).await.unwrap().model, explicit);
    assert_eq!(
        UserSettings::in_root(&root).model(),
        Some(selected_model()),
        "opening with an explicit model must not overwrite the saved selection"
    );
}

/// 照 Claude Code `updateSettingsForSource`：写入时读取现有文件并合并，保留其他键；
/// 用临时文件加重命名原子替换；文件不是合法 JSON 时报错且不覆盖。
#[tokio::test]
async fn settings_writes_merge_unknown_keys_replace_atomically_and_refuse_invalid_json() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let workdir = directory.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("settings.json");
    std::fs::write(
        &path,
        serde_json::to_vec_pretty(&json!({
            "cleanupPeriodDays": 7,
            "projects": {"/elsewhere": {"note": "keep", "permissions": {"rules": []}}},
        }))
        .unwrap(),
    )
    .unwrap();
    let first = Agent::open_embedded(
        root.clone(),
        config(),
        Arc::new(ScriptedModel::new(Vec::new())),
    )
    .unwrap();
    let second = Agent::open_embedded(
        root.clone(),
        config(),
        Arc::new(ScriptedModel::new(Vec::new())),
    )
    .unwrap();
    let first_chat = first.create_chat(workdir.clone()).await.unwrap();
    let second_chat = second.create_chat(workdir.clone()).await.unwrap();

    let inode = std::fs::metadata(&path).unwrap().ino();
    first
        .set_model(
            first_chat,
            selected_model(),
            Arc::new(ScriptedModel::new(Vec::new())),
        )
        .await
        .unwrap();
    assert_ne!(
        std::fs::metadata(&path).unwrap().ino(),
        inode,
        "settings must be replaced by renaming a complete temporary file"
    );
    assert!(
        second
            .set_context_window(second_chat, 32_000)
            .await
            .is_err()
    );
    second
        .set_permission_rules(second_chat, vec![rule("Read", true)])
        .await
        .unwrap();

    let saved = settings_json(&root);
    assert_eq!(saved["cleanupPeriodDays"], 7);
    assert_eq!(saved["projects"]["/elsewhere"]["note"], "keep");
    assert!(saved.get("contextWindowTokens").is_none());
    let selected = selected_model();
    assert_eq!(saved["defaultModel"]["provider"], selected.provider_id);
    assert_eq!(saved["defaultModel"]["model"], selected.model_id);
    assert!(saved.get("model").is_none());
    assert!(!root.join("providers").exists());
    let settings = UserSettings::in_root(&root);
    assert_eq!(settings.model(), Some(selected_model()));
    assert_eq!(
        tools_of(&settings.permission_rules(&workdir.canonicalize().unwrap())),
        ["Read"]
    );
    let leftovers = std::fs::read_dir(&root)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        // `settings.json.lock` 是跨进程写入锁，保留在原处。
        .filter(|name| {
            name.starts_with("settings") && name != "settings.json" && name != "settings.json.lock"
        })
        .collect::<Vec<_>>();
    assert!(leftovers.is_empty(), "{leftovers:?}");

    std::fs::write(&path, "{not json").unwrap();
    assert!(
        first.set_context_window(first_chat, 48_000).await.is_err(),
        "an unreadable settings file must not be overwritten"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "{not json");
    assert_eq!(
        first
            .snapshot(first_chat)
            .await
            .unwrap()
            .context_window_tokens,
        None
    );
}

fn persistent_rule(tool: &str, decision: Decision) -> PermissionRule {
    PermissionRule {
        decision,
        ..rule(tool, true)
    }
}

fn has_rule(rules: &[PermissionRule], id: Uuid) -> bool {
    rules.iter().any(|rule| rule.id == id)
}

async fn rules_of(agent: &Agent, chat: ChatId) -> Vec<PermissionRule> {
    agent.snapshot(chat).await.unwrap().policy.rules
}

/// 持久规则属于添加时的项目分区：`/cwd` 换目录后旧目录的规则不再生效、新目录的规则载入，
/// 会话规则保留；在新目录中的删除不影响旧目录，回到旧目录后删除写回旧目录的分区。
#[tokio::test]
async fn persistent_rules_follow_the_working_directory_partition() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let project_a = directory.path().join("a");
    let project_b = directory.path().join("b");
    std::fs::create_dir_all(&project_a).unwrap();
    std::fs::create_dir_all(&project_b).unwrap();
    let project_a = project_a.canonicalize().unwrap();
    let project_b = project_b.canonicalize().unwrap();
    let agent = Agent::open_embedded(
        root.clone(),
        config(),
        Arc::new(ScriptedModel::new(Vec::new())),
    )
    .unwrap();
    let chat = agent.create_chat(project_a.clone()).await.unwrap();
    let allow_in_a = persistent_rule("Read", Decision::Allow);
    let session = rule("Glob", false);
    agent
        .set_permission_rules(chat, vec![allow_in_a.clone(), session.clone()])
        .await
        .unwrap();
    let settings = UserSettings::in_root(&root);
    let revision = agent.snapshot(chat).await.unwrap().policy.revision;

    agent
        .set_directories(chat, project_b.clone(), Vec::new())
        .await
        .unwrap();
    let in_b = agent.snapshot(chat).await.unwrap();
    assert!(
        !has_rule(&in_b.policy.rules, allow_in_a.id),
        "a persistent rule of project A must not apply in project B"
    );
    assert!(has_rule(&in_b.policy.rules, session.id));
    assert!(in_b.policy.revision > revision);

    let without_a_rule = in_b
        .policy
        .rules
        .iter()
        .filter(|rule| rule.id != allow_in_a.id)
        .cloned()
        .collect::<Vec<_>>();
    agent
        .set_permission_rules(chat, without_a_rule)
        .await
        .unwrap();
    assert!(has_rule(
        &settings.permission_rules(&project_a),
        allow_in_a.id
    ));
    assert!(settings.permission_rules(&project_b).is_empty());

    agent
        .set_directories(chat, project_a.clone(), Vec::new())
        .await
        .unwrap();
    let back_in_a = rules_of(&agent, chat).await;
    assert!(has_rule(&back_in_a, allow_in_a.id));
    assert!(has_rule(&back_in_a, session.id));
    let deleted = back_in_a
        .into_iter()
        .filter(|rule| rule.id != allow_in_a.id)
        .collect::<Vec<_>>();
    agent.set_permission_rules(chat, deleted).await.unwrap();
    assert!(settings.permission_rules(&project_a).is_empty());
    let fresh = agent.create_chat(project_a.clone()).await.unwrap();
    assert!(!has_rule(&rules_of(&agent, fresh).await, allow_in_a.id));
    agent.shutdown().await;
}

/// 持久规则按完整内容比较：同 id 由 Allow 改为 Deny 视为删旧加新，重启后仍为 Deny。
#[tokio::test]
async fn changing_a_persistent_rule_in_place_is_saved() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let workdir = directory.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    let first = Agent::open_embedded(
        root.clone(),
        config(),
        Arc::new(ScriptedModel::new(Vec::new())),
    )
    .unwrap();
    let chat = first.create_chat(workdir.clone()).await.unwrap();
    let allowed = persistent_rule("Bash", Decision::Allow);
    first
        .set_permission_rules(chat, vec![allowed.clone()])
        .await
        .unwrap();
    let denied = PermissionRule {
        decision: Decision::Deny,
        input: Some(json!({"command":"rm -rf /"})),
        ..allowed.clone()
    };
    first
        .set_permission_rules(chat, vec![denied.clone()])
        .await
        .unwrap();
    first.shutdown().await;

    let second = Agent::open_embedded(
        root.clone(),
        config(),
        Arc::new(ScriptedModel::new(Vec::new())),
    )
    .unwrap();
    let restarted = second.create_chat(workdir).await.unwrap();
    let rules = rules_of(&second, restarted).await;
    assert_eq!(rules.len(), 1, "{rules:?}");
    assert_eq!(rules[0].id, allowed.id);
    assert_eq!(rules[0].decision, Decision::Deny);
    assert_eq!(rules[0].input, denied.input);
    second.shutdown().await;
}

/// 本进程已加载的同项目 Chat 立即得到持久规则的增删改；其他项目的 Chat 不受影响。
#[tokio::test]
async fn persistent_rule_changes_reach_other_loaded_chats_of_the_project() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let workdir = directory.path().join("work");
    let elsewhere = directory.path().join("elsewhere");
    std::fs::create_dir_all(&workdir).unwrap();
    std::fs::create_dir_all(&elsewhere).unwrap();
    let agent = Agent::open_embedded(
        root,
        config(),
        Arc::new(ScriptedModel::new(vec![
            call("denied", "Bash", json!({"command":"echo denied"})),
            text("done"),
        ])),
    )
    .unwrap();
    let editing = agent.create_chat(workdir.clone()).await.unwrap();
    let observing = agent.create_chat(workdir.clone()).await.unwrap();
    let unrelated = agent.create_chat(elsewhere).await.unwrap();
    let observed_revision = agent.snapshot(observing).await.unwrap().policy.revision;
    let deny = persistent_rule("Bash", Decision::Deny);
    agent
        .set_permission_rules(editing, vec![deny.clone()])
        .await
        .unwrap();

    let observed = agent.snapshot(observing).await.unwrap();
    assert!(has_rule(&observed.policy.rules, deny.id));
    assert!(observed.policy.revision > observed_revision);
    assert!(!has_rule(&rules_of(&agent, unrelated).await, deny.id));

    let mut events = agent.subscribe();
    let receipt = agent
        .submit(observing, None, "Run the command".into())
        .await
        .unwrap();
    let mut denial_returned = false;
    loop {
        let event = next_event(&mut events).await;
        if event.run_id != receipt.run_id {
            continue;
        }
        match event.kind {
            EventKind::ToolCompleted { result, .. } => {
                assert!(result.is_error, "{result:?}");
                assert!(result.content.contains("denied"), "{}", result.content);
                denial_returned = true;
            }
            EventKind::PermissionRequested { .. } | EventKind::ToolStarted { .. } => {
                panic!("the persistent deny added in another Chat was bypassed")
            }
            EventKind::Completed { .. } => {
                assert!(
                    denial_returned,
                    "the persistent deny never reached the model"
                );
                break;
            }
            EventKind::Failed(reason) => panic!("the denied run failed: {reason}"),
            _ => {}
        }
    }

    let changed = PermissionRule {
        decision: Decision::Ask,
        ..deny.clone()
    };
    agent
        .set_permission_rules(editing, vec![changed])
        .await
        .unwrap();
    let observed = rules_of(&agent, observing).await;
    assert_eq!(observed.len(), 1);
    assert_eq!(observed[0].decision, Decision::Ask);
    agent
        .set_permission_rules(editing, Vec::new())
        .await
        .unwrap();
    assert!(rules_of(&agent, observing).await.is_empty());
    agent.shutdown().await;
}

/// 两个实例交错增删各自的持久规则：读-改-写在 `settings.json.lock` 的独占锁内完成，
/// 已删除的规则不会被另一个实例基于旧内容的写入复活，也不会丢失对方的新增。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_instances_never_resurrect_or_lose_persistent_rules() {
    const ROUNDS: usize = 40;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let workdir = directory.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    let model = Arc::new(ScriptedModel::new(Vec::new()));
    let mut agents = Vec::new();
    for tool in ["Read", "Glob"] {
        let agent = Agent::open_embedded(root.clone(), config(), model.clone()).unwrap();
        let chat = agent.create_chat(workdir.clone()).await.unwrap();
        assert!(agent.snapshot(chat).await.unwrap().policy.rules.is_empty());
        agents.push((tool, agent, chat));
    }
    let start = Arc::new(Barrier::new(3));
    let mut writers = Vec::new();
    for (tool, agent, chat) in agents {
        let start = start.clone();
        writers.push(tokio::spawn(async move {
            start.wait().await;
            let kept = persistent_rule(&format!("{tool}_kept"), Decision::Allow);
            for _ in 0..ROUNDS {
                let transient = persistent_rule(tool, Decision::Deny);
                agent
                    .set_permission_rules(chat, vec![kept.clone(), transient])
                    .await
                    .unwrap();
                agent
                    .set_permission_rules(chat, vec![kept.clone()])
                    .await
                    .unwrap();
            }
            agent.shutdown().await;
            kept.id
        }));
    }
    start.wait().await;
    let mut kept = Vec::new();
    for writer in writers {
        kept.push(writer.await.unwrap());
    }
    let saved = UserSettings::in_root(&root).permission_rules(&workdir.canonicalize().unwrap());
    let mut saved_ids = saved.iter().map(|rule| rule.id).collect::<Vec<_>>();
    saved_ids.sort();
    kept.sort();
    assert_eq!(saved_ids, kept, "{saved:?}");
    assert!(model.requests.lock().unwrap().is_empty());
}

/// 设置文件无效时不静默丢弃：新建与恢复 Chat 都能读到警告，写入报错且文件保持原样；
/// 单条无效规则被跳过并报告；嵌套键类型错误时写入报错，不覆盖其他内容。
#[tokio::test]
async fn invalid_settings_are_reported_and_never_overwritten() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let workdir = directory.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    let workdir = workdir.canonicalize().unwrap();
    let path = root.join("settings.json");

    let valid = rule("Read", true);
    let project = workdir.to_string_lossy().into_owned();
    let with_bad_rule = json!({
        "projects": {project.clone(): {"permissions": {"rules": [
            {"id": valid.id, "decision": "allow", "tool": "Read"},
            {"id": "not-a-uuid", "decision": "deny", "tool": "Bash"},
        ]}}},
    });
    std::fs::write(&path, serde_json::to_vec_pretty(&with_bad_rule).unwrap()).unwrap();
    let agent = Agent::open_embedded(
        root.clone(),
        config(),
        Arc::new(ScriptedModel::new(vec![text("saved")])),
    )
    .unwrap();
    let chat = agent.create_chat(workdir.clone()).await.unwrap();
    let rules = rules_of(&agent, chat).await;
    assert_eq!(tools_of(&rules), ["Read"]);
    let warnings = agent.user_settings_warnings(chat);
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("rules[1]"), "{warnings:?}");
    assert!(!run_turn(&agent, chat, "Save this Chat", PermissionScope::Once).await);
    agent.shutdown().await;

    let trailing_comma = "{\n  \"contextWindowTokens\": 64000,\n}\n";
    std::fs::write(&path, trailing_comma).unwrap();
    let reopened = Agent::open_embedded(
        root.clone(),
        config(),
        Arc::new(ScriptedModel::new(Vec::new())),
    )
    .unwrap();
    let resumed = reopened.resume_chat(chat).await.unwrap();
    assert!(resumed.policy.rules.is_empty());
    let warnings = reopened.user_settings_warnings(chat);
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("not valid JSON"), "{warnings:?}");
    let fresh = reopened.create_chat(workdir.clone()).await.unwrap();
    assert_eq!(reopened.user_settings_warnings(fresh), warnings);
    assert!(reopened.set_context_window(chat, 32_000).await.is_err());
    assert!(
        reopened
            .set_permission_rules(chat, vec![rule("Glob", true)])
            .await
            .is_err()
    );
    assert!(
        reopened
            .set_model(
                chat,
                selected_model(),
                Arc::new(ScriptedModel::new(Vec::new()))
            )
            .await
            .is_err()
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), trailing_comma);
    assert!(rules_of(&reopened, chat).await.is_empty());

    let wrong_nesting = serde_json::to_string_pretty(&json!({
        "projects": {project: {"permissions": "not an object"}, "/elsewhere": {"note": "keep"}},
    }))
    .unwrap();
    std::fs::write(&path, &wrong_nesting).unwrap();
    let nested = reopened.create_chat(workdir.clone()).await.unwrap();
    let warnings = reopened.user_settings_warnings(nested);
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("permissions"), "{warnings:?}");
    assert!(
        reopened
            .set_permission_rules(nested, vec![rule("Glob", true)])
            .await
            .is_err()
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), wrong_nesting);
    assert!(reopened.set_context_window(nested, 32_000).await.is_err());
    UserSettings::in_root(&root)
        .save_thinking_enabled(false)
        .unwrap();
    let saved = settings_json(&root);
    assert_eq!(saved["projects"]["/elsewhere"]["note"], "keep");
    assert!(saved.get("contextWindowTokens").is_none());
    reopened.shutdown().await;
}

/// 照 Claude Code `writeFileSyncAndFlush_DEPRECATED`：设置文件是符号链接时写入链接目标，
/// 临时文件在目标目录中创建，链接本身保留。
#[tokio::test]
async fn settings_writes_follow_a_symlinked_settings_file() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let dotfiles = directory.path().join("dotfiles");
    let workdir = directory.path().join("work");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&dotfiles).unwrap();
    std::fs::create_dir_all(&workdir).unwrap();
    let target = dotfiles.join("tickloop-settings.json");
    std::fs::write(&target, "{\"cleanupPeriodDays\": 7}").unwrap();
    std::os::unix::fs::symlink(
        "../dotfiles/tickloop-settings.json",
        root.join("settings.json"),
    )
    .unwrap();
    let agent = Agent::open_embedded(
        root.clone(),
        config(),
        Arc::new(ScriptedModel::new(Vec::new())),
    )
    .unwrap();
    let chat = agent.create_chat(workdir).await.unwrap();
    assert!(agent.set_context_window(chat, 32_000).await.is_err());
    UserSettings::in_root(&root)
        .save_thinking_enabled(false)
        .unwrap();

    let link = std::fs::symlink_metadata(root.join("settings.json")).unwrap();
    assert!(link.file_type().is_symlink(), "the symlink must be kept");
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&target).unwrap()).unwrap();
    assert_eq!(saved["cleanupPeriodDays"], 7);
    assert!(saved.get("contextWindowTokens").is_none());
    for folder in [&root, &dotfiles] {
        let leftovers = std::fs::read_dir(folder)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect::<Vec<_>>();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }
    agent.shutdown().await;
}

fn thinking_reply(signature: &str) -> Vec<Content> {
    vec![
        Content::Thinking {
            thinking: format!("reasoning bound to {signature}"),
            signature: Some(signature.into()),
        },
        Content::Text {
            text: "Public answer".into(),
        },
    ]
}

fn requests_contain_private_reasoning(model: &ScriptedModel) -> bool {
    model
        .requests
        .lock()
        .unwrap()
        .last()
        .expect("the model was asked")
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .any(|content| {
            matches!(
                content,
                Content::Thinking { .. } | Content::ProtocolData { .. }
            )
        })
}

/// Raw provenance survives restart; the concrete HTTP client verifies actual credentials.
#[tokio::test]
async fn resuming_retains_original_reasoning_and_projects_different_model_sources() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let workdir = directory.path().join("work");
    std::fs::create_dir_all(&workdir).unwrap();
    let service_x = ModelConfig {
        provider: tickloop_agent_protocol::ModelProvider::Api,
        auth_source: tickloop_agent_protocol::ModelAuthSource::ApiKeyEnv,
        name: "service-x".into(),
        base_url: "http://127.0.0.1/x".into(),
        model: "x-model".into(),
        ..config()
    };
    let first = Agent::open_embedded(
        root.clone(),
        service_x.clone(),
        Arc::new(ScriptedModel::new(vec![thinking_reply("x-signature")])),
    )
    .unwrap();
    let chat = first.create_chat(workdir.clone()).await.unwrap();
    run_turn(&first, chat, "Think about it", PermissionScope::Once).await;
    first.shutdown().await;
    let transcript =
        std::fs::read_to_string(tickloop_agent_test_support::transcript_file(&root, chat)).unwrap();
    let assistant = transcript
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .find(|line| line["type"] == "assistant")
        .unwrap();
    assert_eq!(assistant["model"]["profile"], "service-x");
    assert_eq!(assistant["model"]["requested_model"], "x-model");

    let same = Arc::new(ScriptedModel::new(vec![text("same model")]));
    let second = Agent::open_embedded(root.clone(), service_x, same.clone()).unwrap();
    second.resume_chat(chat).await.unwrap();
    run_turn(&second, chat, "Continue", PermissionScope::Once).await;
    assert!(
        requests_contain_private_reasoning(&same),
        "the in-memory client reuses its matching model source after restart"
    );
    second.shutdown().await;

    let service_y = ModelConfig {
        provider: tickloop_agent_protocol::ModelProvider::Api,
        auth_source: tickloop_agent_protocol::ModelAuthSource::ApiKeyEnv,
        name: "service-y".into(),
        base_url: "http://127.0.0.1/y".into(),
        model: "y-model".into(),
        ..config()
    };
    let other = Arc::new(ScriptedModel::new(vec![text("other model")]));
    let third = Agent::open_embedded(root.clone(), service_y, other.clone()).unwrap();
    let resumed = third.resume_chat(chat).await.unwrap();
    assert!(
        resumed
            .messages
            .iter()
            .any(|message| message.content.iter().any(
                |content| matches!(content, Content::Text { text } if text == "Public answer")
            )),
        "public content stays"
    );
    run_turn(&third, chat, "Continue elsewhere", PermissionScope::Once).await;
    assert!(
        !requests_contain_private_reasoning(&other),
        "signatures from another service must not be sent"
    );
    third.shutdown().await;
}

#[test]
fn monitor_preference_preserves_unknown_settings_and_defaults_invalid_values() {
    use tickloop_agent_config::MonitorPreference;
    let directory = tempfile::tempdir().unwrap();
    let settings = UserSettings::in_root(directory.path());
    std::fs::write(
        directory.path().join("settings.json"),
        r#"{"monitor":"invalid","futureKey":{"keep":true},"statusLinePosition":"left"}"#,
    )
    .unwrap();
    assert_eq!(
        settings.load().monitor_preference(),
        MonitorPreference::Auto
    );
    settings
        .save_monitor_preference(MonitorPreference::Hidden)
        .unwrap();
    assert_eq!(
        settings.load().monitor_preference(),
        MonitorPreference::Hidden
    );
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(directory.path().join("settings.json")).unwrap())
            .unwrap();
    assert_eq!(saved["futureKey"], json!({"keep":true}));
    assert_eq!(saved["statusLinePosition"], "left");
    assert_eq!(saved["monitor"], "hidden");
}

#[test]
fn the_former_tick_list_setting_is_neither_read_nor_migrated() {
    use tickloop_agent_config::MonitorPreference;
    let directory = tempfile::tempdir().unwrap();
    let settings = UserSettings::in_root(directory.path());
    std::fs::write(
        directory.path().join("settings.json"),
        r#"{"tickList":"hidden","keep":1}"#,
    )
    .unwrap();
    assert_eq!(
        settings.load().monitor_preference(),
        MonitorPreference::Auto,
        "only the monitor key controls Monitor"
    );
    settings
        .save_monitor_preference(MonitorPreference::Auto)
        .unwrap();
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(directory.path().join("settings.json")).unwrap())
            .unwrap();
    assert_eq!(saved["monitor"], "auto");
    assert_eq!(saved["keep"], 1);
    assert_eq!(
        saved["tickList"], "hidden",
        "saving never rewrites or migrates other keys"
    );
}

/// An invalid `cleanupPeriodDays` is reported like any other settings problem and keeps Chats saved.
#[tokio::test]
async fn an_invalid_cleanup_period_is_reported_as_a_settings_warning() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    let workdir = directory.path().join("work");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&workdir).unwrap();
    std::fs::write(root.join("settings.json"), r#"{"cleanupPeriodDays": -1}"#).unwrap();
    let agent =
        Agent::open_embedded(root, config(), Arc::new(ScriptedModel::new(Vec::new()))).unwrap();

    let chat = agent.create_chat(workdir).await.unwrap();

    let warnings = agent.user_settings_warnings(chat);
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("cleanupPeriodDays"), "{warnings:?}");
    assert!(agent.saves_chats());
}

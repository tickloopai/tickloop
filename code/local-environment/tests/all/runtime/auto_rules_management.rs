use std::sync::Arc;
use tickloop_code_settings::local_agent::LocalAgent;
use tickloop_harness::test_support::ScriptedModel;

use tickloop_harness::protocol::{
    AutoEnvironmentSource, AutoRuleChange, AutoRuleKind, AutoRuleSource, PermissionChange,
};
use tickloop_harness::runtime::{Agent, PermissionChangeError};

#[tokio::test]
async fn imported_auto_draft_cancels_without_writes_and_saves_atomically_after_conflict() {
    use tickloop_code_settings::claude_auto_import::{prepare_import, preview_auto_mode};
    use tickloop_harness::protocol::AutoEnvironmentEntrySource;

    let temp = tempfile::tempdir().unwrap();
    let agent = Agent::open_embedded(
        temp.path().join("state"),
        tickloop_harness::test_support::config(),
        Arc::new(ScriptedModel::new(Vec::new())),
    )
    .unwrap();
    let chat = agent.create_chat(temp.path().into()).await.unwrap();
    let initial = agent.permissions_view(chat).await.unwrap();
    let source = temp.path().join("claude/settings.json");
    let auto_mode = serde_json::json!({
        "allow": ["Allow fixture cleanup"],
        "soft_deny": ["Keep release tags"],
        "environment": ["Private research workstation"]
    });
    let preview = preview_auto_mode(&auto_mode, &initial.auto_rule_settings).unwrap();
    drop(prepare_import(&preview, &source, &initial.auto_rule_settings).unwrap());
    let cancelled = agent.permissions_view(chat).await.unwrap();
    assert_eq!(cancelled.auto_rule_settings, initial.auto_rule_settings);
    assert_eq!(cancelled.revision, initial.revision);

    let other = agent
        .apply_permission_change(
            chat,
            PermissionChange::ChangeAutoRules {
                change: AutoRuleChange::Add {
                    kind: AutoRuleKind::SoftDeny,
                    text: "Keep release tags".into(),
                },
            },
            initial.revision.clone(),
        )
        .await
        .unwrap();
    let stale = prepare_import(&preview, &source, &initial.auto_rule_settings).unwrap();
    let Err(PermissionChangeError::Conflict { latest }) = agent
        .apply_permission_change(
            chat,
            PermissionChange::ChangeAutoRules { change: stale },
            initial.revision,
        )
        .await
    else {
        panic!("stale import must conflict");
    };
    assert_eq!(latest.auto_rule_settings, other.auto_rule_settings);
    let refreshed = preview_auto_mode(&auto_mode, &latest.auto_rule_settings).unwrap();
    assert!(!refreshed[1].selected);
    let saved = agent
        .apply_permission_change(
            chat,
            PermissionChange::ChangeAutoRules {
                change: prepare_import(&refreshed, &source, &latest.auto_rule_settings).unwrap(),
            },
            latest.revision,
        )
        .await
        .unwrap();
    assert_eq!(saved.auto_policy.rules.len(), 84);
    assert_eq!(
        saved.auto_rule_settings.soft_allow.custom[0].text,
        "Allow fixture cleanup"
    );
    assert_eq!(saved.auto_rule_settings.soft_deny.custom.len(), 1);
    assert_eq!(
        saved.auto_rule_settings.hard_deny,
        initial.auto_rule_settings.hard_deny
    );
    assert!(saved.auto_rule_settings.soft_allow.include_builtins);
    assert!(saved.auto_rule_settings.soft_deny.include_builtins);
    let entry = &saved.auto_rule_settings.environment_entries[0];
    assert!(entry.directory.is_none());
    assert!(!entry.include_descendants);
    assert!(
        matches!(&entry.source, AutoEnvironmentEntrySource::Imported { file: Some(file), imported_at_ms: Some(time) } if file == &source && *time > 0)
    );
    let settings = tickloop_code_settings::UserSettings::in_root(&temp.path().join("state"));
    assert_eq!(
        settings.auto_settings_with_revision().unwrap().0.rules,
        saved.auto_rule_settings
    );
}

#[tokio::test]
async fn auto_rule_groups_and_environment_save_with_revision_and_restore_independently() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("state");
    let agent = Agent::open_embedded(
        root,
        tickloop_harness::test_support::config(),
        Arc::new(ScriptedModel::new(Vec::new())),
    )
    .unwrap();
    let chat = agent.create_chat(temp.path().into()).await.unwrap();
    let initial = agent.permissions_view(chat).await.unwrap();
    assert_eq!(initial.auto_policy.rules.len(), 82);
    assert!(
        initial
            .auto_policy
            .rules
            .iter()
            .filter(|rule| rule.kind == AutoRuleKind::SoftDeny)
            .count()
            == 65
    );
    assert!(matches!(
        initial.auto_policy.environment.source,
        AutoEnvironmentSource::Builtin { version: 10 }
    ));

    let added = agent
        .apply_permission_change(
            chat,
            PermissionChange::ChangeAutoRules {
                change: AutoRuleChange::Add {
                    kind: AutoRuleKind::HardDeny,
                    text: "Never transfer account funds automatically".into(),
                },
            },
            initial.revision.clone(),
        )
        .await
        .unwrap();
    let custom = added
        .auto_policy
        .rules
        .iter()
        .find(|rule| rule.kind == AutoRuleKind::HardDeny && rule.source == AutoRuleSource::User)
        .unwrap();
    assert!(matches!(custom.source, AutoRuleSource::User));
    assert_eq!(added.auto_policy.rules.len(), 83);
    assert!(matches!(
        agent
            .apply_permission_change(
                chat,
                PermissionChange::ChangeAutoRules {
                    change: AutoRuleChange::ReplaceEnvironment {
                        text: "Private research workstation".into()
                    }
                },
                initial.revision,
            )
            .await,
        Err(PermissionChangeError::Conflict { .. })
    ));

    let replaced = agent
        .apply_permission_change(
            chat,
            PermissionChange::ChangeAutoRules {
                change: AutoRuleChange::ReplaceGroup {
                    kind: AutoRuleKind::SoftDeny,
                    texts: vec!["No remote publication".into()],
                },
            },
            added.revision,
        )
        .await
        .unwrap();
    assert_eq!(replaced.auto_policy.rules.len(), 19);
    assert!(!replaced.auto_rule_settings.soft_deny.include_builtins);
    let environment = agent
        .apply_permission_change(
            chat,
            PermissionChange::ChangeAutoRules {
                change: AutoRuleChange::ReplaceEnvironment {
                    text: "Private research workstation".into(),
                },
            },
            replaced.revision,
        )
        .await
        .unwrap();
    assert!(matches!(
        environment.auto_policy.environment.source,
        AutoEnvironmentSource::UserReplacement
    ));
    let restored = agent
        .apply_permission_change(
            chat,
            PermissionChange::ChangeAutoRules {
                change: AutoRuleChange::RestoreGroup {
                    kind: AutoRuleKind::SoftDeny,
                },
            },
            environment.revision,
        )
        .await
        .unwrap();
    assert_eq!(restored.auto_policy.rules.len(), 83);
    assert!(matches!(
        restored.auto_policy.environment.source,
        AutoEnvironmentSource::UserReplacement
    ));
    let final_view = agent
        .apply_permission_change(
            chat,
            PermissionChange::ChangeAutoRules {
                change: AutoRuleChange::RestoreEnvironment,
            },
            restored.revision,
        )
        .await
        .unwrap();
    assert!(matches!(
        final_view.auto_policy.environment.source,
        AutoEnvironmentSource::Builtin { version: 10 }
    ));
}

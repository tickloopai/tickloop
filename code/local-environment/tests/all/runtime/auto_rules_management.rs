use std::sync::Arc;
use tickloop_code_settings::local_agent::LocalAgent;
use tickloop_harness::test_support::ScriptedModel;

use tickloop_harness::protocol::{
    AutoEnvironmentSource, AutoRuleChange, AutoRuleKind, AutoRuleSource, PermissionChange,
};
use tickloop_harness::runtime::{Agent, PermissionChangeError};

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
        AutoEnvironmentSource::Builtin { version: 8 }
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
        AutoEnvironmentSource::Builtin { version: 8 }
    ));
}

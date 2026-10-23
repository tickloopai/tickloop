use tickloop_agent_protocol::{
    AutoEnvironment, AutoEnvironmentSource, AutoPolicy, AutoRule, AutoRuleKind, AutoRuleSource,
};

use crate::AutoSettings;

const AUTO_POLICY_VERSION: u64 = 8;
const BUILTIN_GROUP_VERSION: u64 = 8;

#[derive(serde::Deserialize)]
struct BuiltinRule {
    id: String,
    kind: AutoRuleKind,
    text: String,
}

#[derive(serde::Deserialize)]
struct BuiltinPolicy {
    rules: Vec<BuiltinRule>,
    environment: String,
}

static BUILTIN_POLICY: std::sync::LazyLock<BuiltinPolicy> = std::sync::LazyLock::new(|| {
    serde_json::from_str(include_str!("auto_policy/defaults.json"))
        .expect("built-in Auto policy must be valid")
});

impl AutoSettings {
    /// The automatic review policy these settings configure, built-in rule groups included.
    pub fn policy(&self) -> AutoPolicy {
        self.policy_in_directory(None)
    }

    pub fn policy_for_directory(&self, directory: &std::path::Path) -> AutoPolicy {
        self.policy_in_directory(Some(directory))
    }

    fn policy_in_directory(&self, directory: Option<&std::path::Path>) -> AutoPolicy {
        let configured = &self.rules;
        let mut rules = Vec::new();
        for kind in [
            AutoRuleKind::SoftAllow,
            AutoRuleKind::SoftDeny,
            AutoRuleKind::HardDeny,
        ] {
            if configured.group(kind).include_builtins {
                rules.extend(
                    BUILTIN_POLICY
                        .rules
                        .iter()
                        .filter(|rule| rule.kind == kind)
                        .map(|rule| AutoRule {
                            clears: None,
                            id: rule.id.clone(),
                            kind,
                            text: rule.text.clone(),
                            source: AutoRuleSource::Builtin {
                                version: BUILTIN_GROUP_VERSION,
                            },
                        }),
                );
            }
            rules.extend(configured.group(kind).custom.iter().map(|custom| AutoRule {
                clears: None,
                id: custom.id.clone(),
                kind,
                text: custom.text.clone(),
                source: AutoRuleSource::User,
            }));
        }
        let mut environment = configured.environment_replacement.as_ref().map_or_else(
            || AutoEnvironment {
                entries: Vec::new(),
                text: BUILTIN_POLICY.environment.clone(),
                source: AutoEnvironmentSource::Builtin {
                    version: BUILTIN_GROUP_VERSION,
                },
            },
            |text| AutoEnvironment {
                entries: Vec::new(),
                text: text.clone(),
                source: AutoEnvironmentSource::UserReplacement,
            },
        );
        environment.entries = configured
            .environment_entries
            .iter()
            .filter(|entry| {
                entry.directory.as_ref().is_none_or(|scope| {
                    directory.is_some_and(|directory| {
                        directory == scope
                            || entry.include_descendants && directory.starts_with(scope)
                    })
                })
            })
            .cloned()
            .collect();
        AutoPolicy {
            version: AUTO_POLICY_VERSION,
            rules,
            environment,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_preserves_the_pinned_hard_boundary_and_specific_exceptions() {
        let policy = AutoSettings::default().policy();
        let rule = |id: &str| policy.rules.iter().find(|rule| rule.id == id).unwrap();
        assert_eq!(
            rule("builtin.hard_deny.data_exfiltration").kind,
            AutoRuleKind::HardDeny
        );
        assert_eq!(
            rule("builtin.soft_deny.sensitive_source_provenance").kind,
            AutoRuleKind::SoftDeny
        );
        assert!(
            rule("builtin.soft_deny.permission_grant")
                .text
                .contains("who gets which permission")
        );
        let bootstrap = rule("builtin.soft_allow.toolchain_bootstrap");
        assert!(bootstrap.text.contains("sh.rustup.rs"));
        assert!(bootstrap.text.contains("manifest or build config"));
        for unsupported in ["memory_directory", "browser_trusted_navigation"] {
            assert!(
                !policy
                    .rules
                    .iter()
                    .any(|rule| rule.id.ends_with(unsupported))
            );
        }
    }

    #[test]
    fn default_policy_maps_chat_scheduling_without_goal_rules() {
        let policy = AutoSettings::default().policy();
        let rule = |id: &str| policy.rules.iter().find(|rule| rule.id == id).unwrap();
        let scheduling = rule("builtin.soft_allow.claude_code_scheduling");
        assert_eq!(scheduling.kind, AutoRuleKind::SoftAllow);
        for tool in ["CronCreate", "CronList", "CronDelete", "ScheduleWakeup"] {
            assert!(scheduling.text.contains(tool), "missing {tool}");
        }
        for boundary in [
            "Unauthorized Persistence",
            "Account & Standing-Rule Changes",
            "Session-Created Job Cleanup",
            "Interfere With Workloads",
            "user-created",
        ] {
            assert!(scheduling.text.contains(boundary), "missing {boundary}");
        }
        assert!(
            !policy
                .rules
                .iter()
                .any(|rule| rule.id == "builtin.soft_allow.chat_goal_lifecycle")
        );
        for tool in ["GetGoal", "CreateGoal", "UpdateGoal"] {
            assert!(
                !policy.rules.iter().any(|rule| rule.text.contains(tool)),
                "{tool} does not use Auto classification"
            );
        }
    }

    #[test]
    fn default_policy_keeps_the_pinned_rule_kinds() {
        let policy = AutoSettings::default().policy();
        assert_eq!(policy.rules.len(), 82);
        assert_eq!(
            policy
                .rules
                .iter()
                .filter(|rule| rule.kind == AutoRuleKind::SoftAllow)
                .count(),
            15
        );
        assert_eq!(
            policy
                .rules
                .iter()
                .filter(|rule| rule.kind == AutoRuleKind::HardDeny)
                .count(),
            2
        );
        assert!(
            policy
                .rules
                .iter()
                .filter(|rule| rule.kind == AutoRuleKind::SoftDeny)
                .count()
                == 65
        );
        assert!(
            policy
                .rules
                .iter()
                .all(|rule| matches!(rule.source, AutoRuleSource::Builtin { version: 8 }))
        );
    }
}

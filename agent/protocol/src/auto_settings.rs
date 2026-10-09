use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AutoReviewerPreference {
    #[default]
    Llm,
    Jev,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AutoSettingsRevision(String);

impl AutoSettingsRevision {
    pub fn new(digest: String) -> Self {
        Self(digest)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoRuleKind {
    SoftAllow,
    SoftDeny,
    HardDeny,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CustomAutoRule {
    pub id: String,
    pub text: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AutoRuleGroupSettings {
    pub include_builtins: bool,
    pub custom: Vec<CustomAutoRule>,
}

impl Default for AutoRuleGroupSettings {
    fn default() -> Self {
        Self {
            include_builtins: true,
            custom: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct AutoRuleSettings {
    pub soft_allow: AutoRuleGroupSettings,
    pub soft_deny: AutoRuleGroupSettings,
    pub hard_deny: AutoRuleGroupSettings,
    pub environment_replacement: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum AutoRuleChange {
    Add {
        kind: AutoRuleKind,
        text: String,
    },
    Update {
        id: String,
        text: String,
    },
    Remove {
        id: String,
    },
    ReplaceGroup {
        kind: AutoRuleKind,
        texts: Vec<String>,
    },
    RestoreGroup {
        kind: AutoRuleKind,
    },
    ReplaceEnvironment {
        text: String,
    },
    RestoreEnvironment,
}

impl AutoRuleSettings {
    pub fn apply(&mut self, change: AutoRuleChange) -> Result<(), String> {
        let before = self.clone();
        match change {
            AutoRuleChange::Add { kind, text } => {
                self.group_mut(kind).custom.push(CustomAutoRule {
                    id: Uuid::new_v4().to_string(),
                    text,
                })
            }
            AutoRuleChange::Update { id, text } => {
                let kind = [
                    AutoRuleKind::SoftAllow,
                    AutoRuleKind::SoftDeny,
                    AutoRuleKind::HardDeny,
                ]
                .into_iter()
                .find(|kind| self.group(*kind).custom.iter().any(|rule| rule.id == id))
                .ok_or("Auto rule is read-only or does not exist")?;
                let Some(rule) = self
                    .group_mut(kind)
                    .custom
                    .iter_mut()
                    .find(|rule| rule.id == id)
                else {
                    return Err("Auto rule is read-only or does not exist".into());
                };
                rule.text = text;
            }
            AutoRuleChange::Remove { id } => {
                let mut removed = false;
                for kind in [
                    AutoRuleKind::SoftAllow,
                    AutoRuleKind::SoftDeny,
                    AutoRuleKind::HardDeny,
                ] {
                    let group = self.group_mut(kind);
                    let old_len = group.custom.len();
                    group.custom.retain(|rule| rule.id != id);
                    removed |= old_len != group.custom.len();
                }
                if !removed {
                    return Err("Auto rule is read-only or does not exist".into());
                }
            }
            AutoRuleChange::ReplaceGroup { kind, texts } => {
                let group = self.group_mut(kind);
                group.include_builtins = false;
                group.custom = texts
                    .into_iter()
                    .map(|text| CustomAutoRule {
                        id: Uuid::new_v4().to_string(),
                        text,
                    })
                    .collect();
            }
            AutoRuleChange::RestoreGroup { kind } => {
                *self.group_mut(kind) = AutoRuleGroupSettings::default()
            }
            AutoRuleChange::ReplaceEnvironment { text } => {
                self.environment_replacement = Some(text)
            }
            AutoRuleChange::RestoreEnvironment => self.environment_replacement = None,
        }
        self.validate()?;
        if *self == before {
            return Err("Auto settings are unchanged".into());
        }
        Ok(())
    }

    pub fn group(&self, kind: AutoRuleKind) -> &AutoRuleGroupSettings {
        match kind {
            AutoRuleKind::SoftAllow => &self.soft_allow,
            AutoRuleKind::SoftDeny => &self.soft_deny,
            AutoRuleKind::HardDeny => &self.hard_deny,
        }
    }

    pub fn group_mut(&mut self, kind: AutoRuleKind) -> &mut AutoRuleGroupSettings {
        match kind {
            AutoRuleKind::SoftAllow => &mut self.soft_allow,
            AutoRuleKind::SoftDeny => &mut self.soft_deny,
            AutoRuleKind::HardDeny => &mut self.hard_deny,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        let mut ids = HashSet::new();
        for kind in [
            AutoRuleKind::SoftAllow,
            AutoRuleKind::SoftDeny,
            AutoRuleKind::HardDeny,
        ] {
            for rule in &self.group(kind).custom {
                if !Uuid::parse_str(&rule.id).is_ok_and(|parsed| parsed.to_string() == rule.id) {
                    return Err(format!("invalid custom Auto rule ID: {}", rule.id));
                }
                if !ids.insert(&rule.id) {
                    return Err(format!("duplicate custom Auto rule ID: {}", rule.id));
                }
                if rule.text.trim().is_empty() {
                    return Err(format!("Auto rule {} has empty text", rule.id));
                }
            }
        }
        if self
            .environment_replacement
            .as_ref()
            .is_some_and(|text| text.trim().is_empty())
        {
            return Err("Auto Environment replacement is empty".into());
        }
        Ok(())
    }
}

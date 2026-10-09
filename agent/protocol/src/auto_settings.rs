use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

const MAX_ENVIRONMENT_ENTRIES: usize = 256;
const MAX_ENVIRONMENT_TEXT_BYTES: usize = 4096;
const MAX_ENVIRONMENT_CHANGES: usize = 16;
const MAX_ENVIRONMENT_SOURCES: usize = 32;
const MAX_ENVIRONMENT_SOURCE_BYTES: usize = 256;

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
    pub environment_entries: Vec<AutoEnvironmentEntry>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AutoEnvironmentEntry {
    pub id: Uuid,
    pub text: String,
    pub directory: Option<std::path::PathBuf>,
    pub include_descendants: bool,
    pub source: AutoEnvironmentEntrySource,
    pub confirmed_at_ms: u64,
    #[serde(default)]
    pub changes: Vec<AutoEnvironmentChange>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AutoEnvironmentChange {
    pub previous_text: String,
    pub previous_directory: Option<std::path::PathBuf>,
    pub previous_include_descendants: bool,
    pub changed_at_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoEnvironmentEntrySource {
    Manual,
    Generated {
        batch: Uuid,
        generated_at_ms: u64,
        sources: Vec<String>,
    },
    Unknown,
}

impl std::fmt::Display for AutoEnvironmentEntrySource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Manual => formatter.write_str("Manual"),
            Self::Unknown => formatter.write_str("Historical source unknown"),
            Self::Generated {
                generated_at_ms,
                sources,
                ..
            } => write!(
                formatter,
                "Generated at {generated_at_ms} from {}",
                sources.join("; ")
            ),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum AutoRuleChange {
    UpdateEnvironmentEntry {
        previous: AutoEnvironmentEntry,
        text: String,
        directory: Option<std::path::PathBuf>,
        include_descendants: bool,
    },
    MigrateEnvironment {
        expected_text: String,
    },
    AddEnvironmentEntries {
        entries: Vec<AutoEnvironmentEntry>,
    },
    RemoveEnvironmentEntries {
        entries: Vec<AutoEnvironmentEntry>,
    },
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
        let confirmed_at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u64::MAX as u128) as u64;
        match change {
            AutoRuleChange::UpdateEnvironmentEntry {
                previous,
                text,
                directory,
                include_descendants,
            } => {
                let entry = self
                    .environment_entries
                    .iter_mut()
                    .find(|entry| **entry == previous)
                    .ok_or("Environment entry changed; review the edit again")?;
                if entry.text == text
                    && entry.directory == directory
                    && entry.include_descendants == include_descendants
                {
                    return Err("Environment entry is unchanged".into());
                }
                entry.changes.push(AutoEnvironmentChange {
                    previous_text: entry.text.clone(),
                    previous_directory: entry.directory.clone(),
                    previous_include_descendants: entry.include_descendants,
                    changed_at_ms: confirmed_at_ms,
                });
                if entry.changes.len() > MAX_ENVIRONMENT_CHANGES {
                    entry.changes.remove(0);
                }
                entry.text = text;
                entry.directory = directory;
                entry.include_descendants = include_descendants;
                entry.confirmed_at_ms = confirmed_at_ms;
            }
            AutoRuleChange::MigrateEnvironment { expected_text } => {
                if self.environment_replacement.as_ref() != Some(&expected_text) {
                    return Err("Legacy Environment changed; review the migration again".into());
                }
                self.environment_replacement = None;
                self.environment_entries.push(AutoEnvironmentEntry {
                    id: Uuid::new_v4(),
                    text: expected_text,
                    directory: None,
                    include_descendants: false,
                    source: AutoEnvironmentEntrySource::Unknown,
                    confirmed_at_ms,
                    changes: Vec::new(),
                });
            }
            AutoRuleChange::AddEnvironmentEntries { mut entries } => {
                if self.environment_replacement.is_some() {
                    return Err("Restore the built-in Environment explicitly before adding structured entries; the legacy replacement is unchanged".into());
                }
                for entry in &mut entries {
                    entry.confirmed_at_ms = confirmed_at_ms;
                }
                self.environment_entries.extend(entries);
            }
            AutoRuleChange::RemoveEnvironmentEntries { entries } => {
                self.environment_entries
                    .retain(|entry| !entries.contains(entry));
            }
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
                if !self.environment_entries.is_empty() {
                    return Err("Remove structured Environment entries before replacing the base Environment".into());
                }
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
        if self.environment_replacement.is_some() && !self.environment_entries.is_empty() {
            return Err("Legacy Environment replacement cannot contain structured entries".into());
        }
        let mut environment_ids = HashSet::new();
        if self.environment_entries.len() > MAX_ENVIRONMENT_ENTRIES {
            return Err("Too many Environment entries".into());
        }
        for entry in &self.environment_entries {
            if entry.changes.len() > MAX_ENVIRONMENT_CHANGES
                || entry
                    .changes
                    .iter()
                    .any(|change| change.previous_text.len() > MAX_ENVIRONMENT_TEXT_BYTES)
            {
                return Err("Environment change history exceeds its limit".into());
            }
            if !environment_ids.insert(entry.id)
                || entry.text.trim().is_empty()
                || entry.text.len() > MAX_ENVIRONMENT_TEXT_BYTES
            {
                return Err("Invalid or duplicate Environment entry".into());
            }
            if let Some(directory) = &entry.directory {
                if !directory.is_absolute()
                    || directory
                        .components()
                        .any(|component| matches!(component, std::path::Component::ParentDir))
                {
                    return Err("Environment directory must be an absolute canonical path".into());
                }
            } else if entry.include_descendants {
                return Err("Global Environment entries have no descendant scope".into());
            }
            if let AutoEnvironmentEntrySource::Generated { sources, .. } = &entry.source
                && (sources.len() > MAX_ENVIRONMENT_SOURCES
                    || sources
                        .iter()
                        .any(|source| source.len() > MAX_ENVIRONMENT_SOURCE_BYTES))
            {
                return Err("Environment provenance exceeds its limit".into());
            }
        }
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

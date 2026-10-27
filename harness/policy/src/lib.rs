mod auto_policy;
pub mod instructions;
pub mod loop_skill;
pub mod settings;
pub mod skills;
use std::collections::HashSet;
use tickloop_harness_protocol::{AutoReviewerPreference, AutoRuleSettings, PermissionRule};
use uuid::Uuid;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AutoSettings {
    pub schema_version: u64,
    pub reviewer: AutoReviewerPreference,
    pub rules: AutoRuleSettings,
}

impl Default for AutoSettings {
    fn default() -> Self {
        Self {
            schema_version: 1,
            reviewer: AutoReviewerPreference::Llm,
            rules: AutoRuleSettings::default(),
        }
    }
}

/// A change to the persistent rules of one project partition: first remove rules whose id is in
/// `removed` or `added`, then append `added`, so a rule whose content changed (same id) is
/// replaced.
#[derive(Clone, Debug, Default)]
pub struct ProjectRuleChanges {
    pub removed: HashSet<Uuid>,
    pub added: Vec<PermissionRule>,
}

pub mod new_chat;

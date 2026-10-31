use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::RunId;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct RemoteSkillResource {
    pub uri: String,
    pub digest: String,
    pub size: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SkillSource {
    BuiltIn,
    User {
        path: PathBuf,
    },
    Project {
        path: PathBuf,
    },
    /// A Skill an installed Tick package brings, named `<package>:<skill>`.
    TickPackage {
        package: String,
        path: PathBuf,
    },
    /// A Skill an installed, enabled plugin brings, named `<plugin>:<skill>`.
    Plugin {
        plugin: String,
        path: PathBuf,
    },
    Remote {
        server_id: String,
        account_id: String,
        uri: String,
        resources: Vec<RemoteSkillResource>,
    },
    /// A source a later version knows; its Skill is never loaded again from here.
    #[serde(untagged)]
    Later(crate::LaterVariant<SkillSource>),
}

impl crate::TaggedRecord for SkillSource {
    const TAG: &'static str = "kind";
    const KNOWN: &'static [&'static str] = &[
        "built_in",
        "user",
        "project",
        "tick_package",
        "plugin",
        "remote",
    ];
}

impl SkillSource {
    pub fn display_label(&self) -> String {
        match self {
            Self::BuiltIn => "Built-in".into(),
            Self::User { .. } => "User".into(),
            Self::Project { .. } => "Project".into(),
            Self::TickPackage { package, .. } => format!("Tick package · {package}"),
            Self::Plugin { plugin, .. } => format!("Plugin · {plugin}"),
            Self::Remote { .. } => "Remote".into(),
            Self::Later(_) => "Later version".into(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct RemoteSkillDefinition {
    pub name: String,
    pub description: String,
    pub source: SkillSource,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct InvokedSkill {
    /// Whether the original command required Shell preprocessing, even after completion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell_preprocessing: Option<bool>,
    pub name: String,
    pub content: String,
    pub source: SkillSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<RunId>,
}

/// Metadata captured with a user's Skill input. Preparation facts belong to that input's Run;
/// they neither activate guidance by themselves nor grant tool permissions.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct UserSkillInvocation {
    pub name: String,
    pub source: SkillSource,
    /// This input's guidance was fully prepared without requiring Shell. This input-bound
    /// fact survives later same-name invocations replacing the current guidance cache.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub prepared_without_shell: bool,
}

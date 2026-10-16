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
    Remote {
        server_id: String,
        account_id: String,
        uri: String,
        resources: Vec<RemoteSkillResource>,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct RemoteSkillDefinition {
    pub name: String,
    pub description: String,
    pub source: SkillSource,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct InvokedSkill {
    pub name: String,
    pub content: String,
    pub source: SkillSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<RunId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub references: Vec<String>,
}

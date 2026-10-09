use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SkillSource {
    BuiltIn,
    User { path: PathBuf },
    Project { path: PathBuf },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct InvokedSkill {
    pub name: String,
    pub content: String,
    pub source: SkillSource,
}

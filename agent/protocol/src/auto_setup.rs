use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::{AutoEnvironmentEntry, PermissionsRevision};

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoSetupPurpose {
    Development,
    Research,
    #[default]
    Mixed,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AutoSetupOptions {
    pub purpose: AutoSetupPurpose,
    pub scan_shell_history: bool,
    pub other_repositories: Vec<PathBuf>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AutoEnvironmentDraft {
    pub revision: PermissionsRevision,
    pub entries: Vec<AutoEnvironmentEntry>,
    pub gaps: Vec<String>,
}

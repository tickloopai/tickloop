use serde::{Deserialize, Serialize};

/// Isolation of one prepared execution, as declared by its execution environment.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionIsolation {
    pub writes: WriteIsolation,
    pub network: NetworkIsolation,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteIsolation {
    #[default]
    Unrestricted,
    /// The Folder, additional authorized roots and temporary storage.
    FolderAndDirectories,
    /// The entire execution environment is isolated from the user's machine.
    Environment,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkIsolation {
    #[default]
    Unrestricted,
    Allowlist,
    Disconnected,
}

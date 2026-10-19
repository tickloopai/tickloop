use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub const RUNTIME_HANDOFF_VERSION: u16 = 1;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct RuntimeBinaryInfo {
    pub product_version: String,
    pub build_id: String,
    pub protocol_major: u16,
    pub protocol_minor: u16,
    pub handoff_version: u16,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct RuntimeVersion {
    pub executable: PathBuf,
    pub build_id: String,
    pub requested_executable: Option<PathBuf>,
    pub upgrading: bool,
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workers: Vec<WorkerRuntimeVersion>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct WorkerRuntimeVersion {
    pub process_id: u32,
    pub executable: PathBuf,
    pub build_id: Option<String>,
    pub upgrading: bool,
}

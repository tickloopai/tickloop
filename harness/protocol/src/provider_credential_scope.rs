use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProviderCredentialScope {
    pub config_root: Option<PathBuf>,
    pub environment_keys: Vec<String>,
    pub service_account_path: Result<Option<String>, String>,
    pub service_account_configured: Result<bool, String>,
}

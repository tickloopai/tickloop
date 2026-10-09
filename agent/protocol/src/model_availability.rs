use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ModelAvailability {
    DirectoryListed,
    Unavailable { reason: String },
    Unknown { reason: String },
}

impl ModelAvailability {
    pub fn selectable(&self) -> bool {
        !matches!(self, Self::Unavailable { .. })
    }
    pub fn label(&self) -> &str {
        match self {
            Self::DirectoryListed => "Directory confirmed",
            Self::Unavailable { reason } | Self::Unknown { reason } => reason,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum DirectorySemantics {
    Directory,
    ExclusiveAuthorization,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AccountModelDirectory {
    pub models: BTreeMap<String, ModelAvailability>,
    pub semantics: DirectorySemantics,
    pub complete: bool,
}

impl AccountModelDirectory {
    pub fn availability(&self, service_model_id: &str) -> ModelAvailability {
        self.models
            .get(service_model_id)
            .cloned()
            .unwrap_or_else(|| {
                if self.complete && self.semantics == DirectorySemantics::ExclusiveAuthorization {
                    ModelAvailability::Unavailable {
                        reason: "Not authorized in the account directory".into(),
                    }
                } else {
                    ModelAvailability::Unknown {
                        reason: "Not verified in this account directory".into(),
                    }
                }
            })
    }
}

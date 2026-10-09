use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProductKind {
    PayAsYouGo,
    Plan,
    Service,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectionRegion {
    pub name: String,
    pub order: u16,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelDiscovery {
    #[default]
    Unsupported,
    #[serde(rename = "openai_api")]
    OpenAiApi,
    Codex,
    GithubCopilot,
    ProviderApi,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConnection {
    pub vendor: String,
    pub product: String,
    pub kind: ProductKind,
    #[serde(default)]
    pub order: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<ConnectionRegion>,
    #[serde(default)]
    pub discovery: ModelDiscovery,
}

impl ProviderConnection {
    pub fn validate(&self) -> Result<(), String> {
        if self.vendor.trim().is_empty()
            || self.product.trim().is_empty()
            || self
                .region
                .as_ref()
                .is_some_and(|region| region.name.trim().is_empty())
        {
            return Err("connection vendor, product and region name must not be empty".into());
        }
        Ok(())
    }

    pub fn sort_key(&self) -> (ProductKind, u16, u16) {
        (
            self.kind,
            self.order,
            self.region.as_ref().map_or(0, |region| region.order),
        )
    }
}

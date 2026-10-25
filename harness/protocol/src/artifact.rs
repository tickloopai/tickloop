use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub enum ArtifactFeedbackHistory {
    #[default]
    #[serde(rename = "new")]
    NewOnly,
    #[serde(rename = "all")]
    All,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactAudience {
    Private,
    Shared,
    Public,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactPublication {
    pub audience: ArtifactAudience,
    pub membership_revision: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ArtifactTick {
    pub artifact_id: String,
    pub origin: String,
    pub title: String,
    pub format: String,
    pub url: String,
    pub access_role: String,
    pub added_at_ms: u64,
}

impl ArtifactTick {
    pub fn instance_id(&self) -> String {
        tick_protocol::artifact_instance_id(&self.origin, &self.artifact_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifact_tick_uses_the_canonical_subscription_identity() {
        let tick = ArtifactTick {
            artifact_id: "doc".into(),
            origin: "https://docs.example/".into(),
            title: "Document".into(),
            format: "markdown".into(),
            url: "https://docs.example/a/doc".into(),
            access_role: "owner".into(),
            added_at_ms: 1,
        };
        assert_eq!(
            tick.instance_id(),
            tick_protocol::InstanceRef::artifact(&tick.origin, &tick.artifact_id)
                .unwrap()
                .as_str()
        );
    }
}

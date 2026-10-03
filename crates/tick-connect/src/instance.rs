//! The complete identity of one running Tick in its Native host.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

const HERDR_SOURCE: &str = "herdr:";

pub fn artifact_instance_id(origin: &str, id: &str) -> String {
    format!("artifact:{}/{id}", origin.trim_end_matches('/'))
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct InstanceRef(String);

impl InstanceRef {
    pub fn native(run: impl AsRef<str>) -> Result<Self, &'static str> {
        let run = run.as_ref();
        if run.is_empty() || run.chars().any(char::is_control) {
            return Err("a Tick Instance requires a nonempty run identity");
        }
        let identity = if run.starts_with(HERDR_SOURCE) {
            run.to_owned()
        } else {
            format!("{HERDR_SOURCE}{run}")
        };
        if identity == HERDR_SOURCE {
            return Err("a Tick Instance requires a nonempty run identity");
        }
        Ok(Self(identity))
    }

    pub fn artifact(origin: &str, id: &str) -> Result<Self, &'static str> {
        if id.is_empty()
            || id.contains('/')
            || id.chars().any(char::is_control)
            || !(origin.starts_with("https://") || origin.starts_with("http://"))
            || origin.chars().any(char::is_control)
        {
            return Err("an Artifact Tick requires an origin and artifact identity");
        }
        Ok(Self(artifact_instance_id(origin, id)))
    }

    pub fn artifact_parts(&self) -> Option<(&str, &str)> {
        self.0.strip_prefix("artifact:")?.rsplit_once('/')
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for InstanceRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl Serialize for InstanceRef {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for InstanceRef {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let identity = String::deserialize(deserializer)?;
        if let Some(artifact) = identity.strip_prefix("artifact:") {
            let (origin, id) = artifact
                .rsplit_once('/')
                .ok_or_else(|| serde::de::Error::custom("invalid Artifact Tick identity"))?;
            return Self::artifact(origin, id).map_err(serde::de::Error::custom);
        }
        if !identity.starts_with(HERDR_SOURCE) {
            return Err(serde::de::Error::custom(
                "Tick Instance reference must start with herdr:",
            ));
        }
        Self::native(identity).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::InstanceRef;

    #[test]
    fn run_identity_round_trips_without_losing_source_or_bytes() {
        let reference =
            InstanceRef::native("session/workspace/pane/1c6391d8-aaaa-bbbb-cccc-123456789abc")
                .unwrap();
        let serialized = serde_json::to_string(&reference).unwrap();
        assert_eq!(
            serialized,
            "\"herdr:session/workspace/pane/1c6391d8-aaaa-bbbb-cccc-123456789abc\""
        );
        assert_eq!(
            serde_json::from_str::<InstanceRef>(&serialized).unwrap(),
            reference
        );
        assert_ne!(
            reference,
            InstanceRef::native("session/workspace/pane/1c6391d8-aaaa-bbbb-cccc-123456789abd")
                .unwrap()
        );
    }

    #[test]
    fn numeric_and_unprefixed_records_cannot_be_read_as_tick_instances() {
        assert!(serde_json::from_str::<InstanceRef>("7").is_err());
        assert!(serde_json::from_str::<InstanceRef>("\"7\"").is_err());
        assert!(InstanceRef::native("herdr:").is_err());
    }
}

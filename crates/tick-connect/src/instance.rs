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
    pub fn new(tick_type: &str, id: &str) -> Result<Self, &'static str> {
        Self::validate_type(tick_type)?;
        if id.is_empty() || id.chars().any(char::is_control) {
            return Err("a Tick Instance requires a type and nonempty identity");
        }
        if tick_type == "artifact" {
            let (origin, artifact_id) = id
                .rsplit_once('/')
                .ok_or("invalid Artifact Tick identity")?;
            return Self::artifact(origin, artifact_id);
        }
        Ok(Self(format!("{tick_type}:{id}")))
    }

    pub fn validate_type(tick_type: &str) -> Result<(), &'static str> {
        if tick_type.is_empty()
            || !tick_type
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        {
            return Err("a Tick type requires a nonempty identifier");
        }
        Ok(())
    }

    pub fn tick_type(&self) -> &str {
        self.0.split_once(':').expect("validated Tick identity").0
    }

    pub fn id(&self) -> &str {
        self.0.split_once(':').expect("validated Tick identity").1
    }

    pub fn shell(task_id: impl fmt::Display) -> Self {
        Self::new("shell", &task_id.to_string()).expect("valid Shell task identity")
    }
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
        let (tick_type, id) = identity.split_once(':').ok_or_else(|| {
            serde::de::Error::custom("Tick Instance reference requires a type prefix")
        })?;
        Self::new(tick_type, id).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::InstanceRef;

    #[test]
    fn adapter_identities_round_trip_without_becoming_herdr_instances() {
        for identity in ["shell:task-17", "calendar:meeting-42"] {
            let reference: InstanceRef =
                serde_json::from_value(serde_json::json!(identity)).unwrap();
            assert_eq!(reference.as_str(), identity);
            assert_eq!(serde_json::to_value(reference).unwrap(), identity);
        }
        for identity in ["shell:", ":task", "bad type:task", "shell:task\n"] {
            assert!(serde_json::from_value::<InstanceRef>(serde_json::json!(identity)).is_err());
        }
    }

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

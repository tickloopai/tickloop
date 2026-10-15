//! The complete identity of one Tick Instance: its access, how the instance is reached, and its
//! identity there.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{ARTIFACT_ACCESS, HOSTED_ACCESS, SHELL_ACCESS};

pub fn artifact_instance_id(origin: &str, id: &str) -> String {
    format!("artifact:{}/{id}", origin.trim_end_matches('/'))
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct InstanceRef(String);

impl InstanceRef {
    pub fn new(access: &str, id: &str) -> Result<Self, &'static str> {
        Self::validate_access(access)?;
        if id.is_empty() || id.chars().any(char::is_control) {
            return Err("a Tick Instance requires an access and a nonempty identity");
        }
        if access == ARTIFACT_ACCESS {
            let (origin, artifact_id) = id
                .rsplit_once('/')
                .ok_or("invalid Artifact Tick identity")?;
            return Self::artifact(origin, artifact_id);
        }
        Ok(Self(format!("{access}:{id}")))
    }

    pub fn validate_access(access: &str) -> Result<(), &'static str> {
        if access.is_empty()
            || !access
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        {
            return Err("an access requires a nonempty identifier");
        }
        Ok(())
    }

    /// How the instance is reached, the prefix of its identity: `hosted`, `shell` or `artifact`.
    /// It is not the kind a program reports for itself.
    pub fn access(&self) -> &str {
        self.0.split_once(':').expect("validated Tick identity").0
    }

    pub fn id(&self) -> &str {
        self.0.split_once(':').expect("validated Tick identity").1
    }

    pub fn shell(task_id: impl fmt::Display) -> Self {
        Self::new(SHELL_ACCESS, &task_id.to_string()).expect("valid Shell task identity")
    }
    /// A Tick running in the dedicated Tickhost. `hosted` names where it runs, not a kind of
    /// Tick: its protocol and tools are those of the program it runs.
    pub fn hosted(run: uuid::Uuid) -> Self {
        Self(format!("{HOSTED_ACCESS}:{run}"))
    }

    /// The run identity of a hosted instance.
    pub fn hosted_run(&self) -> Option<uuid::Uuid> {
        self.0
            .strip_prefix(HOSTED_ACCESS)?
            .strip_prefix(':')?
            .parse()
            .ok()
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

impl std::str::FromStr for InstanceRef {
    type Err = &'static str;

    fn from_str(identity: &str) -> Result<Self, Self::Err> {
        if let Some(artifact) = identity.strip_prefix("artifact:") {
            let (origin, id) = artifact
                .rsplit_once('/')
                .ok_or("invalid Artifact Tick identity")?;
            return Self::artifact(origin, id);
        }
        let (access, id) = identity
            .split_once(':')
            .ok_or("Tick Instance reference requires an access prefix")?;
        Self::new(access, id)
    }
}

impl<'de> Deserialize<'de> for InstanceRef {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::InstanceRef;

    #[test]
    fn adapter_identities_round_trip_as_written() {
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
    fn hosted_identity_round_trips_with_its_run() {
        let run = uuid::Uuid::parse_str("1c6391d8-aaaa-bbbb-cccc-123456789abc").unwrap();
        let reference = InstanceRef::hosted(run);
        let serialized = serde_json::to_string(&reference).unwrap();
        assert_eq!(
            serialized,
            "\"hosted:1c6391d8-aaaa-bbbb-cccc-123456789abc\""
        );
        let read = serde_json::from_str::<InstanceRef>(&serialized).unwrap();
        assert_eq!(read, reference);
        assert_eq!(read.hosted_run(), Some(run));
    }

    #[test]
    fn numeric_and_unprefixed_records_cannot_be_read_as_tick_instances() {
        assert!(serde_json::from_str::<InstanceRef>("7").is_err());
        assert!(serde_json::from_str::<InstanceRef>("\"7\"").is_err());
        assert!(serde_json::from_str::<InstanceRef>("\"hosted:\"").is_err());
    }
}

//! The complete identity of one Tick Instance: its access, how the instance is reached, and its
//! identity there.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use tickloop_ids::{Id, IdContext, Kind, ModelRef};

use crate::{ARTIFACT_ACCESS, HOSTED_ACCESS, SHELL_ACCESS};

/// One run of a Tick in the Tickhost, the identity of a `hosted:` instance.
pub enum HostedRunKind {}

impl Kind for HostedRunKind {
    const NOUN: &'static str = "Tick Instance";
    const SOURCE: &'static str = "TickManage list gives each instance's tick_instance_id.";
    const MODEL_PREFIX: &'static str = "hosted:";
}

/// One background shell command, the identity of a `shell:` instance and of a Shell task.
pub enum ShellTaskKind {}

impl Kind for ShellTaskKind {
    const NOUN: &'static str = "task";
    const SOURCE: &'static str = "The Bash result that started a background command gives its id.";
    const MODEL_PREFIX: &'static str = "shell:";
}

pub type HostedRunId = Id<HostedRunKind>;

pub type ShellTaskId = Id<ShellTaskKind>;

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
        // Hosted and Shell identities are stored as hyphenated UUIDs whichever form they came in.
        let id = match access {
            HOSTED_ACCESS | SHELL_ACCESS => tickloop_ids::base62::decode_either(id)
                .map_or_else(|| id.to_owned(), |uuid| uuid.to_string()),
            _ => id.to_owned(),
        };
        Ok(Self(format!("{access}:{id}")))
    }

    /// The instance the model names in a tool argument. A `hosted:` or `shell:` UUID resolves
    /// through the Loop's id context; any other identity, such as a test's `hosted:run-1`, is
    /// taken as written and left to the caller's lookup.
    pub fn from_model(ids: &IdContext, text: &str) -> Result<Self, &'static str> {
        let text = text.trim();
        if let Ok(run) = ids.resolve(&ModelRef::<HostedRunKind>::from(text))
            && text.starts_with(HostedRunKind::MODEL_PREFIX)
        {
            return Ok(Self::hosted(run));
        }
        if let Ok(task) = ids.resolve(&ModelRef::<ShellTaskKind>::from(text))
            && text.starts_with(ShellTaskKind::MODEL_PREFIX)
        {
            return Ok(Self::shell(task));
        }
        text.parse()
    }

    /// The identity as a person reads it: a `hosted:` or `shell:` UUID in base62, any other
    /// identity as stored.
    pub fn public(&self) -> String {
        if let Some(run) = self.hosted_run() {
            return format!("{HOSTED_ACCESS}:{}", run.public());
        }
        if let Some(task) = self.shell_task() {
            return format!("{SHELL_ACCESS}:{}", task.public());
        }
        self.0.clone()
    }

    /// The identity as written into model text: a `hosted:` or `shell:` UUID in the Loop's model
    /// form, any other identity as stored.
    pub fn model_text(&self, ids: &IdContext) -> String {
        if let Some(run) = self.hosted_run() {
            return ids.model_text(run);
        }
        if let Some(task) = self.shell_task() {
            return ids.model_text(task);
        }
        self.0.clone()
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

    pub fn shell(task: impl Into<ShellTaskId>) -> Self {
        Self(format!("{SHELL_ACCESS}:{}", task.into().uuid()))
    }

    /// The Shell task of a `shell:` instance.
    pub fn shell_task(&self) -> Option<ShellTaskId> {
        let id = self.0.strip_prefix(SHELL_ACCESS)?.strip_prefix(':')?;
        tickloop_ids::base62::decode_either(id).map(Id::from_uuid)
    }

    /// A Tick running in the dedicated Tickhost. `hosted` names where it runs, not a kind of
    /// Tick: its protocol and tools are those of the program it runs.
    pub fn hosted(run: impl Into<HostedRunId>) -> Self {
        Self(format!("{HOSTED_ACCESS}:{}", run.into().uuid()))
    }

    /// The run identity of a hosted instance, written in either form.
    pub fn hosted_run(&self) -> Option<HostedRunId> {
        let id = self.0.strip_prefix(HOSTED_ACCESS)?.strip_prefix(':')?;
        tickloop_ids::base62::decode_either(id).map(Id::from_uuid)
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
        assert_eq!(read.hosted_run(), Some(run.into()));
    }

    #[test]
    fn hosted_and_shell_identities_are_stored_as_uuids_and_read_in_either_form() {
        let ids = tickloop_ids::IdContext::new();
        let run = super::HostedRunId::new();
        let task = super::ShellTaskId::new();
        let hosted = InstanceRef::hosted(run);
        let shell = InstanceRef::shell(task);
        assert_eq!(hosted.as_str(), format!("hosted:{}", run.uuid()));
        assert_eq!(shell.as_str(), format!("shell:{}", task.uuid()));
        assert_eq!(hosted.model_text(&ids), ids.model_text(run));
        assert_eq!(shell.model_text(&ids), ids.model_text(task));
        for (reference, model) in [
            (&hosted, hosted.model_text(&ids)),
            (&shell, shell.model_text(&ids)),
        ] {
            assert_eq!(&InstanceRef::from_model(&ids, &model).unwrap(), reference);
            assert_eq!(
                &InstanceRef::from_model(&ids, reference.as_str()).unwrap(),
                reference
            );
            assert_eq!(&model.parse::<InstanceRef>().unwrap(), reference);
        }
        assert_eq!(
            InstanceRef::from_model(&ids, &format!("hosted:{}", run.public()))
                .unwrap()
                .hosted_run(),
            Some(run)
        );
        let calendar = InstanceRef::new("calendar", "fixture").unwrap();
        assert_eq!(calendar.model_text(&ids), "calendar:fixture");
    }

    #[test]
    fn numeric_and_unprefixed_records_cannot_be_read_as_tick_instances() {
        assert!(serde_json::from_str::<InstanceRef>("7").is_err());
        assert!(serde_json::from_str::<InstanceRef>("\"7\"").is_err());
        assert!(serde_json::from_str::<InstanceRef>("\"hosted:\"").is_err());
    }
}

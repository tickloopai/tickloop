//! The Tick standard: newline-delimited JSON-RPC between a host and one Tick Instance over the
//! Tick's stdin/stdout, with its versions, methods, frame types and size limits.

mod instance;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub use instance::{InstanceRef, artifact_instance_id};

pub const NATIVE_PROTOCOL_VERSION: u32 = 2;
pub const NATIVE_UPDATE_VERSION: u32 = 3;
pub const MAX_REQUEST_BYTES: usize = 512 * 1024;
pub const MAX_DESCRIPTION_BYTES: usize = 512 * 1024;
pub const MAX_UPDATE_BYTES: usize = 512 * 1024;
pub const MAX_TOOL_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
pub const INSTANCE_ID_ENV: &str = "TICKLOOP_INSTANCE_ID";

pub const JSON_RPC_VERSION: &str = "2.0";
pub const DESCRIBE_METHOD: &str = "tick/describe";
pub const READ_METHOD: &str = "tick/read";
pub const TOOL_CALL_METHOD: &str = "tool/call";
/// The notification a Tick sends for each Tick Update.
pub const UPDATE_METHOD: &str = "tick/update";

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const TOOL_FAILED: i64 = -32000;
pub const VERSION_MISMATCH: i64 = -32001;
pub const INSTANCE_MISMATCH: i64 = -32002;
pub const FRAME_TOO_LARGE: i64 = -32003;

/// The instance type of a Tick running in a Herdr Pane.
pub const HERDR_INSTANCE_TYPE: &str = "herdr";
/// The instance type of a Tick running hidden in the Tickhost. It names where the Tick runs, not
/// a kind of Tick: its protocol and tools are those of the program it runs.
pub const HOSTED_INSTANCE_TYPE: &str = "hosted";
/// The instance type of a Shell Tick: one shell command execution of an Agent.
pub const SHELL_INSTANCE_TYPE: &str = "shell";
/// The instance types a host launches a protocol Tick under, as the prefix of the identity it
/// passes in [`INSTANCE_ID_ENV`].
const LAUNCHED_INSTANCE_TYPES: [&str; 2] = [HERDR_INSTANCE_TYPE, HOSTED_INSTANCE_TYPE];

/// Whether `identity` is a complete `herdr:` or `hosted:` instance identity a host could have
/// launched this Tick under.
pub fn is_launched_instance_id(identity: &str) -> bool {
    identity.split_once(':').is_some_and(|(tick_type, id)| {
        LAUNCHED_INSTANCE_TYPES.contains(&tick_type)
            && !id.is_empty()
            && !id.chars().any(char::is_control)
    })
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct TickTool {
    pub name: String,
    pub description: String,
    pub params: Value,
    pub mutates: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct TickDescriptor {
    pub protocol: u32,
    pub tick_type: String,
    pub title: String,
    pub description: String,
    pub recover_with: Option<String>,
    pub tools: Vec<TickTool>,
}

impl TickDescriptor {
    pub fn validate(&self) -> Result<(), String> {
        if self.protocol != NATIVE_PROTOCOL_VERSION {
            return Err(format!(
                "unsupported Tick protocol version {}",
                self.protocol
            ));
        }
        if self.tick_type.trim().is_empty() || self.title.trim().is_empty() {
            return Err("Tick type and title must be nonempty".into());
        }
        for (index, tool) in self.tools.iter().enumerate() {
            if tool.name.trim().is_empty() || !tool.params.is_object() {
                return Err("Tick Tool needs a name and parameter schema".into());
            }
            jsonschema::validator_for(&tool.params).map_err(|error| {
                format!("invalid schema for Tick Tool {:?}: {error}", tool.name)
            })?;
            if self.tools[..index]
                .iter()
                .any(|prior| prior.name == tool.name)
            {
                return Err(format!("duplicate Tick Tool {:?}", tool.name));
            }
        }
        if let Some(recovery) = &self.recover_with
            && !self
                .tools
                .iter()
                .any(|tool| &tool.name == recovery && !tool.mutates)
        {
            return Err(format!(
                "recovery tool {recovery:?} is not a declared query"
            ));
        }
        Ok(())
    }
}

pub fn descriptor_digest(descriptor: &TickDescriptor) -> String {
    let value = serde_json::to_value(descriptor).expect("Tick descriptor is serializable");
    descriptor_digest_value(&value)
}

pub fn descriptor_digest_value(value: &Value) -> String {
    let bytes = serde_json_canonicalizer::to_vec(value)
        .expect("Tick descriptor JSON is RFC 8785 serializable");
    format!("{:x}", Sha256::digest(bytes))
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct TickRead {
    pub version: u32,
    pub tick_instance_id: String,
    pub initialized: bool,
    pub sequence: u64,
    pub observed_at: String,
    pub data: Option<Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct TickUpdate {
    pub version: u32,
    pub tick_instance_id: String,
    pub source: String,
    pub sequence: u64,
    pub observed_at: String,
    pub data: Value,
}

pub struct TickProtocol {
    instance_id: String,
    sequence: Option<u64>,
}

impl TickProtocol {
    pub fn new(instance_id: impl Into<String>) -> Self {
        Self {
            instance_id: instance_id.into(),
            sequence: None,
        }
    }

    pub fn descriptor(&self, result: Value) -> Result<TickDescriptor, String> {
        let descriptor: TickDescriptor =
            serde_json::from_value(result).map_err(|error| error.to_string())?;
        check_encoded_size(&descriptor, MAX_DESCRIPTION_BYTES, "Tick description")?;
        descriptor.validate()?;
        Ok(descriptor)
    }

    pub fn read(&mut self, result: Value) -> Result<TickRead, String> {
        let read: TickRead = serde_json::from_value(result).map_err(|error| error.to_string())?;
        self.validate_identity(read.version, &read.tick_instance_id)?;
        if !read.initialized && (read.data.is_some() || read.sequence != 0) {
            return Err("uninitialized Tick cannot report content or a sequence".into());
        }
        if read.initialized && read.data.is_none() {
            return Err("initialized Tick needs content".into());
        }
        if self
            .sequence
            .is_some_and(|sequence| read.sequence < sequence)
        {
            return Err("Tick read sequence moved backward".into());
        }
        self.sequence = Some(read.sequence);
        Ok(read)
    }

    pub fn update(&mut self, params: Value) -> Result<TickUpdate, String> {
        let update: TickUpdate =
            serde_json::from_value(params).map_err(|error| error.to_string())?;
        self.validate_identity(update.version, &update.tick_instance_id)?;
        check_encoded_size(&update, MAX_UPDATE_BYTES, "Tick update")?;
        if update.sequence == 0
            || self
                .sequence
                .is_some_and(|sequence| update.sequence <= sequence)
        {
            return Err("Tick update sequence repeated or moved backward".into());
        }
        self.sequence = Some(update.sequence);
        Ok(update)
    }

    pub fn sequence(&self) -> Option<u64> {
        self.sequence
    }

    fn validate_identity(&self, version: u32, instance_id: &str) -> Result<(), String> {
        if version != NATIVE_UPDATE_VERSION {
            return Err(format!("unsupported Tick update version {version}"));
        }
        if instance_id != self.instance_id {
            return Err("Tick instance identity mismatch".into());
        }
        Ok(())
    }
}

/// Refuses `value` when its encoding exceeds `limit` bytes. What a Tick sends is parsed and
/// forwarded re-encoded, and number formatting can make it several times larger than the Tick's
/// own line (`1e15` becomes `1000000000000000.0`), so limits apply to the encoding.
pub fn check_encoded_size(value: &impl Serialize, limit: usize, what: &str) -> Result<(), String> {
    let encoded = serde_json::to_vec(value).map_err(|error| error.to_string())?;
    if encoded.len() > limit {
        return Err(format!("{what} exceeds {limit} bytes once encoded"));
    }
    Ok(())
}

pub fn parse_request(line: &[u8]) -> Result<Value, String> {
    if line.len() > MAX_REQUEST_BYTES {
        return Err(format!("Tick request exceeds {MAX_REQUEST_BYTES} bytes"));
    }
    let frame: Value = serde_json::from_slice(line).map_err(|error| error.to_string())?;
    if frame.get("jsonrpc").and_then(Value::as_str) != Some(JSON_RPC_VERSION) {
        return Err("unsupported JSON-RPC version".into());
    }
    if frame.get("method").and_then(Value::as_str).is_none() {
        return Err("request has no method".into());
    }
    Ok(frame)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_complete_herdr_and_hosted_identities_count_as_launched() {
        for identity in ["herdr:session/w/p/1", "hosted:7f1c"] {
            assert!(is_launched_instance_id(identity), "{identity}");
        }
        for identity in [
            "herdr:",
            "hosted",
            "shell:task-1",
            "artifact:https://a/1",
            "herdr:a\nb",
        ] {
            assert!(!is_launched_instance_id(identity), "{identity}");
        }
    }

    #[test]
    fn bad_update_does_not_replace_a_valid_sequence() {
        let mut protocol = TickProtocol::new("herdr:one");
        let valid = json!({"version":3,"tick_instance_id":"herdr:one","source":"demo","sequence":4,"observed_at":"2026-09-24T00:00:00Z","data":{"value":4}});
        assert_eq!(protocol.update(valid.clone()).unwrap().sequence, 4);
        assert!(protocol.update(valid).is_err());
        assert!(protocol.update(json!({"version":3,"tick_instance_id":"herdr:other","source":"demo","sequence":5,"observed_at":"2026-09-24T00:00:00Z","data":{}})).is_err());
        assert!(protocol.update(json!({"version":2,"tick_instance_id":"herdr:one","source":"demo","sequence":5,"observed_at":"2026-09-24T00:00:00Z","data":{}})).is_err());
        assert_eq!(protocol.sequence(), Some(4));
    }

    #[test]
    fn an_update_is_limited_by_its_encoded_size_not_its_line() {
        let mut protocol = TickProtocol::new("herdr:one");
        let values = vec!["1e15"; 100_000].join(",");
        let line = format!(
            r#"{{"version":3,"tick_instance_id":"herdr:one","source":"demo","sequence":1,"observed_at":"2026-09-24T00:00:00Z","data":[{values}]}}"#
        );
        assert!(line.len() < MAX_UPDATE_BYTES);
        let params: Value = serde_json::from_str(&line).unwrap();
        assert!(serde_json::to_vec(&params).unwrap().len() > MAX_UPDATE_BYTES);
        assert_eq!(
            protocol.update(params).unwrap_err(),
            format!("Tick update exceeds {MAX_UPDATE_BYTES} bytes once encoded")
        );
        assert_eq!(protocol.sequence(), None);
    }

    #[test]
    fn a_description_is_limited_by_its_encoded_size() {
        let protocol = TickProtocol::new("herdr:one");
        let description = "d".repeat(MAX_DESCRIPTION_BYTES);
        assert_eq!(
            protocol
                .descriptor(json!({"protocol":2,"tick_type":"external.weather","title":"Weather","description":description,"recover_with":null,"tools":[]}))
                .unwrap_err(),
            format!("Tick description exceeds {MAX_DESCRIPTION_BYTES} bytes once encoded")
        );
    }

    #[test]
    fn unknown_tick_type_with_valid_recovery_is_allowed() {
        let protocol = TickProtocol::new("herdr:one");
        let descriptor = protocol.descriptor(json!({"protocol":2,"tick_type":"external.weather","title":"Weather","description":"Conditions","recover_with":"current","tools":[{"name":"current","description":"Read","params":{"type":"object"},"mutates":false}]})).unwrap();
        assert_eq!(descriptor.tick_type, "external.weather");
    }

    #[test]
    fn uninitialized_read_cannot_claim_content() {
        let mut protocol = TickProtocol::new("herdr:one");
        let read = json!({"version":3,"tick_instance_id":"herdr:one","initialized":false,"sequence":0,"observed_at":"2026-09-24T00:00:00Z","data":null});
        assert!(!protocol.read(read).unwrap().initialized);
        assert!(protocol.read(json!({"version":3,"tick_instance_id":"herdr:one","initialized":false,"sequence":0,"observed_at":"2026-09-24T00:00:00Z","data":{"value":1}})).is_err());
        assert_eq!(protocol.sequence(), Some(0));
    }

    #[test]
    fn invalid_tool_schema_is_rejected() {
        let protocol = TickProtocol::new("herdr:one");
        let bad = json!({"protocol":2,"tick_type":"example.counter","title":"Counter","description":"External","recover_with":null,"tools":[{"name":"bad","description":"Bad","params":{"type":"not-a-type"},"mutates":true}]});
        assert!(
            protocol
                .descriptor(bad)
                .unwrap_err()
                .contains("invalid schema")
        );
    }
}

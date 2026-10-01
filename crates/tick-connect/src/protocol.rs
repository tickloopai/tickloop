use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub const NATIVE_PROTOCOL_VERSION: u32 = 2;
pub const NATIVE_UPDATE_VERSION: u32 = 3;
pub const MAX_REQUEST_BYTES: usize = 512 * 1024;
pub const MAX_DESCRIPTION_BYTES: usize = 512 * 1024;
pub const MAX_UPDATE_BYTES: usize = 512 * 1024;
pub const MAX_TOOL_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
pub const INSTANCE_ID_ENV: &str = "TICKLOOP_INSTANCE_ID";
pub const READ_METHOD: &str = "tick/read";

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

pub(crate) fn descriptor_digest_value(value: &Value) -> String {
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

pub fn parse_request(line: &[u8]) -> Result<Value, String> {
    if line.len() > MAX_REQUEST_BYTES {
        return Err(format!("Tick request exceeds {MAX_REQUEST_BYTES} bytes"));
    }
    let frame: Value = serde_json::from_slice(line).map_err(|error| error.to_string())?;
    if frame.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
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

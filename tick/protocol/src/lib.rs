//! The Tick protocol: newline-delimited JSON-RPC between a host and one Tick Instance over the
//! Tick's stdin/stdout, with its version, methods, frame types and size limits.

mod instance;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use instance::{InstanceRef, artifact_instance_id};

pub const NATIVE_PROTOCOL_VERSION: u32 = 3;
pub const MAX_REQUEST_BYTES: usize = 512 * 1024;
pub const MAX_DESCRIPTION_BYTES: usize = 512 * 1024;
pub const MAX_UPDATE_BYTES: usize = 512 * 1024;
pub const MAX_TOOL_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
/// The identity a host launches a Tick under. A Tick may log it; no frame carries it.
pub const INSTANCE_ID_ENV: &str = "TICKLOOP_INSTANCE_ID";

pub const JSON_RPC_VERSION: &str = "2.0";
pub const DESCRIBE_METHOD: &str = "tick/describe";
pub const READ_METHOD: &str = "tick/read";
pub const CALL_METHOD: &str = "tick/call";
/// The notification a Tick sends for each Tick Update.
pub const UPDATE_METHOD: &str = "tick/update";

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
/// The arguments do not match, or the Tick declares no such tool.
pub const INVALID_PARAMS: i64 = -32602;
pub const TOOL_FAILED: i64 = -32000;
pub const VERSION_MISMATCH: i64 = -32001;
pub const FRAME_TOO_LARGE: i64 = -32003;
/// A `tick/call` named another revision than the Tick's current catalog, so it was not run.
pub const REVISION_MISMATCH: i64 = -32004;

/// The instance type of a Tick running in a Herdr Pane.
pub const HERDR_INSTANCE_TYPE: &str = "herdr";
/// The instance type of a Tick running hidden in the Tickhost. It names where the Tick runs, not
/// a kind of Tick: its protocol and tools are those of the program it runs.
pub const HOSTED_INSTANCE_TYPE: &str = "hosted";
/// The instance type of a Shell Tick: one shell command execution of an Agent.
pub const SHELL_INSTANCE_TYPE: &str = "shell";
/// The instance type of an artifact Tick: one artifact of the artifact service.
pub const ARTIFACT_INSTANCE_TYPE: &str = "artifact";
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
    pub input_schema: Value,
    pub mutates: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct TickDescriptor {
    pub protocol: u32,
    /// What the program says it is, such as `example.counter`.
    pub kind: String,
    pub title: String,
    pub description: String,
    /// Names this catalog: the Tick changes it whenever any tool changes, and refuses a
    /// `tick/call` naming another revision with [`REVISION_MISMATCH`].
    pub revision: String,
    pub tools: Vec<TickTool>,
    /// The query tool that rebuilds what latest-only updates dropped; without one, dropped
    /// updates cannot be recovered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recover_with: Option<String>,
}

impl TickDescriptor {
    /// Reads a `tick/describe` result. Another protocol version is refused by its number before
    /// anything else, so an older Tick learns why rather than which field it lacks.
    pub fn parse(result: Value) -> Result<Self, String> {
        match result.get("protocol").and_then(Value::as_u64) {
            Some(version) if version == u64::from(NATIVE_PROTOCOL_VERSION) => {}
            Some(version) => {
                return Err(format!(
                    "unsupported Tick protocol version {version}; this host speaks Tick protocol {NATIVE_PROTOCOL_VERSION}"
                ));
            }
            None => return Err("Tick description names no protocol version".into()),
        }
        let descriptor: Self = serde_json::from_value(result).map_err(|error| error.to_string())?;
        check_encoded_size(&descriptor, MAX_DESCRIPTION_BYTES, "Tick description")?;
        descriptor.validate()?;
        Ok(descriptor)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.protocol != NATIVE_PROTOCOL_VERSION {
            return Err(format!(
                "unsupported Tick protocol version {}",
                self.protocol
            ));
        }
        if self.kind.trim().is_empty() || self.title.trim().is_empty() {
            return Err("Tick kind and title must be nonempty".into());
        }
        if self.revision.is_empty() {
            return Err("Tick catalog revision must be nonempty".into());
        }
        for (index, tool) in self.tools.iter().enumerate() {
            if tool.name.trim().is_empty() || !tool.input_schema.is_object() {
                return Err("Tick Tool needs a name and an input schema".into());
            }
            jsonschema::validator_for(&tool.input_schema).map_err(|error| {
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

/// What `tick/read` answers and every `tick/update` carries.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct TickState {
    pub seq: u64,
    /// Null while the Tick has no content yet.
    pub data: Value,
    /// RFC 3339. A host fills in when it received the state where the Tick left it out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
}

impl TickState {
    /// The state of a Tick nothing was read from yet.
    pub fn empty() -> Self {
        Self {
            seq: 0,
            data: Value::Null,
            at: None,
        }
    }

    pub fn is_ready(&self) -> bool {
        !self.data.is_null()
    }
}

/// The host side of one Tick Instance's states: each `tick/update` moves `seq` forward and has
/// content, and no `tick/read` reports a `seq` before one already seen.
#[derive(Default)]
pub struct TickProtocol {
    seq: Option<u64>,
}

impl TickProtocol {
    /// A `tick/read` answer, bounded only by the response frame that carried it.
    pub fn read(&mut self, result: Value) -> Result<TickState, String> {
        let state = received_state(result, "Tick read")?;
        if self.seq.is_some_and(|seq| state.seq < seq) {
            return Err("Tick read seq moved backward".into());
        }
        self.seq = Some(state.seq);
        Ok(state)
    }

    pub fn update(&mut self, params: Value) -> Result<TickState, String> {
        let state = received_state(params, "Tick update")?;
        check_encoded_size(&state, MAX_UPDATE_BYTES, "Tick update")?;
        if !state.is_ready() {
            return Err("Tick update needs data".into());
        }
        if self.seq.is_some_and(|seq| state.seq <= seq) {
            return Err("Tick update seq repeated or moved backward".into());
        }
        self.seq = Some(state.seq);
        Ok(state)
    }

    pub fn seq(&self) -> Option<u64> {
        self.seq
    }
}

fn received_state(value: Value, what: &str) -> Result<TickState, String> {
    let mut state: TickState = serde_json::from_value(value).map_err(|error| error.to_string())?;
    match &state.at {
        Some(at) => {
            time::OffsetDateTime::parse(at, &time::format_description::well_known::Rfc3339)
                .map_err(|error| format!("{what} `at` is not an RFC 3339 time: {error}"))?;
        }
        None => state.at = Some(now_rfc3339()),
    }
    Ok(state)
}

/// The current time as a state's `at`.
pub fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .expect("the current UTC time formats as RFC 3339")
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

    fn counter(extra: Value) -> Value {
        let mut descriptor = json!({"protocol":3,"kind":"example.counter","title":"Counter","description":"External","revision":"1","tools":[{"name":"current","description":"Read","input_schema":{"type":"object"},"mutates":false}]});
        if let (Value::Object(descriptor), Value::Object(extra)) = (&mut descriptor, extra) {
            descriptor.extend(extra);
        }
        descriptor
    }

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
    fn a_state_with_null_data_is_not_ready_and_no_update_may_have_one() {
        let mut protocol = TickProtocol::default();
        let read = protocol.read(json!({"seq":0,"data":null})).unwrap();
        assert!(!read.is_ready());
        assert!(
            protocol
                .update(json!({"seq":1,"data":null}))
                .unwrap_err()
                .contains("needs data")
        );
        assert_eq!(protocol.seq(), Some(0));
        assert!(
            protocol
                .update(json!({"seq":1,"data":{"value":1}}))
                .unwrap()
                .is_ready()
        );
    }

    #[test]
    fn updates_move_seq_forward_and_reads_never_move_it_back() {
        let mut protocol = TickProtocol::default();
        assert_eq!(
            protocol
                .read(json!({"seq":2,"data":{"value":2}}))
                .unwrap()
                .seq,
            2
        );
        assert!(
            protocol
                .update(json!({"seq":2,"data":{"value":2}}))
                .is_err()
        );
        assert_eq!(protocol.seq(), Some(2), "a refused update keeps the seq");
        assert_eq!(
            protocol
                .update(json!({"seq":5,"data":{"value":5}}))
                .unwrap()
                .seq,
            5
        );
        assert!(
            protocol
                .read(json!({"seq":4,"data":{"value":4}}))
                .unwrap_err()
                .contains("backward")
        );
        assert_eq!(
            protocol
                .read(json!({"seq":5,"data":{"value":5}}))
                .unwrap()
                .seq,
            5
        );
        assert_eq!(protocol.seq(), Some(5));
    }

    #[test]
    fn a_missing_at_is_the_receipt_time_and_an_invalid_one_is_refused() {
        let mut protocol = TickProtocol::default();
        let filled = protocol.update(json!({"seq":1,"data":{}})).unwrap();
        let at = filled.at.expect("the host fills in the receipt time");
        time::OffsetDateTime::parse(&at, &time::format_description::well_known::Rfc3339).unwrap();
        let reported = protocol
            .update(json!({"seq":2,"data":{},"at":"2026-10-13T08:00:01Z"}))
            .unwrap();
        assert_eq!(reported.at.as_deref(), Some("2026-10-13T08:00:01Z"));
        assert!(
            protocol
                .update(json!({"seq":3,"data":{},"at":"yesterday"}))
                .unwrap_err()
                .contains("RFC 3339")
        );
    }

    #[test]
    fn an_update_is_limited_by_its_encoded_size_not_its_line() {
        let mut protocol = TickProtocol::default();
        let values = vec!["1e15"; 100_000].join(",");
        let line = format!(r#"{{"seq":1,"data":[{values}]}}"#);
        assert!(line.len() < MAX_UPDATE_BYTES);
        let params: Value = serde_json::from_str(&line).unwrap();
        assert!(serde_json::to_vec(&params).unwrap().len() > MAX_UPDATE_BYTES);
        assert_eq!(
            protocol.update(params).unwrap_err(),
            format!("Tick update exceeds {MAX_UPDATE_BYTES} bytes once encoded")
        );
        assert_eq!(protocol.seq(), None);
    }

    #[test]
    fn a_description_is_limited_by_its_encoded_size() {
        let description = "d".repeat(MAX_DESCRIPTION_BYTES);
        assert_eq!(
            TickDescriptor::parse(counter(json!({"description":description}))).unwrap_err(),
            format!("Tick description exceeds {MAX_DESCRIPTION_BYTES} bytes once encoded")
        );
    }

    #[test]
    fn a_protocol_2_description_is_refused_by_its_version() {
        let error = TickDescriptor::parse(json!({"protocol":2,"tick_type":"example.counter","title":"Counter","description":"External","recover_with":null,"tools":[]})).unwrap_err();
        assert_eq!(
            error,
            "unsupported Tick protocol version 2; this host speaks Tick protocol 3"
        );
        assert!(TickDescriptor::parse(json!({"kind":"example.counter"})).is_err());
    }

    #[test]
    fn recovery_is_optional_and_names_a_declared_query() {
        let without = TickDescriptor::parse(counter(json!({}))).unwrap();
        assert_eq!(without.recover_with, None);
        assert!(
            serde_json::to_value(&without)
                .unwrap()
                .get("recover_with")
                .is_none()
        );
        let with = TickDescriptor::parse(counter(json!({"recover_with":"current"}))).unwrap();
        assert_eq!(with.recover_with.as_deref(), Some("current"));
        assert_eq!(with.kind, "example.counter");
        assert!(TickDescriptor::parse(counter(json!({"recover_with":"missing"}))).is_err());
    }

    #[test]
    fn a_catalog_needs_a_revision_and_valid_tool_schemas() {
        assert!(
            TickDescriptor::parse(counter(json!({"revision":""})))
                .unwrap_err()
                .contains("revision")
        );
        assert!(TickDescriptor::parse(counter(json!({"revision":4}))).is_err());
        let bad = counter(
            json!({"tools":[{"name":"bad","description":"Bad","input_schema":{"type":"not-a-type"},"mutates":true}]}),
        );
        assert!(
            TickDescriptor::parse(bad)
                .unwrap_err()
                .contains("invalid schema")
        );
        let renamed = counter(
            json!({"tools":[{"name":"old","description":"Old","params":{"type":"object"},"mutates":true}]}),
        );
        assert!(TickDescriptor::parse(renamed).is_err());
    }
}

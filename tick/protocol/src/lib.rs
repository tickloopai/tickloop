//! The Tick protocol: newline-delimited JSON-RPC between a host and one Tick Instance over the
//! Tick's stdin/stdout, with its version, methods, frame types and size limits.

mod instance;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use instance::{
    HostedRunId, HostedRunKind, InstanceRef, ShellTaskId, ShellTaskKind, artifact_instance_id,
};

pub const NATIVE_PROTOCOL_VERSION: u32 = 3;
pub const MAX_REQUEST_BYTES: usize = 512 * 1024;
pub const MAX_DESCRIPTION_BYTES: usize = 512 * 1024;
pub const MAX_UPDATE_BYTES: usize = 512 * 1024;
pub const MAX_TOOL_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
/// The range of terminal rows a Tick may declare as the fewest it stays usable at; a Tick that
/// declares none gets the fewest.
pub const MIN_SCREEN_ROWS: u16 = 6;
pub const MAX_SCREEN_ROWS: u16 = 14;
/// The identity a host launches a Tick under. A Tick may log it; no frame carries it.
pub const INSTANCE_ID_ENV: &str = "TICKLOOP_INSTANCE_ID";

pub const JSON_RPC_VERSION: &str = "2.0";
pub const DESCRIBE_METHOD: &str = "tick/describe";
pub const READ_METHOD: &str = "tick/read";
pub const CALL_METHOD: &str = "tick/call";
/// The notification a Tick sends for each Tick Update.
pub const UPDATE_METHOD: &str = "tick/update";

pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
/// The arguments do not match, or the Tick declares no such tool.
pub const INVALID_PARAMS: i64 = -32602;
pub const TOOL_FAILED: i64 = -32000;
pub const VERSION_MISMATCH: i64 = -32001;
pub const FRAME_TOO_LARGE: i64 = -32003;
/// A `tick/call` named another revision than the Tick's current catalog, so it was not run.
pub const REVISION_MISMATCH: i64 = -32004;

/// The access of a Tick running in the Tickhost. It names where the Tick runs, not a kind of
/// Tick: its protocol and tools are those of the program it runs.
pub const HOSTED_ACCESS: &str = "hosted";
/// The access of a Shell Tick: one shell command execution of an Agent.
pub const SHELL_ACCESS: &str = "shell";
/// The access of an artifact Tick: one artifact of the artifact service.
pub const ARTIFACT_ACCESS: &str = "artifact";

/// Whether `identity` is a complete `hosted:` instance identity the Tickhost could have launched
/// this Tick under, as it passes it in [`INSTANCE_ID_ENV`].
pub fn is_launched_instance_id(identity: &str) -> bool {
    identity.split_once(':').is_some_and(|(access, id)| {
        access == HOSTED_ACCESS && !id.is_empty() && !id.chars().any(char::is_control)
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
    /// What the Tick declares about its state beside the state itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<StateDeclaration>,
    /// What the Tick declares about the terminal it draws on. A host reads it once, when the
    /// Tick initializes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screen: Option<ScreenDeclaration>,
}

/// What a Tick declares about the terminal it draws on.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct ScreenDeclaration {
    /// The fewest terminal rows the Tick stays usable at, its own status line included:
    /// from [`MIN_SCREEN_ROWS`] to [`MAX_SCREEN_ROWS`]; [`MIN_SCREEN_ROWS`] when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_rows: Option<u16>,
}

/// What a Tick declares about the `data` of its state.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct StateDeclaration {
    /// The arrays in `data` ordered in time, so a host that has to shorten one keeps its newest
    /// end.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub series: Vec<StateSeries>,
}

/// An array in a state's `data` whose elements are ordered in time.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct StateSeries {
    /// JSON Pointer (RFC 6901) to the array within `data`; `""` is `data` itself.
    pub path: String,
    pub newest: SeriesEnd,
}

/// The end of a series that holds its newest element.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SeriesEnd {
    First,
    Last,
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
        if let Some(rows) = result
            .pointer("/screen/min_rows")
            .filter(|rows| !rows.is_null())
            && !rows.as_u64().is_some_and(screen_rows_are_valid)
        {
            return Err(invalid_min_rows());
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
        let series = self.state.iter().flat_map(|state| &state.series);
        for (index, declared) in series.clone().enumerate() {
            if !declared.path.is_empty() && !declared.path.starts_with('/') {
                return Err(format!(
                    "state series path {:?} is not a JSON Pointer",
                    declared.path
                ));
            }
            if series
                .clone()
                .take(index)
                .any(|prior| prior.path == declared.path)
            {
                return Err(format!("state series {:?} declared twice", declared.path));
            }
        }
        if self
            .screen
            .as_ref()
            .and_then(|screen| screen.min_rows)
            .is_some_and(|rows| !screen_rows_are_valid(u64::from(rows)))
        {
            return Err(invalid_min_rows());
        }
        Ok(())
    }

    /// The fewest terminal rows the Tick declared it stays usable at, or [`MIN_SCREEN_ROWS`].
    pub fn min_rows(&self) -> u16 {
        self.screen
            .as_ref()
            .and_then(|screen| screen.min_rows)
            .unwrap_or(MIN_SCREEN_ROWS)
    }
}

fn screen_rows_are_valid(rows: u64) -> bool {
    (u64::from(MIN_SCREEN_ROWS)..=u64::from(MAX_SCREEN_ROWS)).contains(&rows)
}

fn invalid_min_rows() -> String {
    format!("screen.min_rows must be an integer from {MIN_SCREEN_ROWS} to {MAX_SCREEN_ROWS}")
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

/// The host side of one Tick Instance's states, checked in the order frames arrive: each
/// `tick/update` moves `seq` past the previous update and has content, and each `tick/read`
/// answer is no older than the previous answer or than the updates received before it was asked.
/// A read answer is not compared with updates that arrive around it: the Tick may write an update
/// just before or just after answering a read with the same or an earlier state.
#[derive(Default)]
pub struct TickProtocol {
    update_seq: Option<u64>,
    read_seq: Option<u64>,
}

impl TickProtocol {
    /// The `seq` a `tick/read` sent now has to answer at least: the Tick wrote the latest update
    /// received so far before it could see the request.
    pub fn read_floor(&self) -> Option<u64> {
        self.update_seq
    }

    /// A `tick/read` answer, bounded only by the response frame that carried it. `floor` is the
    /// [`Self::read_floor`] of when the request was sent.
    pub fn read(&mut self, result: Value, floor: Option<u64>) -> Result<TickState, String> {
        let state = received_state(result, "Tick read")?;
        if self.read_seq.max(floor).is_some_and(|seq| state.seq < seq) {
            return Err("Tick read seq moved backward".into());
        }
        self.read_seq = Some(state.seq);
        Ok(state)
    }

    pub fn update(&mut self, params: Value) -> Result<TickState, String> {
        let state = received_state(params, "Tick update")?;
        check_encoded_size(&state, MAX_UPDATE_BYTES, "Tick update")?;
        if !state.is_ready() {
            return Err("Tick update needs data".into());
        }
        if self.update_seq.is_some_and(|seq| state.seq <= seq) {
            return Err("Tick update seq repeated or moved backward".into());
        }
        self.update_seq = Some(state.seq);
        Ok(state)
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
    fn only_complete_hosted_identities_count_as_launched() {
        assert!(is_launched_instance_id("hosted:7f1c"));
        for identity in [
            "herdr:session/w/p/1",
            "hosted:",
            "hosted",
            "shell:task-1",
            "artifact:https://a/1",
            "hosted:a\nb",
        ] {
            assert!(!is_launched_instance_id(identity), "{identity}");
        }
    }

    #[test]
    fn a_state_with_null_data_is_not_ready_and_no_update_may_have_one() {
        let mut protocol = TickProtocol::default();
        let read = protocol.read(json!({"seq":0,"data":null}), None).unwrap();
        assert!(!read.is_ready());
        assert!(
            protocol
                .update(json!({"seq":1,"data":null}))
                .unwrap_err()
                .contains("needs data")
        );
        assert_eq!(protocol.read_floor(), None);
        assert!(
            protocol
                .update(json!({"seq":1,"data":{"value":1}}))
                .unwrap()
                .is_ready()
        );
    }

    #[test]
    fn updates_and_read_answers_each_move_forward_on_their_own() {
        let mut protocol = TickProtocol::default();
        let read = |protocol: &mut TickProtocol, seq: u64, floor| {
            protocol.read(json!({"seq":seq,"data":{"value":seq}}), floor)
        };
        let update = |protocol: &mut TickProtocol, seq: u64| {
            protocol.update(json!({"seq":seq,"data":{"value":seq}}))
        };
        assert_eq!(read(&mut protocol, 2, None).unwrap().seq, 2);
        assert_eq!(
            update(&mut protocol, 2).unwrap().seq,
            2,
            "the Tick answered the read before writing the update it had published"
        );
        assert!(update(&mut protocol, 2).unwrap_err().contains("repeated"));
        assert_eq!(
            protocol.read_floor(),
            Some(2),
            "a refused update keeps the seq"
        );
        assert_eq!(update(&mut protocol, 5).unwrap().seq, 5);
        assert_eq!(
            read(&mut protocol, 3, Some(2)).unwrap().seq,
            3,
            "a read asked before update 5 arrived may answer an earlier state"
        );
        assert!(
            read(&mut protocol, 2, None)
                .unwrap_err()
                .contains("backward")
        );
        assert!(
            read(&mut protocol, 4, Some(5))
                .unwrap_err()
                .contains("backward"),
            "a read asked after update 5 arrived answers at least 5"
        );
        assert_eq!(read(&mut protocol, 5, Some(5)).unwrap().seq, 5);
        assert_eq!(protocol.read_floor(), Some(5));
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
        assert_eq!(protocol.read_floor(), None);
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
    fn a_state_may_declare_which_end_of_each_series_is_newest() {
        let without = TickDescriptor::parse(counter(json!({}))).unwrap();
        assert_eq!(without.state, None);
        assert!(
            serde_json::to_value(&without)
                .unwrap()
                .get("state")
                .is_none()
        );
        let declared = TickDescriptor::parse(counter(json!({"state":{"series":[
            {"path":"/candles","newest":"last"},
            {"path":"/news/items","newest":"first"}
        ]}})))
        .unwrap();
        assert_eq!(
            declared.state.unwrap().series,
            vec![
                StateSeries {
                    path: "/candles".into(),
                    newest: SeriesEnd::Last
                },
                StateSeries {
                    path: "/news/items".into(),
                    newest: SeriesEnd::First
                },
            ]
        );
        for refused in [
            json!({"state":{"series":[{"path":"candles","newest":"last"}]}}),
            json!({"state":{"series":[{"path":"/a","newest":"last"},{"path":"/a","newest":"first"}]}}),
            json!({"state":{"series":[{"path":"/a","newest":"middle"}]}}),
        ] {
            assert!(
                TickDescriptor::parse(counter(refused.clone())).is_err(),
                "{refused}"
            );
        }
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

    #[test]
    fn a_screen_may_declare_the_fewest_rows_from_6_to_14() {
        let without = TickDescriptor::parse(counter(json!({}))).unwrap();
        assert_eq!(without.screen, None);
        assert_eq!(without.min_rows(), MIN_SCREEN_ROWS);
        assert!(
            serde_json::to_value(&without)
                .unwrap()
                .get("screen")
                .is_none()
        );
        let empty = TickDescriptor::parse(counter(json!({"screen":{}}))).unwrap();
        assert_eq!(empty.min_rows(), 6);
        for rows in [6, 9, 14] {
            let declared =
                TickDescriptor::parse(counter(json!({"screen":{"min_rows":rows}}))).unwrap();
            assert_eq!(declared.min_rows(), rows);
        }
        for refused in [
            json!(5),
            json!(15),
            json!(6.5),
            json!(0),
            json!(-7),
            json!("8"),
        ] {
            assert_eq!(
                TickDescriptor::parse(counter(json!({"screen":{"min_rows":refused}}))).unwrap_err(),
                "screen.min_rows must be an integer from 6 to 14",
                "{refused}"
            );
        }
        let mut built = TickDescriptor::parse(counter(json!({}))).unwrap();
        built.screen = Some(ScreenDeclaration { min_rows: Some(15) });
        assert!(built.validate().is_err(), "a Tick cannot declare it either");
    }
}

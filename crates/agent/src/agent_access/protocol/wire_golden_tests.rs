//! Golden samples of the frames themselves. The payloads they carry come from the fixtures the
//! `wire_golden` integration test records, so these files pin only the framing around them. Set
//! `TICKLOOP_BLESS_WIRE_FIXTURES=1` to rewrite them after a reviewed protocol change.

use std::{fs, path::PathBuf};

use serde::{Serialize, de::DeserializeOwned};

use super::*;

const BLESS_VARIABLE: &str = "TICKLOOP_BLESS_WIRE_FIXTURES";

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/wire")
        .join(format!("{name}.json"))
}

fn pretty<T: Serialize>(value: &T) -> String {
    format!("{}\n", serde_json::to_string_pretty(value).unwrap())
}

fn recorded<T: DeserializeOwned>(name: &str) -> Vec<T> {
    let path = fixture_path(name);
    let text =
        fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

/// Serializing `value` gives the recorded bytes, and the recorded bytes decode to a value that
/// serializes to them again.
fn assert_golden<T: Serialize + DeserializeOwned>(name: &str, value: &T) {
    let path = fixture_path(name);
    let serialized = pretty(value);
    if std::env::var_os(BLESS_VARIABLE).is_some() {
        fs::write(&path, &serialized).unwrap();
    }
    let recorded =
        fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    assert!(
        serialized == recorded,
        "{name}: the serialization no longer matches {}",
        path.display()
    );
    let decoded: T = serde_json::from_str(&recorded)
        .unwrap_or_else(|error| panic!("{name}: the recorded wire no longer decodes: {error}"));
    assert!(
        pretty(&decoded) == recorded,
        "{name}: the recorded wire does not survive a round trip"
    );
}

fn id(value: u128) -> Uuid {
    Uuid::from_u128(value)
}

fn execution() -> ExecutionRef {
    ExecutionRef {
        daemon_epoch: id(0xe0),
        agent_id: id(0xa1),
        generation: 3,
    }
}

fn view() -> ViewRef {
    ViewRef {
        session_id: id(0x51),
        view_id: id(0x52),
    }
}

fn cursor() -> Cursor {
    Cursor {
        daemon_epoch: id(0xe0),
        execution: Some(execution()),
        sequence: 17,
    }
}

#[test]
fn client_messages_keep_their_recorded_wire() {
    let query = recorded::<ObservationRequest>("observation_request").remove(1);
    let command = recorded::<AgentCommand>("agent_command").remove(0);
    assert_golden(
        "client_message",
        &vec![
            ClientMessage::Hello {
                major: PROTOCOL_MAJOR,
                minor: PROTOCOL_MINOR,
            },
            ClientMessage::ShutdownDaemon {
                request_id: id(0x01),
                daemon_epoch: id(0xe0),
            },
            ClientMessage::Observe {
                request_id: id(0x02),
                query,
            },
            ClientMessage::Execute {
                request_id: id(0x03),
                command: CommandEnvelope {
                    daemon_epoch: id(0xe0),
                    command_id: id(0x81),
                    command,
                },
            },
            ClientMessage::ShowView {
                request_id: id(0x04),
                update: DisplayUpdate::Displayed {
                    view: view(),
                    expected_version: 1,
                    execution: execution(),
                    host: None,
                },
            },
            ClientMessage::ShowView {
                request_id: id(0x05),
                update: DisplayUpdate::SessionClosed {
                    session_id: id(0x51),
                },
            },
            ClientMessage::CancelObservation {
                observation_id: id(0x06),
            },
        ],
    );
}

#[test]
fn server_messages_keep_their_recorded_wire() {
    let initial = recorded::<ObservationValue>("observation_value").remove(1);
    let change = recorded::<ObservationChange>("observation_change").remove(0);
    let result = recorded::<CommandResult>("command_result").remove(0);
    let error = AccessError {
        code: AccessErrorCode::VersionMismatch,
        message: "business major 10 is not supported".into(),
        latest_permissions: None,
        path: None,
    };
    assert_golden(
        "server_message",
        &vec![
            ServerMessage::Hello(ServerHello {
                process_id: 4242,
                major: PROTOCOL_MAJOR,
                minor: PROTOCOL_MINOR,
                daemon_epoch: id(0xe0),
                session_id: id(0x51),
                capabilities: vec![
                    AccessCapability::Overview,
                    AccessCapability::ChunkedResponses,
                ],
                limits: ProtocolLimits {
                    frame_bytes: 134_217_728,
                    source_items: 4096,
                    feed_items: 1024,
                    feed_bytes: 8_388_608,
                    change_bytes: 1_048_576,
                    receipt_items: 4096,
                    receipt_seconds: 600,
                    view_lease_seconds: 45,
                    page_items: 256,
                    default_page_items: 100,
                    chunk_bytes: 4_194_304,
                },
            }),
            ServerMessage::Opened {
                request_id: id(0x02),
                observation_id: id(0x06),
                initial,
                cursor: cursor(),
                reset: Some(ResetReason::EpochChanged),
                feed: true,
            },
            ServerMessage::Changed {
                observation_id: id(0x06),
                update: ObservationUpdate {
                    cursor: cursor(),
                    change,
                },
            },
            ServerMessage::Receipt {
                request_id: id(0x03),
                receipt: CommandReceipt {
                    daemon_epoch: id(0xe0),
                    command_id: id(0x81),
                    target: Some(execution()),
                    state: ReceiptState::Completed,
                    result: Some(result),
                    error: None,
                    persistence: PersistenceState::FlushObserved,
                },
            },
            ServerMessage::Display {
                request_id: id(0x04),
                receipt: DisplayReceipt {
                    view: view(),
                    version: 2,
                    displayed: true,
                },
            },
            ServerMessage::ObservationClosed {
                observation_id: id(0x06),
                error: error.clone(),
            },
            ServerMessage::Error {
                request_id: None,
                error,
            },
            ServerMessage::ResponseStart {
                request_id: id(0x02),
                transfer_id: id(0x07),
                kind: ChunkedResponseKind::Opened,
                total_bytes: 10,
                chunk_bytes: 8,
            },
            ServerMessage::ResponseChunk {
                transfer_id: id(0x07),
                offset: 8,
                base64: "e30=".into(),
            },
            ServerMessage::ResponseEnd {
                transfer_id: id(0x07),
                total_bytes: 10,
                sha256: "44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a".into(),
            },
            ServerMessage::DaemonStopped {
                request_id: id(0x01),
            },
        ],
    );
}

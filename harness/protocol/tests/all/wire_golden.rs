//! Golden Harness and Transcript wire samples. The fixtures under `tests/fixtures/wire` record
//! the serialization the daemon, workers and Views exchange, and the frames that carry it; a change
//! to any of them is a protocol change and must be reviewed against the handshake rules of ADR 0013
//! and 0021. Set `TICKLOOP_BLESS_WIRE_FIXTURES=1` to rewrite the files after such a reviewed change.

use std::{fs, path::PathBuf};

use serde::{Serialize, de::DeserializeOwned};
use serde_json::json;
use uuid::Uuid;

use tick_protocol::InstanceRef;
use tickloop_harness_protocol::{
    Adapter, ChatSnapshot, ConnectionRegion, Content, Decision, ExecutionMode, ImageMediaType,
    ImageSource, InputModality, Message, ModelAuthSource, ModelCard, ModelConfig, ModelDiscovery,
    ModelProvider, MonitorMember, MonitorMembership, PermissionPolicy, PermissionRule, ProductKind,
    Protocol, ProviderConnection, ReasoningEffort, Role, ThinkingBudgets, ThinkingControl,
    ToolResultBlock, ToolResultContent,
};

const BLESS_VARIABLE: &str = "TICKLOOP_BLESS_WIRE_FIXTURES";

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/wire")
        .join(format!("{name}.json"))
}

fn pretty<T: Serialize>(value: &T) -> String {
    format!("{}\n", serde_json::to_string_pretty(value).unwrap())
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

fn hosted_instance() -> InstanceRef {
    InstanceRef::hosted(id(0x40))
}

fn terminal_instance() -> InstanceRef {
    InstanceRef::hosted(id(0x41))
}

fn model_config() -> ModelConfig {
    ModelConfig {
        connection: Some(ProviderConnection {
            vendor: "Example".into(),
            product: "Example Plan".into(),
            kind: ProductKind::Plan,
            order: 2,
            region: Some(ConnectionRegion {
                name: "Global".into(),
                order: 1,
            }),
            discovery: ModelDiscovery::ProviderApi,
        }),
        name: "example-plan".into(),
        protocol: Protocol::Messages,
        base_url: "https://models.example.test/v1".into(),
        model: "example-large".into(),
        api_key_env: "EXAMPLE_API_KEY".into(),
        max_output_tokens: 32_000,
        provider: ModelProvider::Api,
        auth_source: ModelAuthSource::ApiKeyEnv,
        provider_id: "example".into(),
        provider_name: "Example".into(),
        model_id: "example-large".into(),
        card: Some(ModelCard {
            id: "example-large".into(),
            display_name: "Example Large".into(),
            adapter: Adapter::Messages,
            max_output_tokens: 32_000,
            context_window_tokens: Some(200_000),
            thinking: ThinkingControl::Toggle,
            adaptive_by_default: false,
            effort_levels: vec![ReasoningEffort::Low, ReasoningEffort::High],
            input_modalities: vec![InputModality::Text, InputModality::Image],
            temperature_control: None,
            thinking_off: None,
            native_tool_search: None,
            thinking_budgets: Some(ThinkingBudgets {
                low: 2048,
                medium: 8192,
                high: 16384,
            }),
        }),
        input_modalities: vec![InputModality::Text, InputModality::Image],
    }
}

#[test]
fn recorded_model_cards_load_the_retired_duplicate_field_without_reintroducing_it() {
    let model = model_config();
    let mut recorded = serde_json::to_value(&model).unwrap();
    recorded["card"]["service_model_id"] = json!(model.model);
    assert_eq!(
        serde_json::from_value::<ModelConfig>(recorded.clone()).unwrap(),
        model
    );
    assert!(serde_json::from_value::<ModelCard>(recorded["card"].clone()).is_err());
    assert!(
        serde_json::to_value(model).unwrap()["card"]
            .get("service_model_id")
            .is_none()
    );
}

fn permission_rule(value: u128, decision: Decision, persistent: bool) -> PermissionRule {
    PermissionRule {
        id: id(value),
        decision,
        tool: "Bash".into(),
        specifier: Some("git status".into()),
        input: Some(json!({"command": "git status"})),
        loop_id: (!persistent).then(|| id(0x10)),
        persistent,
    }
}

fn permission_policy() -> PermissionPolicy {
    PermissionPolicy {
        mode: ExecutionMode::AcceptEdits,
        rules: vec![permission_rule(0x71, Decision::Allow, true)],
        project_constraints: vec![permission_rule(0x72, Decision::Ask, false)],
        managed_constraints: vec![permission_rule(0x73, Decision::Deny, true)],
        // Never on the wire: the golden shows none of these.
        host_rules: vec![permission_rule(0x74, Decision::Allow, false)],
        plan_without_auto: true,
        mode_before_plan: Some(ExecutionMode::Auto),
        launch_rules: Vec::new(),
        revision: 4,
    }
}

fn png() -> ImageSource {
    ImageSource::Base64 {
        media_type: ImageMediaType::Png,
        data: "iVBORw0KGgo=".into(),
    }
}

fn messages() -> Vec<Message> {
    vec![
        Message {
            role: Role::System,
            content: vec![Content::Text {
                text: "You are Tickloop.".into(),
            }],
        },
        Message {
            role: Role::User,
            content: vec![
                Content::Text {
                    text: "Check the counter".into(),
                },
                Content::Image {
                    source: png(),
                    width: 1,
                    height: 1,
                },
            ],
        },
        Message {
            role: Role::Assistant,
            content: vec![
                Content::Thinking {
                    thinking: "Read it first.".into(),
                    signature: Some("sig-1".into()),
                },
                Content::Thinking {
                    thinking: "Unsigned.".into(),
                    signature: None,
                },
                Content::ToolUse {
                    id: "call-1".into(),
                    name: "TickRead".into(),
                    input: json!({"instance": "hosted:00000000-0000-0000-0000-000000000041"}),
                },
                Content::ProtocolData {
                    protocol: Protocol::Responses,
                    value: json!({"encrypted": "opaque"}),
                },
            ],
        },
        Message {
            role: Role::User,
            content: vec![
                Content::ToolResult {
                    tool_use_id: "call-1".into(),
                    content: ToolResultContent::Text("7".into()),
                    is_error: false,
                },
                Content::ToolResult {
                    tool_use_id: "call-2".into(),
                    content: ToolResultContent::Blocks(vec![
                        ToolResultBlock::Text {
                            text: "chart".into(),
                        },
                        ToolResultBlock::Image {
                            source: png(),
                            width: 2,
                            height: 2,
                        },
                    ]),
                    is_error: true,
                },
            ],
        },
    ]
}

/// A Chat as the Transcript and the wire carry it, decoded from a seed that leaves defaulted
/// fields out so that the defaults are part of the golden shape.
fn chat_snapshot() -> ChatSnapshot {
    let mut snapshot: ChatSnapshot = serde_json::from_value(json!({
        "id": id(0xc1),
        "folder": "/work/project",
        "messages": messages(),
        "generated_title": "Counter check",
        "artifact_ticks": [{
            "artifact_id": "doc",
            "origin": "https://claude.ai",
            "title": "Plan",
            "format": "html",
            "url": "https://claude.ai/artifact/doc",
            "access_role": "writer",
            "added_at_ms": 1_000,
        }],
        "delegation": {
            "delegation_id": id(0xd1),
            "parent_chat_id": id(0xc0),
            "parent_loop_id": id(0x10),
            "parent_run_id": id(0x20),
            "parent_call_id": "call-subagent",
            "task": "Audit the counter",
            "ceiling": {
                "folder": "/work/project",
                "additional_directories": ["/work/shared"],
                "policy": permission_policy(),
                "session_denials": [{
                    "tool": "Bash",
                    "identity_sha256": "ab".repeat(32),
                    "workspace": {"folder": "/work/project", "additional_directories": []},
                }],
                "host_context": {
                    "folder": "/work/project",
                    "workspace_id": "w1",
                    "interaction": {
                        "socket": "/run/herdr.sock",
                        "pane": {
                            "pane_id": "p1",
                            "terminal_id": "term-1",
                            "workspace_id": "w1",
                            "tab_id": "t1",
                            "cwd": "/work/project",
                        },
                    },
                },
                "host_tools": {
                    "ReadWorkspace": [
                        {"kind": "native", "endpoint": {"kind": "herdr_socket", "path": "/run/herdr.sock", "owner": 501, "device": 16_777_220, "inode": 42}},
                    ],
                    "TickRead": [
                        {"kind": "native", "endpoint": {"kind": "tickhost", "endpoint": "/run/tickhost.sock", "uid": 501}},
                    ],
                    "TickCall": [{"kind": "mcp", "server_id": "calendar", "account_id": "me"}],
                },
                "tick_instances": [terminal_instance(), hosted_instance()],
            },
        },
        "family_completions": [{
            "notification": {"rejected": {"code": "stale_target", "message": "The receiving parent is unavailable"}},
            "usage": {"input_tokens": 11, "output_tokens": 7},
            "child_chat_id": id(0xc2),
            "child_loop_id": id(0x11),
            "child_run_id": id(0x21),
            "handle": "audit",
            "state": "Idle",
            "result": "Actual child report",
            "error": null,
            "started_at": 1,
            "finished_at": 2,
            "triggering_message_ids": [id(0x91)],
        }],
        "message_runs": {"2": id(0x20)},
        "message_ids": {"1": id(0x91), "2": id(0x92)},
        "prompt_execution_modes": {"1": "acceptEdits"},
        "inherited_prompts": [1],
        "message_usage": {"2": {"input_tokens": 120, "output_tokens": 30, "cache_read_input_tokens": 100}},
        "message_models": {"2": {"profile": "example-plan", "protocol": "messages", "requested_model": "example-large"}},
        "chat_loop": {
            "id": id(0x10),
            "state": "Idle",
            "run_id": id(0x20),
            "run_started_at": 1_000,
            "state_since": 2_000,
            "receiving": {"state": "paused", "reason": "stopped"},
            "subscriptions": [{
                "id": id(0x31),
                "instance": hosted_instance(),
                "frequency": "medium",
                "authority": null,
            }],
        },
    }))
    .unwrap();
    snapshot.monitor = MonitorMembership {
        members: vec![MonitorMember {
            is_terminal: false,
            instance: terminal_instance(),
            title: Some("Counter".into()),
            min_rows: None,
        }],
        removed: [hosted_instance()].into(),
        handled: [(terminal_instance(), 7_500)].into(),
        observed: [(terminal_instance(), 7_900)].into(),
        taken_in: [(terminal_instance(), 7_500)].into(),
        used: [(terminal_instance(), 7_800)].into(),
    };
    snapshot.model = model_config();
    snapshot.policy = permission_policy();
    snapshot.reasoning_effort = Some(ReasoningEffort::High);
    snapshot.additional_directories = vec!["/work/shared".into()];
    snapshot.last_sequence = 9;
    snapshot.context_window_tokens = Some(200_000);
    snapshot.last_model_run = Some(tickloop_ids::Id::from(id(0x20)));
    snapshot
        .pending_thinking
        .insert(tickloop_ids::Id::from(id(0x20)), "Reading".into());
    snapshot
}

#[test]
fn transcript_chat_snapshots_keep_their_recorded_wire() {
    assert_golden("chat_snapshot", &chat_snapshot());
}

/// A measurement recorded before token counting holds bytes alone; one counted with a
/// tokenizer adds its token counts beside them, so older readers still load it.
#[test]
fn transcript_context_measurements_keep_their_recorded_wire() {
    let snapshot = chat_snapshot();
    let bytes = tickloop_harness_protocol::ContextMeasurement::capture(&snapshot, 12_000, 3_000);
    assert_golden(
        "context_measurement",
        &vec![bytes.clone(), bytes.with_tokens(5_200, 1_400)],
    );
}

#[test]
fn transcript_messages_keep_their_recorded_wire() {
    assert_golden("message", &messages());
}

#[test]
fn task_board_mode_preserves_legacy_teams_and_new_solo_boards() {
    let mut board = tickloop_harness_protocol::TaskBoard::new(id(0xc1).into(), id(0x10).into());
    assert!(!board.is_team);
    let mut legacy = serde_json::to_value(&board).unwrap();
    legacy.as_object_mut().unwrap().remove("is_team");
    legacy.as_object_mut().unwrap().remove("task_number_offset");
    let restored = serde_json::from_value::<tickloop_harness_protocol::TaskBoard>(legacy).unwrap();
    assert!(restored.is_team);
    assert_eq!(restored.task_number_offset, 0);
    let solo = board.clone();
    board.is_team = true;
    assert_golden("task_board", &vec![solo, board]);
}

#[test]
fn approval_status_events_round_trip() {
    use tickloop_harness_protocol::{
        ApprovalPath, ApprovalRecord, ApprovalResult, ApprovalReviewerKind, EventKind,
    };
    let record: ApprovalRecord = serde_json::from_value(json!({
        "id":id(1),"chat_id":id(2),"loop_id":id(3),"run_id":id(4),"operation_id":id(5),
        "call_id":"call-write","at_ms":1,"tool":"Write","input_summary":{"text":"file","truncated":false},
        "outcome":ApprovalResult::Allow,"path":ApprovalPath::Human,"reason":"Human approved this operation",
        "human_reason":"Explicit Ask rule requires confirmation","approval_source":"explicit_ask",
        "policy_revision":1,"matching_rules":[],"review":null
    })).unwrap();
    let events = vec![
        EventKind::ApprovalReviewStarted {
            call_id: "call-write".into(),
            reviewer: ApprovalReviewerKind::Jev,
        },
        EventKind::ApprovalReviewStarted {
            call_id: "call-write".into(),
            reviewer: ApprovalReviewerKind::Llm,
        },
        EventKind::ApprovalRecorded {
            record: Box::new(record),
        },
    ];
    assert_golden("approval-status-events", &events);
}

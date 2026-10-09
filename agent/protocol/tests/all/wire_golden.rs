//! Golden AgentAccess and Transcript wire samples. The fixtures under `tests/fixtures/wire` record
//! the serialization the daemon, workers and Views exchange, and the frames that carry it; a change
//! to any of them is a protocol change and must be reviewed against the handshake rules of ADR 0013
//! and 0021. Set `TICKLOOP_BLESS_WIRE_FIXTURES=1` to rewrite the files after such a reviewed change.

use std::{fs, path::PathBuf};

use serde::{Serialize, de::DeserializeOwned};
use serde_json::json;
use uuid::Uuid;

use tick_protocol::InstanceRef;
use tickloop_agent_protocol::{
    AccessCapability, AccessError, AccessErrorCode, Adapter, AgentCommand, AgentDetailQuery,
    AgentFilter, AgentHostOptions, AgentSummary, ArtifactTick, ChatSnapshot, ChunkedResponseKind,
    ClientMessage, CommandEnvelope, CommandReceipt, CommandResult, ConfigurationChange,
    ConnectionRegion, Content, Cursor, Decision, DisplayReceipt, DisplayUpdate, DisplayedView,
    EnvironmentValue, ExecutionAvailability, ExecutionMode, ExecutionRef, HostLocation,
    HostWorkContext, HostedMember, HostedMemberState, HostedResources, HostedScreenSnapshot,
    HostedSnapshot, HostedViewGrant, ImageMediaType, ImageSource, InputModality, InteractionSource,
    LaunchSpec, LiveChatSnapshot, LoopAction, Message, ModelAuthSource, ModelCard, ModelConfig,
    ModelDiscovery, ModelProvider, MonitorMember, MonitorMemberSource, MonitorMembership,
    ObservationChange, ObservationRequest, ObservationUpdate, ObservationValue, PROTOCOL_MAJOR,
    PROTOCOL_MINOR, Page, PendingRequestKind, PermissionPolicy, PermissionRule, PermissionScope,
    PersistenceState, ProductKind, Protocol, ProtocolLimits, ProviderConnection,
    ProviderCredentialScope, ReasoningEffort, ReceiptState, RequestRef, ResetReason, Role,
    ServerHello, ServerMessage, ThinkingBudgets, ThinkingControl, ToolResultBlock,
    ToolResultContent, TranscriptRef, UserInput, ViewRef,
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

fn herdr_instance() -> InstanceRef {
    InstanceRef::native("session/workspace/pane/counter").unwrap()
}

fn hosted_instance() -> InstanceRef {
    InstanceRef::hosted(id(0x40))
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
            service_model_id: "example-large-2026".into(),
            display_name: "Example Large".into(),
            adapter: Adapter::Messages,
            max_output_tokens: 32_000,
            context_window_tokens: Some(200_000),
            thinking: ThinkingControl::Toggle,
            adaptive_by_default: false,
            effort_levels: vec![ReasoningEffort::Low, ReasoningEffort::High],
            input_modalities: vec![InputModality::Text, InputModality::Image],
            thinking_budgets: Some(ThinkingBudgets {
                low: 2048,
                medium: 8192,
                high: 16384,
            }),
        }),
        input_modalities: vec![InputModality::Text, InputModality::Image],
    }
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
        revision: 4,
    }
}

fn host_location() -> HostLocation {
    HostLocation {
        host_instance: "herdr:16777220:42".into(),
        socket: "/run/herdr.sock".into(),
        workspace: Some("w1".into()),
        tab: Some("t1".into()),
        pane: Some("p1".into()),
        terminal: Some("term-1".into()),
        client: None,
    }
}

fn interaction_source() -> InteractionSource {
    InteractionSource {
        view: ViewRef {
            session_id: id(0x51),
            view_id: id(0x52),
        },
        display_version: 2,
        host: Some(host_location()),
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
                text: "You are Tick Loop.".into(),
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
                    name: "ReadTick".into(),
                    input: json!({"instance": "herdr:counter"}),
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
        "workdir": "/work/project",
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
                "workdir": "/work/project",
                "additional_directories": ["/work/shared"],
                "policy": permission_policy(),
                "session_denials": [{
                    "tool": "Bash",
                    "identity_sha256": "ab".repeat(32),
                    "workspace": {"workdir": "/work/project", "additional_directories": []},
                }],
                "host_context": {
                    "workdir": "/work/project",
                    "workspace_id": "w1",
                    "interaction": {
                        "socket": "/run/herdr.sock",
                        "pane": {
                            "pane_id": "p1",
                            "terminal_id": "term-1",
                            "workspace_id": "w1",
                            "tab_id": "t1",
                            "cwd": "/work/project",
                            "tokens": {"agent": "tickloop"},
                        },
                    },
                },
                "host_tools": {
                    "ReadTick": [
                        {"kind": "native", "endpoint": {"kind": "herdr_socket", "path": "/run/herdr.sock", "owner": 501, "device": 16_777_220, "inode": 42}},
                        {"kind": "native", "endpoint": {"kind": "tickhost", "endpoint": "/run/tickhost.sock", "uid": 501}},
                    ],
                    "CallTickTool": [{"kind": "mcp", "server_id": "calendar", "account_id": "me"}],
                },
                "tick_instances": [herdr_instance(), hosted_instance()],
            },
        },
        "family_completions": [{
            "notification": {"rejected": {"code": "stale_target", "message": "The receiving parent is stopped"}},
            "usage": {"input_tokens": 11, "output_tokens": 7},
            "child_chat_id": id(0xc2),
            "child_loop_id": id(0x11),
            "child_run_id": id(0x21),
            "handle": "audit",
            "state": "Completed",
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
        "loops": [{
            "id": id(0x10),
            "goal": "Watch the counter",
            "state": "WaitingTrigger",
            "run_id": id(0x20),
            "run_started_at": 1_000,
            "state_since": 2_000,
            "trigger": {
                "id": id(0x30),
                "prompt": "Report changes",
                "interval_seconds": 60,
                "expires_at_ms": null,
                "accepts_events": true,
                "armed": true,
            },
        }],
    }))
    .unwrap();
    snapshot.monitor = MonitorMembership {
        members: vec![MonitorMember {
            instance: herdr_instance(),
            source: MonitorMemberSource::Added,
            title: Some("Counter".into()),
        }],
        removed: [hosted_instance()].into(),
    };
    snapshot.model = model_config();
    snapshot.policy = permission_policy();
    snapshot.reasoning_effort = Some(ReasoningEffort::High);
    snapshot.additional_directories = vec!["/work/shared".into()];
    snapshot.last_sequence = 9;
    snapshot.context_window_tokens = Some(200_000);
    snapshot.last_model_run = Some(id(0x20));
    snapshot.pending_thinking.insert(id(0x20), "Reading".into());
    snapshot
}

fn live_chat_snapshot() -> LiveChatSnapshot {
    let mut live =
        LiveChatSnapshot::from_snapshot(execution(), chat_snapshot(), vec!["slow disk".into()]);
    live.pending_model = Some(model_config());
    live.transcript_write_error = Some("disk full".into());
    live
}

fn launch_spec() -> LaunchSpec {
    LaunchSpec {
        workdir: "/work/project".into(),
        config_root: Some("/home/user/.tickloop".into()),
        model: Some(model_config()),
        host_options: AgentHostOptions::embedded_defaults(),
        session_persistence: true,
        exact_chat: Some(id(0xc1)),
        environment: vec![EnvironmentValue {
            name: b"TERM".to_vec(),
            value: b"xterm-256color".to_vec(),
        }],
        host_work_context: Some(HostWorkContext {
            socket: "/run/herdr.sock".into(),
            workspace: Some("w1".into()),
        }),
    }
}

fn access_error() -> AccessError {
    AccessError {
        code: AccessErrorCode::UntrustedWorkingDirectory,
        message: "trust /work/project first".into(),
        latest_permissions: None,
        path: Some("/work/project".into()),
    }
}

fn hosted_members() -> Vec<HostedMember> {
    vec![
        HostedMember {
            instance: hosted_instance(),
            title: Some("Quotes".into()),
            state: HostedMemberState::Running,
            resources: Some(HostedResources {
                business_pid: 4242,
                cpu_time_ms: Some(1_500),
                rss_bytes: None,
                sampled_at_ms: 9_000,
            }),
        },
        HostedMember {
            instance: InstanceRef::hosted(id(0x41)),
            title: None,
            state: HostedMemberState::Lost,
            resources: None,
        },
    ]
}

fn monitor_membership() -> MonitorMembership {
    MonitorMembership {
        members: vec![MonitorMember {
            instance: hosted_instance(),
            source: MonitorMemberSource::Opened,
            title: Some("Quotes".into()),
        }],
        removed: [herdr_instance()].into(),
    }
}

#[test]
fn agent_commands_keep_their_recorded_wire() {
    let request = RequestRef {
        execution: execution(),
        request_id: id(0x61),
        kind: PendingRequestKind::Permission,
        version: 1,
    };
    assert_golden(
        "agent_command",
        &vec![
            AgentCommand::Create {
                launch: launch_spec(),
                initial: Some(UserInput::Text {
                    text: "Check the counter".into(),
                    contexts: serde_json::from_value(json!([
                        {"kind": "file", "path": "/work/project/notes.md", "content": "notes", "truncated": false},
                        {"kind": "image", "origin": {"type": "pasted", "id": 1}, "image": {"media_type": "image/png", "data": "iVBORw0KGgo=", "width": 1, "height": 1}},
                    ]))
                    .unwrap(),
                    paths: vec!["/work/project/notes.md".into()],
                }),
                origin: Some(interaction_source()),
            },
            AgentCommand::Submit {
                execution: execution(),
                target_loop: Some(id(0x10)),
                input: UserInput::Skill {
                    name: "tickloop".into(),
                    arguments: "status".into(),
                },
                origin: None,
            },
            AgentCommand::Configure {
                execution: execution(),
                change: ConfigurationChange::Model {
                    config: model_config(),
                    thinking: Some(None),
                },
                provider: None,
            },
            AgentCommand::Configure {
                execution: execution(),
                change: ConfigurationChange::ReplaceRules {
                    rules: permission_policy().rules,
                },
                provider: None,
            },
            AgentCommand::DecidePermission {
                request,
                allow: true,
                scope: PermissionScope::Session,
                comment: None,
                rule: Some("Bash(git status)".into()),
            },
            AgentCommand::ManageLoop {
                execution: execution(),
                action: LoopAction::ConfigureTrigger {
                    loop_id: id(0x10),
                    trigger: Some(
                        serde_json::from_value(json!({
                            "id": id(0x30),
                            "prompt": "Report changes",
                            "interval_seconds": 60,
                            "expires_at_ms": null,
                            "accepts_events": false,
                            "armed": true,
                        }))
                        .unwrap(),
                    ),
                },
            },
            AgentCommand::AddMonitorMember {
                execution: execution(),
                instance: herdr_instance(),
                title: Some("Counter".into()),
            },
            AgentCommand::HostedTickSnapshot {
                execution: execution(),
                instance: hosted_instance(),
            },
            AgentCommand::LoadRecord {
                record: TranscriptRef {
                    store_root: "/home/user/.tickloop/chats".into(),
                    chat_id: id(0xc1),
                },
                as_new: false,
                launch: None,
            },
            AgentCommand::FlushTranscripts,
        ],
    );
}

#[test]
fn observation_requests_keep_their_recorded_wire() {
    let cursor = Cursor {
        daemon_epoch: id(0xe0),
        execution: Some(execution()),
        sequence: 17,
    };
    assert_golden(
        "observation_request",
        &vec![
            ObservationRequest::Overview {
                filter: AgentFilter {
                    workdir: Some("/work/project".into()),
                    workspace: None,
                },
                page: Page {
                    offset: 0,
                    limit: 50,
                },
                after: None,
            },
            ObservationRequest::Agent {
                execution: execution(),
                detail: AgentDetailQuery::AccountModels {
                    config: Box::new(model_config()),
                    refresh: true,
                },
                after: Some(cursor),
            },
            ObservationRequest::Agent {
                execution: execution(),
                detail: AgentDetailQuery::Snapshot,
                after: None,
            },
            ObservationRequest::Transcript {
                record: TranscriptRef {
                    store_root: "/home/user/.tickloop/chats".into(),
                    chat_id: id(0xc1),
                },
                page: Page::default(),
            },
            ObservationRequest::Receipt {
                epoch: id(0xe0),
                command_id: id(0x81),
            },
        ],
    );
}

#[test]
fn observation_values_keep_their_recorded_wire() {
    let summary = AgentSummary {
        availability: ExecutionAvailability::Live,
        execution: execution(),
        chat_id: id(0xc1),
        workdir: "/work/project".into(),
        state: tickloop_agent_protocol::LoopState::Running,
        run_id: Some(id(0x20)),
        title: "Counter check".into(),
        subagent: None,
        has_work: Some(true),
        updated_at: Some(2_000),
        preview: None,
        waiting_permissions: 1,
        waiting_questions: 0,
        parent: None,
        record: None,
        workspace: Some("w1".into()),
        capabilities: vec!["Submit".into()],
        views: vec![DisplayedView {
            view: interaction_source().view,
            version: 2,
            execution: execution(),
            host: Some(host_location()),
        }],
    };
    assert_golden(
        "observation_value",
        &vec![
            ObservationValue::Agent(Box::new(live_chat_snapshot())),
            ObservationValue::Overview(vec![summary]),
            ObservationValue::ProviderCredentials(Box::new(ProviderCredentialScope {
                config_root: Some("/home/user/.tickloop".into()),
                environment_keys: vec!["EXAMPLE_API_KEY".into()],
                service_account_path: Ok(None),
                service_account_configured: Err("unreadable".into()),
            })),
            ObservationValue::AccountModelsPending,
            ObservationValue::AccountModels(Err("signed out".into())),
            ObservationValue::Skills {
                available: Some(true),
                warnings: vec![],
            },
            ObservationValue::RewindFiles(None),
            ObservationValue::Receipt(Some(Box::new(CommandReceipt {
                daemon_epoch: id(0xe0),
                command_id: id(0x81),
                target: Some(execution()),
                state: ReceiptState::Failed,
                result: None,
                error: Some(access_error()),
                persistence: PersistenceState::WriteFailed("disk full".into()),
            }))),
        ],
    );
}

#[test]
fn observation_changes_keep_their_recorded_wire() {
    assert_golden(
        "observation_change",
        &vec![
            ObservationChange::Event(
                serde_json::from_value(json!({
                    "sequence": 10,
                    "chat_id": id(0xc1),
                    "loop_id": id(0x10),
                    "run_id": id(0x20),
                    "kind": {"ToolStarted": {"call_id": "call-1", "name": "ReadTick", "input": {"instance": "herdr:counter"}}},
                }))
                .unwrap(),
            ),
            ObservationChange::Event(
                serde_json::from_value(json!({
                    "sequence": 11,
                    "chat_id": id(0xc1),
                    "loop_id": id(0x10),
                    "run_id": id(0x20),
                    "kind": {"Completed": {"usage": {"input_tokens": 120, "output_tokens": 30}}},
                }))
                .unwrap(),
            ),
            ObservationChange::Changed,
            ObservationChange::Resync {
                reason: ResetReason::Lagged,
            },
            ObservationChange::Reset {
                reason: ResetReason::ConversationChanged,
                snapshot: Box::new(live_chat_snapshot()),
            },
        ],
    );
}

#[test]
fn command_results_keep_their_recorded_wire() {
    assert_golden(
        "command_result",
        &vec![
            CommandResult::HostedTicks(hosted_members()),
            CommandResult::HostedTickSnapshot(HostedSnapshot {
                screen: HostedScreenSnapshot {
                    lines: vec!["AAPL 182.5".into(), "MSFT 401.2".into()],
                    truncated: true,
                    rows: 30,
                    columns: 120,
                    alternate_screen: false,
                },
                taken_at_ms: 7_000,
            }),
            CommandResult::HostedTickReopened {
                instance: hosted_instance(),
                membership: monitor_membership(),
            },
            CommandResult::HostedTickView(HostedViewGrant {
                endpoint: "/run/tickhost.sock".into(),
                host_epoch: id(0xe1),
                instance: hosted_instance(),
                view: id(0x53),
            }),
            CommandResult::ArtifactTicks(vec![ArtifactTick {
                artifact_id: "doc".into(),
                origin: "https://claude.ai".into(),
                title: "Plan".into(),
                format: "html".into(),
                url: "https://claude.ai/artifact/doc".into(),
                access_role: "writer".into(),
                added_at_ms: 1_000,
            }]),
            CommandResult::Created {
                execution: execution(),
                snapshot: Box::new(live_chat_snapshot()),
                submitted: Some(
                    serde_json::from_value(json!({
                        "chat_id": id(0xc1),
                        "loop_id": id(0x10),
                        "run_id": id(0x20),
                    }))
                    .unwrap(),
                ),
                shell: None,
            },
            CommandResult::Queued {
                command_id: id(0x81),
            },
            CommandResult::Text("ok".into()),
            CommandResult::Done,
        ],
    );
}

#[test]
fn transcript_chat_snapshots_keep_their_recorded_wire() {
    assert_golden("chat_snapshot", &chat_snapshot());
}

#[test]
fn transcript_messages_keep_their_recorded_wire() {
    assert_golden("message", &messages());
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

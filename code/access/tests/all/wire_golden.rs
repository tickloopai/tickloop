//! Golden AgentAccess and Transcript wire samples. The fixtures under `tests/fixtures/wire` record
//! the serialization the daemon, workers and Views exchange, and the frames that carry it; a change
//! to any of them is a protocol change and must be reviewed against the handshake rules of ADR 0013
//! and 0021. Set `TICKLOOP_BLESS_WIRE_FIXTURES=1` to rewrite the files after such a reviewed change.

use std::{fs, path::PathBuf};

use serde::{Serialize, de::DeserializeOwned};
use serde_json::json;
use uuid::Uuid;

use tick_protocol::InstanceRef;
use tickloop_code_access::{
    AccessCapability, AccessError, AccessErrorCode, AgentCommand, AgentDetailQuery, AgentFilter,
    AgentSummary, ChunkedResponseKind, ClientMessage, CommandEnvelope, CommandReceipt,
    CommandResult, ConfigurationChange, Cursor, DisplayReceipt, DisplayUpdate, DisplayedView,
    EnvironmentValue, ExecutionAvailability, ExecutionRef, HistoryAction, HostLocation,
    HostWorkContext, InteractionSource, LaunchSpec, LiveChatSnapshot, LoopAction,
    ObservationChange, ObservationRequest, ObservationUpdate, ObservationValue, PROTOCOL_MAJOR,
    PROTOCOL_MINOR, Page, PendingRequestKind, PersistenceState, ProtocolLimits, ReceiptState,
    RequestRef, ResetReason, ServerHello, ServerMessage, TranscriptRef, UserInput, ViewRef,
};
use tickloop_harness::protocol::mcp::{McpServerState, McpServerStatus, McpToolSummary};
use tickloop_harness::protocol::{
    Adapter, AgentHostOptions, ArtifactTick, ChatSnapshot, ConnectionRegion, Content, Decision,
    Event, EventKind, ExecutionMode, HostedMember, HostedMemberState, HostedProgramExit,
    HostedResources, HostedViewGrant, ImageMediaType, ImageSource, InputModality, Message,
    ModelAuthSource, ModelCard, ModelConfig, ModelDiscovery, ModelProvider, MonitorMember,
    MonitorMembership, ObservationKind, OperationRecord, OperationState, OtherTickhost,
    PermissionPolicy, PermissionRule, PermissionScope, ProductKind, Protocol, ProviderConnection,
    ProviderCredentialScope, ReasoningEffort, Role, TakenBackPrompt, ThinkingBudgets,
    ThinkingControl, TickLinkState, TickSubscriptionChange, TickSubscriptionFrequency,
    TickSubscriptionStatus, TickhostOccupancy, ToolResult, ToolResultBlock, ToolResultContent,
};

#[test]
fn worker_runtime_scope_defaults_to_unknown_for_older_daemons_and_round_trips_when_present() {
    use tickloop_code_access::WorkerRuntimeVersion;
    let legacy = json!({
        "process_id": 4242,
        "executable": "/runtime/tickloop",
        "build_id": "previous-build",
        "upgrading": true,
    });
    let mut worker: WorkerRuntimeVersion = serde_json::from_value(legacy.clone()).unwrap();
    assert!(worker.executions.is_empty());
    assert!(worker.error.is_none());
    assert_eq!(serde_json::to_value(&worker).unwrap(), legacy);
    let root = ExecutionRef {
        daemon_epoch: Uuid::from_u128(1),
        agent_id: Uuid::from_u128(2),
        generation: 3,
    };
    let child = ExecutionRef {
        agent_id: Uuid::from_u128(4),
        ..root
    };
    worker.executions = vec![root, child];
    worker.error = Some("retained Shell cannot transfer".into());
    let wire = serde_json::to_value(&worker).unwrap();
    assert_eq!(wire["executions"], json!([root, child]));
    assert_eq!(wire["error"], "retained Shell cannot transfer");
    assert_eq!(
        serde_json::from_value::<WorkerRuntimeVersion>(wire).unwrap(),
        worker
    );
}

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
        // Never on the wire: the golden shows none.
        host_rules: vec![permission_rule(0x74, Decision::Allow, false)],
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

fn live_chat_snapshot() -> LiveChatSnapshot {
    let mut live =
        LiveChatSnapshot::from_snapshot(execution(), chat_snapshot(), vec!["slow disk".into()]);
    live.pending_model = Some(model_config());
    live.transcript_write_error = Some("disk full".into());
    live
}

fn launch_spec() -> LaunchSpec {
    LaunchSpec {
        folder: "/work/project".into(),
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
            is_terminal: false,
            instance: hosted_instance(),
            title: Some("Quotes".into()),
            state: HostedMemberState::Running,
            resources: Some(HostedResources {
                business_pid: 4242,
                cpu_time_ms: Some(1_500),
                rss_bytes: None,
                sampled_at_ms: 9_000,
            }),
            updated_at_ms: Some(8_000),
            exit: None,
        },
        HostedMember {
            is_terminal: true,
            instance: InstanceRef::hosted(id(0x42)),
            title: Some("cat".into()),
            state: HostedMemberState::Exited,
            resources: None,
            updated_at_ms: None,
            exit: Some(HostedProgramExit {
                code: Some(0),
                signal: None,
            }),
        },
        HostedMember {
            is_terminal: false,
            instance: InstanceRef::hosted(id(0x41)),
            title: None,
            state: HostedMemberState::Lost,
            resources: None,
            updated_at_ms: None,
            exit: None,
        },
    ]
}

fn monitor_membership() -> MonitorMembership {
    MonitorMembership {
        members: vec![MonitorMember {
            is_terminal: false,
            instance: hosted_instance(),
            title: Some("Quotes".into()),
        }],
        removed: [terminal_instance()].into(),
        handled: [(hosted_instance(), 8_500)].into(),
        observed: [(hosted_instance(), 8_900)].into(),
        taken_in: [(hosted_instance(), 8_500)].into(),
        used: [(hosted_instance(), 8_800)].into(),
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
            AgentCommand::Configure { execution: execution(), change: ConfigurationChange::TaskToolsEnabled { enabled: true }, provider: None },
            AgentCommand::Submit {
                execution: execution(),
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
                action: LoopAction::SetScheduledTaskPaused { task_id: id(0x0123_abcd), paused: true },
            },
            AgentCommand::ManageLoop {
                execution: execution(),
                action: LoopAction::UpdateSubscription { subscription_id: id(0x31), change: TickSubscriptionChange::Paused(true) },
            },
            AgentCommand::ManageLoop {
                execution: execution(),
                action: LoopAction::UpdateSubscription {
                    subscription_id: id(0x31),
                    change: TickSubscriptionChange::Frequency(TickSubscriptionFrequency::Low),
                },
            },
            AgentCommand::ManageLoop {
                execution: execution(),
                action: LoopAction::TickUpdate { instance: hosted_instance() },
            },
            AgentCommand::ManageLoop {
                execution: execution(),
                action: LoopAction::TickManage {
                    instance: hosted_instance(),
                },
            },
            AgentCommand::ManageLoop {
                execution: execution(),
                action: LoopAction::SendNow { run_id: id(0x22) },
            },
            AgentCommand::ReopenHostedTick {
                execution: execution(),
                instance: hosted_instance(),
                restore_subscriptions: true,
            },
            AgentCommand::RemoveMonitorMember {
                execution: execution(),
                instance: terminal_instance(),
            },
            AgentCommand::AttachHostedTick {
                execution: execution(),
                instance: InstanceRef::shell(id(0x5e)),
            },
            AgentCommand::LoadRecord {
                record: TranscriptRef {
                    store_root: "/home/user/.tickloop/chats".into(),
                    chat_id: id(0xc1),
                },
                as_new: false,
                launch: None,
            },
            AgentCommand::ClearCompletedTasks {
                execution: execution(),
                root_loop_id: id(0x10),
                expected_revision: 7,
            },
            AgentCommand::FlushTranscripts,
            AgentCommand::ReplaceTickhost,
            AgentCommand::UpgradeRuntime { executable: "/opt/tickloop/versions/0.2.10/tickloop".into() },
            AgentCommand::EditHistory {
                execution: execution(),
                action: HistoryAction::TakeBackInterruptedPrompt { run_id: id(0x20) },
            },
            AgentCommand::DeclineQuestion {
                request: RequestRef {
                    request_id: id(0x62),
                    kind: PendingRequestKind::Question,
                    ..request
                },
            },
            AgentCommand::ReconnectMcpServer {
                execution: execution(),
                name: "plugin_notes_search".into(),
            },
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
                    folder: Some("/work/project".into()),
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
            ObservationRequest::Agent {
                execution: execution(),
                detail: AgentDetailQuery::Status,
                after: None,
            },
            ObservationRequest::Agent {
                execution: execution(),
                detail: AgentDetailQuery::TaskToolsSetting,
                after: None,
            },
            ObservationRequest::Agent {
                execution: execution(),
                detail: AgentDetailQuery::Operation {
                    run_id: id(0x20),
                    call_id: "call-edit".into(),
                },
                after: None,
            },
            ObservationRequest::Agent {
                execution: execution(),
                detail: AgentDetailQuery::Events { after: 9 },
                after: None,
            },
            ObservationRequest::Agent {
                execution: execution(),
                detail: AgentDetailQuery::McpServers,
                after: None,
            },
            ObservationRequest::OtherTickhost,
            ObservationRequest::RuntimeVersion,
        ],
    );
}

#[test]
fn observation_values_keep_their_recorded_wire() {
    let summary = AgentSummary {
        availability: ExecutionAvailability::Live,
        execution: execution(),
        chat_id: id(0xc1),
        folder: Some("/work/project".into()),
        state: tickloop_harness::protocol::LoopState::Running,
        run_id: Some(tickloop_ids::Id::from(id(0x20))),
        title: "Counter check".into(),
        subagent: None,
        has_work: Some(true),
        updated_at: Some(2_000),
        run_started_at: Some(1_000),
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
            ObservationValue::McpServers(vec![
                McpServerStatus {
                    name: "plugin_notes_search".into(),
                    source: "plugin:notes".into(),
                    transport: "http".into(),
                    state: McpServerState::NeedsLogin,
                    exposure: "direct".into(),
                    tools: vec![],
                    failure: None,
                    oauth: true,
                    logged_in: false,
                },
                McpServerStatus {
                    name: "local".into(),
                    source: "global".into(),
                    transport: "stdio".into(),
                    state: McpServerState::Failed,
                    exposure: "direct".into(),
                    tools: vec![McpToolSummary {
                        name: "mcp__local__probe".into(),
                        description: "Report readiness".into(),
                    }],
                    failure: Some("MCP connection closed".into()),
                    oauth: false,
                    logged_in: false,
                },
            ]),
            ObservationValue::Subscriptions(vec![TickSubscriptionStatus {
                paused: false,
                id: id(0x31),
                loop_state: tickloop_harness::protocol::LoopState::Idle,
                instance: hosted_instance(),
                frequency: TickSubscriptionFrequency::High,
                accumulated: 2,
                has_pending_change: false,
                pending_since_ms: None,
                program_exit: None,
                last_delivery_ms: Some(6_000),
                last_delivery: Some("hosted:00000000-0000-0000-0000-000000000040 #4".into()),
                kind: Some(ObservationKind::Protocol),
                title: Some("Quotes".into()),
                updated_at_ms: Some(6_500),
                link: TickLinkState::Live,
                next_check_at_ms: Some(66_000),
                period_ms: Some(60_000),
            }]),
            ObservationValue::Receipt(Some(Box::new(CommandReceipt {
                daemon_epoch: id(0xe0),
                command_id: id(0x81),
                target: Some(execution()),
                state: ReceiptState::Failed,
                result: None,
                error: Some(access_error()),
                persistence: PersistenceState::WriteFailed("disk full".into()),
            }))),
            ObservationValue::Status(Box::new(live_chat_snapshot().status())),
            ObservationValue::TaskToolsSetting(true),
            ObservationValue::TaskToolsSetting(false),
            ObservationValue::Operation(Some(Box::new(OperationRecord {
                parent_call_id: None,
                task_binding: None,
                id: id(0x50),
                loop_id: tickloop_ids::Id::from(id(0x10)),
                run_id: tickloop_ids::Id::from(id(0x20)),
                call_id: "call-edit".into(),
                name: "Edit".into(),
                input: json!({"file_path": "src/counter.rs"}),
                file_change: None,
                state: OperationState::Completed,
                result: Some(ToolResult {
                    content: "Edited src/counter.rs".into(),
                    is_error: false,
                    output_ref: None,
                }),
            }))),
            ObservationValue::Events(vec![Event {
                sequence: 10,
                chat_id: tickloop_ids::Id::from(id(0xc1)),
                loop_id: tickloop_ids::Id::from(id(0x10)),
                run_id: tickloop_ids::Id::from(id(0x20)),
                kind: EventKind::ToolCompleted {
                    sources: Vec::new(),
                    call_id: "call-read".into(),
                    result: ToolResult {
                        content: "A result too long for one feed change".into(),
                        is_error: false,
                        output_ref: None,
                    },
                },
            }]),
            ObservationValue::OtherTickhost(Some(OtherTickhost {
                description: "Running Tickhost build 0.1.0+older; this build 0.2.0+current".into(),
                occupancy: Some(TickhostOccupancy {
                    ticks: vec!["Quote · AAPL.US".into()],
                    shells: 1,
                    pending: 1,
                    views: Some(2),
                }),
            })),
            ObservationValue::OtherTickhost(None),
            ObservationValue::RuntimeVersion(Box::new(tickloop_code_access::RuntimeVersion {
                executable: "/opt/tickloop/versions/0.2.9/tickloop".into(),
                build_id: "0.2.9+0000000000000000".into(),
                requested_executable: Some("/opt/tickloop/versions/0.2.10/tickloop".into()),
                upgrading: true,
                error: None,
                workers: Vec::new(),
            })),
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
                    "kind": {"ToolStarted": {"call_id": "call-1", "name": "TickRead", "input": {"instance": "hosted:00000000-0000-0000-0000-000000000041"}}},
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
            CommandResult::HostedTickReopened {
                instance: hosted_instance(),
                membership: monitor_membership(),
                restore_error: Some(
                    "1 of its Tick Subscriptions could not follow: a Deny rule refuses it".into(),
                ),
            },
            CommandResult::HostedTickView(HostedViewGrant {
                endpoint: "/run/tickhost.sock".into(),
                host_epoch: id(0xe1),
                instance: hosted_instance(),
                view: id(0x53),
            }),
            CommandResult::ArtifactTicks(vec![
                ArtifactTick {
                    artifact_id: "doc".into(),
                    origin: "https://claude.ai".into(),
                    title: "Plan".into(),
                    format: "html".into(),
                    url: "https://claude.ai/artifact/doc".into(),
                    access_role: "writer".into(),
                    added_at_ms: 1_000,
                },
                ArtifactTick {
                    artifact_id: "deck".into(),
                    origin: "https://claude.ai".into(),
                    title: "Launch review".into(),
                    format: "slides".into(),
                    url: "https://claude.ai/artifact/deck".into(),
                    access_role: "editor".into(),
                    added_at_ms: 2_000,
                },
            ]),
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
            CommandResult::PromptTakenBack {
                snapshot: Box::new(live_chat_snapshot()),
                draft: TakenBackPrompt {
                    prompt: "Compare [Image #1]".into(),
                    contexts: serde_json::from_value(json!([
                        {"kind": "image", "origin": {"type": "pasted", "id": 1}, "image": {"media_type": "image/png", "data": "iVBORw0KGgo=", "width": 1, "height": 1}},
                    ]))
                    .unwrap(),
                },
            },
        ],
    );
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
                handoff: None,
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
                build: Some("0.2.0+00c0ffee00c0ffee".into()),
                executable: Some("/opt/tickloop/bin/tickloop".into()),
                upgrade_protocol: None,
                handoff: None,
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

#[test]
fn negotiated_handoff_frames_have_stable_wire_and_redacted_debug() {
    use tickloop_code_access::{
        CLIENT_HANDOFF_VERSION, ClientHandoffHello, ServerHandoffHello, SessionResume,
    };
    let proof = SessionResume {
        daemon_epoch: id(0xe0),
        session_id: id(0x51),
        transfer_id: id(0x71),
        token: "test-only-session-secret".into(),
    };
    let clients = vec![
        ClientMessage::Hello {
            major: PROTOCOL_MAJOR,
            minor: PROTOCOL_MINOR,
            handoff: Some(ClientHandoffHello {
                version: CLIENT_HANDOFF_VERSION,
                resume: None,
            }),
        },
        ClientMessage::Hello {
            major: PROTOCOL_MAJOR,
            minor: PROTOCOL_MINOR,
            handoff: Some(ClientHandoffHello {
                version: CLIENT_HANDOFF_VERSION,
                resume: Some(proof),
            }),
        },
        ClientMessage::HandoffReady {
            transfer_id: id(0x71),
        },
    ];
    assert!(!format!("{clients:?}").contains("test-only-session-secret"));
    assert_golden("handoff_client_message", &clients);
    let ServerMessage::Hello(mut hello) = recorded::<ServerMessage>("server_message").remove(0)
    else {
        panic!("first recorded server message is its Hello");
    };
    hello.handoff = Some(ServerHandoffHello {
        version: CLIENT_HANDOFF_VERSION,
        resume_token: "test-only-session-secret".into(),
    });
    let servers = vec![
        ServerMessage::Hello(hello),
        ServerMessage::HandoffPrepare {
            transfer_id: id(0x71),
        },
        ServerMessage::HandoffAbort {
            transfer_id: id(0x71),
        },
        ServerMessage::NotAccepted {
            request_id: id(0x03),
            transfer_id: id(0x71),
        },
        ServerMessage::HandoffCommit {
            transfer_id: id(0x71),
            successor_process_id: 4243,
        },
    ];
    assert!(!format!("{servers:?}").contains("test-only-session-secret"));
    assert_golden("handoff_server_message", &servers);
    assert_eq!(
        serde_json::to_value(AccessErrorCode::HandoffNotAccepted).unwrap(),
        json!("HandoffNotAccepted")
    );
    assert_eq!(
        serde_json::from_value::<AccessErrorCode>(json!("HandoffNotAccepted")).unwrap(),
        AccessErrorCode::HandoffNotAccepted
    );
}

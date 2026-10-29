//! Every code/daemon integration test, compiled as one binary.

#[path = "all/agent_access.rs"]
mod agent_access;
#[path = "all/agent_history_transport.rs"]
mod agent_history_transport;
#[path = "all/agent_message_daemon_diagnosis.rs"]
mod agent_message_daemon_diagnosis;
#[path = "all/agent_registry.rs"]
mod agent_registry;
#[path = "all/agent_titles.rs"]
mod agent_titles;
#[path = "all/builtin_catalog.rs"]
mod builtin_catalog;
#[path = "all/busy_inputs.rs"]
mod busy_inputs;
#[path = "all/family_records.rs"]
mod family_records;
#[path = "all/hook_views.rs"]
mod hook_views;
#[path = "all/live_context.rs"]
mod live_context;
#[path = "all/permission_claims.rs"]
mod permission_claims;
#[path = "all/prompt_render.rs"]
mod prompt_render;
#[path = "all/streaming_snapshot.rs"]
mod streaming_snapshot;
#[path = "all/tick_tool_approval.rs"]
mod tick_tool_approval;
#[path = "all/tick_tool_routes.rs"]
mod tick_tool_routes;

#[path = "all/mcp_servers.rs"]
mod mcp_servers;

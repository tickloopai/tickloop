//! Every harness/runtime integration test, compiled as one binary.

#[path = "all/advance.rs"]
mod advance;
#[path = "all/agent_approval_routing.rs"]
mod agent_approval_routing;
#[path = "all/citations.rs"]
mod citations;
#[path = "all/cloud_composition.rs"]
mod cloud_composition;
#[path = "all/durable_deliveries.rs"]
mod durable_deliveries;

#[path = "all/durable_queue.rs"]
mod durable_queue;

#[path = "all/durable_waits.rs"]
mod durable_waits;

#[path = "all/model_ids.rs"]
mod model_ids;
#[path = "all/retained_budget.rs"]
mod retained_budget;

#[path = "all/response_lifecycle.rs"]
mod response_lifecycle;

#[path = "all/approval_budget.rs"]
mod approval_budget;

#[path = "all/background_hooks.rs"]
mod background_hooks;

#[path = "all/artifact_real_service.rs"]
mod artifact_real_service;

#[path = "all/concurrent_chats.rs"]
mod concurrent_chats;

#[path = "all/explicit_ask_scopes.rs"]
mod explicit_ask_scopes;

#[path = "all/file_tools.rs"]
mod file_tools;

#[path = "all/plan_auto.rs"]
mod plan_auto;

#[path = "all/subagent_launch.rs"]
mod subagent_launch;

#[path = "all/host_tools.rs"]
mod host_tools;

#[path = "all/interleaved_context.rs"]
mod interleaved_context;

#[path = "all/mcp_approval.rs"]
mod mcp_approval;

#[path = "all/mcp_startup.rs"]
mod mcp_startup;

#[path = "all/mcp_resources.rs"]
mod mcp_resources;

#[path = "all/model_image_capability.rs"]
mod model_image_capability;
#[path = "all/model_live.rs"]
mod model_live;
#[path = "all/model_stream_replay.rs"]
mod model_stream_replay;
#[path = "all/prompt_cache_wire.rs"]
mod prompt_cache_wire;

#[path = "all/output_limit_recovery.rs"]
mod output_limit_recovery;

#[path = "all/tool_batches.rs"]
mod tool_batches;

#[path = "all/transcript_stores.rs"]
mod transcript_stores;

#[path = "all/upgrade.rs"]
mod upgrade;

#[path = "all/waiting_runs.rs"]
mod waiting_runs;

#[path = "all/native_task_auto.rs"]
mod native_task_auto;

#[path = "all/mcp_servers.rs"]
mod mcp_servers;

#[path = "all/third_party_approval.rs"]
mod third_party_approval;

#[path = "all/tool_exposure.rs"]
mod tool_exposure;

#[path = "all/tool_list_stability.rs"]
mod tool_list_stability;

#[path = "all/launch_rules.rs"]
mod launch_rules;

#[path = "all/wrapped_command_rules.rs"]
mod wrapped_command_rules;

#[path = "all/environment_approval.rs"]
mod environment_approval;

#[path = "all/deferred_plan_tool.rs"]
mod deferred_plan_tool;
#[path = "all/plan_reminders.rs"]
mod plan_reminders;

#[path = "all/sandbox_auto_approval.rs"]
mod sandbox_auto_approval;

#[path = "all/tool_hooks.rs"]
mod tool_hooks;

#[path = "all/session_hooks.rs"]
mod session_hooks;

//! Every agent/runtime integration test, compiled as one binary.

#[path = "all/advance.rs"]
mod advance;
#[path = "all/agent_approval_routing.rs"]
mod agent_approval_routing;
#[path = "all/durable_deliveries.rs"]
mod durable_deliveries;
#[path = "all/durable_family.rs"]
mod durable_family;
#[path = "all/durable_queue.rs"]
mod durable_queue;
#[path = "all/durable_tasks.rs"]
mod durable_tasks;
#[path = "all/durable_triggers.rs"]
mod durable_triggers;
#[path = "all/durable_waits.rs"]
mod durable_waits;
#[path = "all/retained_budget.rs"]
mod retained_budget;

#[path = "all/response_lifecycle.rs"]
mod response_lifecycle;

#[path = "all/approval_budget.rs"]
mod approval_budget;
#[path = "all/approval_stages.rs"]
mod approval_stages;

#[path = "all/subagent_handback.rs"]
mod subagent_handback;

#[path = "all/auto_review_improvements.rs"]
mod auto_review_improvements;

#[path = "all/approval_assessment.rs"]
mod approval_assessment;
#[path = "all/artifact_real_service.rs"]
mod artifact_real_service;
#[path = "all/auto_configuration.rs"]
mod auto_configuration;
#[path = "all/auto_fallback.rs"]
mod auto_fallback;
#[path = "all/auto_reviewer_responses.rs"]
mod auto_reviewer_responses;
#[path = "all/auto_rule_behavior.rs"]
mod auto_rule_behavior;
#[path = "all/auto_rules_management.rs"]
mod auto_rules_management;
#[path = "all/automation_live.rs"]
mod automation_live;
#[path = "all/chat_branching.rs"]
mod chat_branching;
#[path = "all/chat_switching.rs"]
mod chat_switching;
#[path = "all/command_archive.rs"]
mod command_archive;
#[path = "all/compaction_boundaries.rs"]
mod compaction_boundaries;
#[path = "all/concurrent_chats.rs"]
mod concurrent_chats;
#[path = "all/context.rs"]
mod context;
#[path = "all/conversation.rs"]
mod conversation;
#[path = "all/conversation_local_messages.rs"]
mod conversation_local_messages;
#[path = "all/copilot_models.rs"]
mod copilot_models;
#[path = "all/denial_history.rs"]
mod denial_history;
#[path = "all/diff_review.rs"]
mod diff_review;
#[path = "all/directed_summary.rs"]
mod directed_summary;
#[path = "all/execution_modes.rs"]
mod execution_modes;
#[path = "all/explicit_ask_scopes.rs"]
mod explicit_ask_scopes;
#[path = "all/failed_replies.rs"]
mod failed_replies;
#[path = "all/file_tools.rs"]
mod file_tools;
#[path = "all/folderless_chats.rs"]
mod folderless_chats;
#[path = "all/glm_models.rs"]
mod glm_models;
#[path = "all/goal_routing_live.rs"]
mod goal_routing_live;
#[path = "all/host_tools.rs"]
mod host_tools;
#[path = "all/image_limits.rs"]
mod image_limits;
#[path = "all/input_context.rs"]
mod input_context;
#[path = "all/interaction.rs"]
mod interaction;
#[path = "all/interleaved_context.rs"]
mod interleaved_context;
#[path = "all/interrupted_prompt.rs"]
mod interrupted_prompt;
#[path = "all/jev_auto.rs"]
mod jev_auto;
#[path = "all/loop_chain.rs"]
mod loop_chain;
#[path = "all/loop_trigger_safety.rs"]
mod loop_trigger_safety;
#[path = "all/mcp_approval.rs"]
mod mcp_approval;
#[path = "all/mcp_tools.rs"]
mod mcp_tools;
#[path = "all/model_error_retry.rs"]
mod model_error_retry;
#[path = "all/model_image_capability.rs"]
mod model_image_capability;
#[path = "all/model_live.rs"]
mod model_live;
#[path = "all/model_stream_replay.rs"]
mod model_stream_replay;
#[path = "all/module_boundaries_live.rs"]
mod module_boundaries_live;
#[path = "all/monitor_updates.rs"]
mod monitor_updates;
#[path = "all/nested_subagents.rs"]
mod nested_subagents;
#[path = "all/observers.rs"]
mod observers;
#[path = "all/output_limit_recovery.rs"]
mod output_limit_recovery;
#[path = "all/permissions.rs"]
mod permissions;
#[path = "all/permissions_management.rs"]
mod permissions_management;
#[path = "all/plan_approval.rs"]
mod plan_approval;
#[path = "all/plans.rs"]
mod plans;
#[path = "all/project_rule_freshness.rs"]
mod project_rule_freshness;
#[path = "all/provider_error_security.rs"]
mod provider_error_security;
#[path = "all/provider_errors.rs"]
mod provider_errors;
#[path = "all/queued_inputs.rs"]
mod queued_inputs;
#[path = "all/read_file_images.rs"]
mod read_file_images;
#[path = "all/recovery.rs"]
mod recovery;
#[path = "all/request_usage.rs"]
mod request_usage;
#[path = "all/rewind_file_safety.rs"]
mod rewind_file_safety;
#[path = "all/rewind_points.rs"]
mod rewind_points;
#[path = "all/rewind_restore.rs"]
mod rewind_restore;
#[path = "all/session_permission_branching.rs"]
mod session_permission_branching;
#[path = "all/shell_descriptions.rs"]
mod shell_descriptions;
#[path = "all/shell_read_only.rs"]
mod shell_read_only;
#[path = "all/shell_tools.rs"]
mod shell_tools;
#[path = "all/skills.rs"]
mod skills;
#[path = "all/support/mod.rs"]
mod support;
#[path = "all/switching_http.rs"]
mod switching_http;
#[path = "all/task_notification_runs.rs"]
mod task_notification_runs;
#[path = "all/temp_storage.rs"]
mod temp_storage;
#[path = "all/tick_adapters.rs"]
mod tick_adapters;
#[path = "all/tick_package_logins.rs"]
mod tick_package_logins;
#[path = "all/tick_package_opens.rs"]
mod tick_package_opens;
#[path = "all/tick_subscriptions.rs"]
mod tick_subscriptions;
#[path = "all/tickhost_shells.rs"]
mod tickhost_shells;
#[path = "all/tickhost_versions.rs"]
mod tickhost_versions;
#[path = "all/tool_results.rs"]
mod tool_results;
#[path = "all/transcript_stores.rs"]
mod transcript_stores;
#[path = "all/transcripts.rs"]
mod transcripts;
#[path = "all/triggers.rs"]
mod triggers;
#[path = "all/trusted_approval_context.rs"]
mod trusted_approval_context;
#[path = "all/unpersisted_state.rs"]
mod unpersisted_state;
#[path = "all/user_settings.rs"]
mod user_settings;
#[path = "all/waiting_runs.rs"]
mod waiting_runs;
#[path = "all/web_tools.rs"]
mod web_tools;
#[path = "all/workspace_host_context.rs"]
mod workspace_host_context;

#[path = "all/shared_tasks.rs"]
mod shared_tasks;

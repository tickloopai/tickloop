//! Every code/console integration test, compiled as one binary.

#[path = "all/agent_console.rs"]
mod agent_console;
#[path = "all/agent_family_navigation.rs"]
mod agent_family_navigation;
#[path = "all/agent_messages.rs"]
mod agent_messages;
#[path = "all/artifact_auto_attachment.rs"]
mod artifact_auto_attachment;
#[path = "all/delegation.rs"]
mod delegation;
#[path = "all/monitor_membership.rs"]
mod monitor_membership;
#[path = "all/tick_adapters.rs"]
mod tick_adapters;

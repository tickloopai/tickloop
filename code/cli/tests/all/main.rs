//! The code/cli integration tests: they start the tickloop executable or the cli_fixture,
//! compiled as one binary. Tests of a single crate live in that crate's `tests/all.rs`.

#[path = "../support/mod.rs"]
mod support;

mod agent_console_pty;
mod agent_daemon;
mod agent_daemon_control;
mod agent_startup_errors;
mod artifact_terminal;
mod artifact_terminal_feedback;
mod builtin_catalog;
mod codex_terminal;
mod command_consolidation;
mod graceful_upgrade;
mod hooks_panel_pty;
mod hosted_bounds;
mod hosted_ticks;
mod marketplace_commands;
mod marketplaces_panel_pty;
mod monitor_pty;
mod plugin_commands;
mod provider_picker;
mod runtime_source;
mod terminal;
mod tick_package_commands;
mod ticks_panel_pty;
mod update_command;
mod upgrade_races;
mod workspace_trust_pty;

mod mcp_commands;
mod mcp_login;
mod mcp_panel_pty;

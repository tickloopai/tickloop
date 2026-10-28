//! Every code/settings integration test, compiled as one binary.

#[path = "all/auto_compact_settings.rs"]
mod auto_compact_settings;
#[path = "all/claude_auto_import.rs"]
mod claude_auto_import;

#[path = "all/builtin_catalog.rs"]
mod builtin_catalog;
#[path = "all/builtin_skills.rs"]
mod builtin_skills;
#[path = "all/goal_settings.rs"]
mod goal_settings;
#[path = "all/instruction_files.rs"]
mod instruction_files;
#[path = "all/loop_skill.rs"]
mod loop_skill;
#[path = "all/model_configuration.rs"]
mod model_configuration;
#[path = "all/monitor_settings.rs"]
mod monitor_settings;
#[path = "all/plan_settings.rs"]
mod plan_settings;
#[path = "all/plugin_skills.rs"]
mod plugin_skills;
#[path = "all/settings_store.rs"]
mod settings_store;
#[path = "all/skill_catalog.rs"]
mod skill_catalog;
#[path = "all/skill_frontmatter.rs"]
mod skill_frontmatter;
#[path = "all/tick_package_skills.rs"]
mod tick_package_skills;

#[path = "all/mcp_servers.rs"]
mod mcp_servers;

#[path = "all/mcp_credentials.rs"]
mod mcp_credentials;

#[path = "all/additional_directories.rs"]
mod additional_directories;

#[path = "all/sandbox_settings.rs"]
mod sandbox_settings;

//! Every agent/config integration test, compiled as one binary.

#[path = "all/builtin_catalog.rs"]
mod builtin_catalog;
#[path = "all/builtin_skills.rs"]
mod builtin_skills;
#[path = "all/goal_settings.rs"]
mod goal_settings;
#[path = "all/model_configuration.rs"]
mod model_configuration;
#[path = "all/monitor_settings.rs"]
mod monitor_settings;
#[path = "all/skill_catalog.rs"]
mod skill_catalog;
#[path = "all/tick_package_skills.rs"]
mod tick_package_skills;

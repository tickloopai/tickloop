//! Every agent/config integration test, compiled as one binary.

#[path = "all/builtin_catalog.rs"]
mod builtin_catalog;
#[path = "all/builtin_skills.rs"]
mod builtin_skills;
#[path = "all/model_configuration.rs"]
mod model_configuration;
#[path = "all/monitor_settings.rs"]
mod monitor_settings;

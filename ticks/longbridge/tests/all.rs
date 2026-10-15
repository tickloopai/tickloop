//! Every longbridge integration test, compiled as one binary.

#[path = "all/auth.rs"]
mod auth;
#[path = "all/auth_live.rs"]
mod auth_live;
#[path = "all/auth_ui.rs"]
mod auth_ui;
#[path = "all/earnings_probe.rs"]
mod earnings_probe;
#[path = "all/entry.rs"]
mod entry;
#[path = "all/native_visible_stdio.rs"]
mod native_visible_stdio;
#[path = "all/package.rs"]
mod package;
#[path = "all/rate_limit.rs"]
mod rate_limit;
#[path = "all/support/mod.rs"]
mod support;
#[path = "all/us_positions.rs"]
mod us_positions;

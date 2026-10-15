//! Every futu integration test, compiled as one binary.

#[path = "all/entry.rs"]
mod entry;
#[path = "all/native_visible_stdio.rs"]
mod native_visible_stdio;
#[path = "all/opend.rs"]
mod opend;
#[path = "all/package.rs"]
mod package;
#[path = "all/support/mod.rs"]
mod support;

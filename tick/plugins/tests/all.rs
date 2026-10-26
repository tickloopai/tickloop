//! Every tick/plugins integration test, compiled as one binary.

#[path = "all/manifest.rs"]
mod manifest;
#[path = "all/marketplace.rs"]
mod marketplace;
#[path = "all/migration.rs"]
mod migration;
#[path = "all/store.rs"]
mod store;
#[path = "all/support.rs"]
mod support;

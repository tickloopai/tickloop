//! Every tick/packages integration test, compiled as one binary.

#[path = "all/archive.rs"]
mod archive;
#[path = "all/download.rs"]
mod download;
#[path = "all/install.rs"]
mod install;
#[path = "all/manifest.rs"]
mod manifest;
#[path = "all/marketplace.rs"]
mod marketplace;
#[path = "all/support.rs"]
mod support;

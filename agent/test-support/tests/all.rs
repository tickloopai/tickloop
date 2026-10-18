//! Every agent/test-support integration test, compiled as one binary.

#[path = "all/durable_family.rs"]
mod durable_family;
#[path = "all/durable_waits.rs"]
mod durable_waits;
#[path = "all/stateless_host.rs"]
mod stateless_host;

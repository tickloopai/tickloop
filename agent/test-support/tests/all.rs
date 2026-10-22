//! Every agent/test-support integration test, compiled as one binary.

#[path = "all/durable_deliveries.rs"]
mod durable_deliveries;
#[path = "all/durable_family.rs"]
mod durable_family;
#[path = "all/durable_holdings.rs"]
mod durable_holdings;
#[path = "all/durable_waits.rs"]
mod durable_waits;
#[path = "all/stateless_host.rs"]
mod stateless_host;

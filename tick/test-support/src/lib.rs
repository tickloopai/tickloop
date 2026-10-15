//! Test-only mock data for the financial Ticks: simulated quotes, account, news, options,
//! community posts and an earnings release, so tests run a Tick offline without an account. Only
//! tests and the `tick-mock` executable depend on this crate; no shipped program links it.

mod api;
mod community;
mod options;
mod reader;

pub use api::MockApi;
pub use community::MockCommunity;
pub use options::MockOptions;
pub use reader::mock_library;

/// The source the mock data names.
pub const SOURCE: &str = "mock";

//! The shared Harness API. Hosts select capabilities with Cargo features; these module paths
//! remain stable when the implementation crates move. This crate contains only re-exports.

pub use tickloop_agent_base as base;
pub use tickloop_agent_protocol as protocol;
pub use tickloop_ids as ids;

#[cfg(feature = "artifacts")]
pub use tickloop_agent_artifacts as artifacts;
#[cfg(feature = "cron")]
pub use tickloop_agent_cron as cron;
#[cfg(feature = "model")]
pub use tickloop_agent_model as model;
#[cfg(feature = "engine")]
pub use tickloop_agent_runtime as runtime;
#[cfg(feature = "test-support")]
pub use tickloop_agent_test_support as test_support;
#[cfg(feature = "engine")]
pub use tickloop_agent_tools as tools;
#[cfg(feature = "transcript")]
pub use tickloop_agent_transcript as transcript;

#[cfg(feature = "policy")]
pub use tickloop_agent_policy as policy;

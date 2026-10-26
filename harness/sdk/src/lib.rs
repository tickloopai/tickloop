//! The shared Harness API. Hosts select capabilities with Cargo features; these module paths
//! remain stable when the implementation crates move. This crate contains only re-exports.
//! `protocol` and `ids` are always available. `base` is opt-in, also enabled by `tools`,
//! `model`, `artifacts`, `policy`, `engine` and `test-support`; `cron` and `transcript` stay lightweight.

#[cfg(feature = "base")]
pub use tickloop_harness_base as base;
pub use tickloop_harness_protocol as protocol;
pub use tickloop_ids as ids;

#[cfg(feature = "artifacts")]
pub use tickloop_harness_artifacts as artifacts;
#[cfg(feature = "cron")]
pub use tickloop_harness_cron as cron;
#[cfg(feature = "model")]
pub use tickloop_harness_model as model;
#[cfg(feature = "engine")]
pub use tickloop_harness_runtime as runtime;
#[cfg(feature = "test-support")]
pub use tickloop_harness_test_support as test_support;
#[cfg(feature = "tools")]
pub use tickloop_harness_tools as tools;
#[cfg(feature = "transcript")]
pub use tickloop_harness_transcript as transcript;

#[cfg(feature = "policy")]
pub use tickloop_harness_policy as policy;

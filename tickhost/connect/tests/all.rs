//! Every tickhost/connect integration test, compiled as one binary.

#[path = "all/host_adapter.rs"]
mod host_adapter;
#[path = "all/native_identity.rs"]
mod native_identity;
#[path = "all/placement.rs"]
mod placement;
#[path = "all/session.rs"]
mod session;
#[path = "all/session_encoded_limits.rs"]
mod session_encoded_limits;
#[path = "all/session_observation.rs"]
mod session_observation;

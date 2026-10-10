//! Every tickhost/connect integration test, compiled as one binary.

#[path = "all/herdr_socket.rs"]
mod herdr_socket;
#[path = "all/placement.rs"]
mod placement;
#[path = "all/session.rs"]
mod session;
#[path = "all/session_encoded_limits.rs"]
mod session_encoded_limits;
#[path = "all/session_observation.rs"]
mod session_observation;

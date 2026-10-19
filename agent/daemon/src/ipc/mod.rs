//! The AgentAccess server over a private Unix socket, exposing the daemon or one worker's Agent
//! to the clients of this user.

mod server;
#[cfg(test)]
mod tests;

pub(crate) use server::serve_daemon_listener_with_lifecycle;
pub use server::{serve_connection, serve_daemon_listener, serve_listener, serve_worker_listener};

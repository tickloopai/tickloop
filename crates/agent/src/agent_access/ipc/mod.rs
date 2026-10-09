//! The AgentAccess transport over a private Unix socket: length-prefixed frames shared by both
//! ends, the client a Console or daemon uses to reach an Agent, and the server that exposes one.

mod client;
mod frame;
mod server;
#[cfg(test)]
mod tests;

pub use client::LocalAgentClient;
pub use server::{serve_connection, serve_daemon_listener, serve_listener, serve_worker_listener};

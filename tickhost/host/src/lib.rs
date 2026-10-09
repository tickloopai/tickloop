//! The Tickhost: a local service that runs Agent-opened protocol Ticks in private PTYs,
//! independent of the Agent service's lifetime and of any layout host.

pub mod cli;
pub mod host;
pub mod paths;
pub mod process;
pub mod pty;
pub mod screen;

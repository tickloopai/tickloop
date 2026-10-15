//! What every Tick Command shares: the display flags and configuration, the locale check,
//! `describe`, and running a Tick as a snapshot, a standalone terminal session or the protocol
//! Tick its host launched. Each program parses only its own Ticks and runs them through here.

pub mod cli;
pub mod entry;
mod locale;

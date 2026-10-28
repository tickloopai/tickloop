//! The Tickhost: a local service that runs Agent-opened extension Ticks in private PTYs and the
//! Shell tasks Workers submit in pipe mode, independent of any layout host.

pub mod cli;
pub mod host;
pub mod in_process;
pub mod paths;
pub mod process;
pub mod pty;
pub mod screen;
pub mod screen_signals;
pub mod shell;
mod shell_screen;
pub mod views;

pub mod sandbox;

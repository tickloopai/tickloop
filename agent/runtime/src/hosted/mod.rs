//! Agent-opened Ticks in the dedicated Tickhost: the daemon's single host controller, the
//! authenticated channel through which workers ask the daemon to open, close and list them, and
//! tell it when a Chat ends, and the Herdr Panes that show them.

pub mod controller;
pub mod lifecycle;
pub mod registry;
pub mod terminal_input;
pub mod tools;
pub mod views;
pub mod worker_control;

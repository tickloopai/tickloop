//! Agent-opened Ticks and Shell tasks in the dedicated Tickhost: the daemon's single host
//! controller, the authenticated channel through which workers ask the daemon to open, close and
//! list Ticks, grant Shell spawns, and tell it when a Chat ends, and the Herdr Panes that show
//! the Ticks.

pub mod controller;
pub mod lifecycle;
pub mod registry;
pub mod shells;
pub mod terminal_input;
pub mod tools;
pub mod views;
pub mod worker_control;

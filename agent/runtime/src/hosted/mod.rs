//! Agent-opened Ticks in the dedicated Tickhost: the daemon's single host controller and the
//! authenticated channel through which workers ask the daemon to open, close and list them, and
//! tell it when a Chat ends.

pub mod controller;
pub mod lifecycle;
pub mod registry;
pub mod tools;
pub mod worker_control;

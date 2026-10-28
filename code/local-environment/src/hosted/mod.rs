//! Agent-opened Ticks and Shell tasks in the dedicated Tickhost: the daemon's single host
//! controller, the authenticated channel through which workers ask the daemon to open, close and
//! list Ticks and grant Shell spawns.

pub mod controller;
pub mod registry;
pub mod shells;
pub mod tools;
pub mod worker_control;

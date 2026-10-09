//! Agent-opened Ticks in the dedicated Tick host: the owner's single host controller and the
//! authenticated channel through which workers ask the owner to open, close and list them, and
//! tell it when a Chat ends.

pub mod attach;
pub mod controller;
pub mod lifecycle;
pub mod registry;
pub mod tools;
pub mod worker_control;

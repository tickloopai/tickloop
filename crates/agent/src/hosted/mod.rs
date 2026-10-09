//! Agent-opened Ticks in the dedicated Tick host: the owner's single host controller and the
//! authenticated channel through which workers ask the owner to open, close and list them.

pub mod controller;
pub mod tools;
pub mod worker_control;

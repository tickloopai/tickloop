//! The financial Ticks: each one reads its data only through the provider interfaces, publishes
//! its content through the Tick SDK and draws with the shared terminal UI.

pub mod args;
pub mod commands;
pub mod config;
pub mod earnings;
pub mod event_chart;
pub mod input;
pub mod metadata;
pub mod options;
pub mod output;
pub mod reader;
pub mod research;
pub mod runtime;
pub mod terminal;
pub mod tools;
pub mod viewport;

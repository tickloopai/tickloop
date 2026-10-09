//! What a Tick needs to speak the Tick standard: one [`stream::TickStream`] per Tick Instance
//! answers `tick/describe`, `tick/read` and `tool/call` and publishes Tick Updates, and
//! [`tools::Tools`] declares the Tick Tools it offers.

pub mod stream;
pub mod tools;

//! The builtin provider: simulated quotes, account, news and options for every Tick, so a Tick
//! runs offline without an account.

mod api;
mod options;

use tick_provider::Interface;

pub use api::DemoApi;
pub use options::DemoOptions;

/// The `source` of the Tick Updates the builtin provider's data produces.
pub const SOURCE: &str = "demo";

pub const INTERFACES: &[Interface] = &[
    Interface::Quote,
    Interface::Trade,
    Interface::News,
    Interface::Options,
];

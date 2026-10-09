//! The Longbridge provider: quotes, the trading account and news through the Longbridge SDK, with
//! device-code OAuth and the SDK's rate limits.

pub mod auth;
pub mod news_api;
pub mod probe;
pub mod rate_limit;
pub mod sdk;
pub mod us_positions;

use tick_provider::Interface;

/// The `source` of the Tick Updates Longbridge data produces.
pub const SOURCE: &str = "longbridge";

pub const INTERFACES: &[Interface] = &[Interface::Quote, Interface::Trade, Interface::News];

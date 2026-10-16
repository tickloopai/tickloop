//! The Longbridge provider: quotes, the trading account, news and community posts through the
//! Longbridge SDK, with the login Tickloop keeps for the package and the SDK's rate limits; and
//! `tick-longbridge`, which runs the Ticks that read them and can log in on its own.

pub mod auth;
pub mod cli;
pub mod community_api;
pub mod entry;
pub mod login;
pub mod news_api;
pub mod probe;
pub mod rate_limit;
pub mod region;
pub mod sdk;
pub mod us_positions;

use tick_provider::error::Error;

/// The Tick package this program comes from, whose login it uses.
pub const PACKAGE: &str = "longbridge";

/// What an authorization failure asks the user to do: log in through Tickloop, which keeps the
/// package's login.
pub const AUTH_HINT: &str =
    "log in with /ticks in the Agent, or `tickloop tick login longbridge` in a terminal";

/// An authorization failure that tells the user how to log in again.
pub fn not_authorized(message: impl std::fmt::Display) -> Error {
    Error::auth(format!("{message}; {AUTH_HINT}"))
}

/// Whether `error` is an authorization failure that tells the user how to log in again.
#[cfg(test)]
fn asks_to_log_in(error: &Error) -> bool {
    error.code == tick_provider::exit_code::NOT_AUTHORIZED && error.message.ends_with(AUTH_HINT)
}

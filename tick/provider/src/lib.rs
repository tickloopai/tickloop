//! The provider abstraction: the interfaces a Tick reads market, account, news, option and
//! community data through, and the data they exchange. Each provider implements the interfaces
//! it can serve.

pub mod api;
pub mod community;
pub mod dates;
pub mod error;
pub mod exit_code;
pub mod fiscal;
pub mod news;
pub mod options;
pub mod preferences;

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

/// One interface a provider can implement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Interface {
    Quote,
    Trade,
    News,
    Options,
    Community,
}

impl Interface {
    /// The trait that implements this interface.
    pub fn trait_name(self) -> &'static str {
        match self {
            Self::Quote => "QuoteApi",
            Self::Trade => "TradeApi",
            Self::News => "NewsApi",
            Self::Options => "OptionsApi",
            Self::Community => "CommunityApi",
        }
    }
}

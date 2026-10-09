//! The Tick Tools each financial Tick declares.

use tick_provider::{api::QuoteApi, error::Error};

use crate::metadata::TickKind;

/// A financial Tick's declared tools: its query tools close over the quote services.
pub type Tools = tick_sdk::tools::Tools<dyn QuoteApi, Error>;

pub fn for_kind(kind: TickKind) -> Tools {
    match kind {
        TickKind::Quote => crate::commands::quote::tools(),
        TickKind::Watchlist => crate::commands::watchlist::tools(),
        TickKind::Kline => crate::commands::kline::tools(),
        TickKind::Earnings => crate::commands::earnings::tools(),
        TickKind::Options => crate::commands::options::tools(),
        TickKind::Reader => crate::commands::reader::tools(),
        TickKind::Positions | TickKind::News => Tools::default(),
    }
}

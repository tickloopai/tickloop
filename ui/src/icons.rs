/// U3's braille spinner frames.
pub const BRAILLE_SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

#[derive(Clone, Copy, Debug)]
pub enum Icon {
    Connection,
    TickUpdate,
    Paper,
    Rise,
    Fall,
    MarketPre,
    MarketOpen,
    MarketClosed,
    Position,
    Kline,
}

impl Icon {
    pub fn glyph(self) -> &'static str {
        match self {
            Self::Connection => "●",
            // The connection point flashes one U3 spinner frame on a Tick Update.
            Self::TickUpdate => BRAILLE_SPINNER_FRAMES[0],
            Self::Paper => "⊕",
            Self::Rise => "▲",
            Self::Fall => "▼",
            Self::MarketPre => "◌",
            Self::MarketOpen => "▶",
            Self::MarketClosed => "■",
            Self::Position => "▦",
            Self::Kline => "▤",
        }
    }
}

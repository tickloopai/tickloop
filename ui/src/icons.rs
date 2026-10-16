/// The brand mark in a terminal cell: the braille cell's eight outer dots, lit one at a time
/// clockwise from 12 o'clock. Working steps a frame every [`TICK_SPINNER_INTERVAL_MS`], a Tick Update
/// steps one frame, and waiting rests on the first.
pub const TICK_SPINNER_FRAMES: [&str; 8] = ["⠁", "⠈", "⠐", "⠠", "⢀", "⡀", "⠄", "⠂"];
pub const TICK_SPINNER_INTERVAL_MS: u64 = 110;
/// The same ring lit cumulatively, clockwise from 12 o'clock: one frame per eighth of a whole.
pub const TICK_PROGRESS_FRAMES: [&str; 8] = ["⠁", "⠉", "⠙", "⠹", "⢹", "⣹", "⣽", "⣿"];

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
            Self::TickUpdate => TICK_SPINNER_FRAMES[0],
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

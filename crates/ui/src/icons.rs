use crate::settings::IconTheme;

/// U3's braille spinner frames. Braille renders in both glyph sets, so the two themes agree.
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
    pub fn glyph(self, theme: IconTheme) -> &'static str {
        let (unicode, nerd) = match self {
            Self::Connection => ("●", "\u{f0318} "),
            // The connection point flashes one U3 spinner frame on a Tick Update.
            Self::TickUpdate => (BRAILLE_SPINNER_FRAMES[0], BRAILLE_SPINNER_FRAMES[0]),
            Self::Paper => ("⊕", "\u{f023} "),
            Self::Rise => ("▲", "\u{f0535} "),
            Self::Fall => ("▼", "\u{f0533} "),
            Self::MarketPre => ("◌", "\u{f0150} "),
            Self::MarketOpen => ("▶", "\u{f04b} "),
            Self::MarketClosed => ("■", "\u{f04d} "),
            Self::Position => ("▦", "\u{f0584} "),
            Self::Kline => ("▤", "\u{f05e2} "),
        };
        match theme {
            IconTheme::Unicode => unicode,
            IconTheme::Nerd => nerd,
        }
    }
}

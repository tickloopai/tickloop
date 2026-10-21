/// The brand mark in a terminal cell: the braille cell's eight outer dots drawn as a ring, with
/// one gap stepping clockwise from 12 o'clock, since a lone lit dot reads as punctuation at text
/// size. Working steps a frame every [`TICK_SPINNER_INTERVAL_MS`], a Tick Update steps one frame,
/// and waiting rests on the first.
pub const TICK_SPINNER_FRAMES: [&str; 8] = ["⣾", "⣷", "⣯", "⣟", "⡿", "⢿", "⣻", "⣽"];
pub const TICK_SPINNER_INTERVAL_MS: u64 = 110;

#[derive(Clone, Copy, Debug)]
pub enum Icon {
    Connection,
    TickUpdate,
}

impl Icon {
    pub fn glyph(self) -> &'static str {
        match self {
            Self::Connection => "●",
            Self::TickUpdate => TICK_SPINNER_FRAMES[0],
        }
    }
}

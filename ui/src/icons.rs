/// The working dot: the brand's orange Agent dot alternating with its ringed form, the
/// terminal's take on the dot's spreading outer ring. Waiting rests on the first frame.
pub const WORKING_DOT_FRAMES: [&str; 2] = ["●", "◉"];

#[derive(Clone, Copy, Debug)]
pub enum Icon {
    Connection,
    TickUpdate,
}

impl Icon {
    pub fn glyph(self) -> &'static str {
        match self {
            Self::Connection => "●",
            Self::TickUpdate => "◉",
        }
    }
}

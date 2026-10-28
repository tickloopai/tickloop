//! Picks the key-bubbling sequences a hosted Tick writes out of its terminal output. The
//! terminal model's parser splits OSC sequences, whichever reads they arrive in, and drops the
//! ones it does not handle, so they never reach the screen; its callback keeps the Tick's
//! signals. The parser buffers an OSC until its terminator without a limit, so every OSC longer
//! than [`MAX_OSC_BYTES`] is cut off before it reaches the parser.

use tick_protocol::KeySignal;

/// The longest OSC body kept. No OSC changes the screen, and a key-bubbling sequence is a few
/// dozen bytes.
pub(crate) const MAX_OSC_BYTES: usize = 1024;
/// The most signals one read keeps; a Tick writing more in one read loses the rest.
pub(crate) const MAX_SIGNALS_PER_READ: usize = 64;

const ESCAPE: u8 = 0x1b;
const BELL: u8 = 0x07;
/// CAN and SUB abort a sequence.
const CANCEL: u8 = 0x18;
const SUBSTITUTE: u8 = 0x1a;
const OSC_INTRODUCER: u8 = b']';

/// Collects the key-bubbling signals among the OSC sequences the parser does not handle.
#[derive(Default)]
pub(crate) struct SignalCollector {
    signals: Vec<KeySignal>,
}

impl SignalCollector {
    pub(crate) fn take(&mut self) -> Vec<KeySignal> {
        std::mem::take(&mut self.signals)
    }
}

impl vt100::Callbacks for SignalCollector {
    fn unhandled_osc(&mut self, _: &mut vt100::Screen, params: &[&[u8]]) {
        if self.signals.len() < MAX_SIGNALS_PER_READ
            && let Some(signal) = KeySignal::parse(params)
        {
            self.signals.push(signal);
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum OscState {
    #[default]
    Outside,
    AfterEscape,
    /// Inside an OSC body that has this many bytes so far.
    Inside(usize),
    /// Inside an OSC body that outgrew the limit: its bytes are dropped up to its end.
    Overlong,
}

/// Follows the output's OSC sequences across reads the way the parser does, and cuts off each
/// one that outgrows [`MAX_OSC_BYTES`]: a CAN ends it in the parser, and its remaining bytes up
/// to its own end are dropped.
#[derive(Default)]
pub(crate) struct OscLimit {
    state: OscState,
}

impl OscLimit {
    /// The bytes to give the parser for `bytes`, borrowed when nothing had to be cut.
    pub(crate) fn filter<'a>(&mut self, bytes: &'a [u8]) -> std::borrow::Cow<'a, [u8]> {
        let mut kept: Option<Vec<u8>> = None;
        for (index, byte) in bytes.iter().copied().enumerate() {
            let ends_body = matches!(byte, BELL | CANCEL | SUBSTITUTE | ESCAPE);
            let (next, keep) = match self.state {
                OscState::Overlong if ends_body => (Self::after_end(byte), byte == ESCAPE),
                OscState::Overlong => (OscState::Overlong, false),
                OscState::Inside(_) if ends_body => (Self::after_end(byte), true),
                OscState::Inside(length) if length >= MAX_OSC_BYTES => {
                    let kept = kept.get_or_insert_with(|| bytes[..index].to_vec());
                    kept.push(CANCEL);
                    (OscState::Overlong, false)
                }
                OscState::Inside(length) => (OscState::Inside(length + 1), true),
                OscState::AfterEscape if byte == OSC_INTRODUCER => (OscState::Inside(0), true),
                _ if byte == ESCAPE => (OscState::AfterEscape, true),
                _ => (OscState::Outside, true),
            };
            self.state = next;
            match (&mut kept, keep) {
                (Some(kept), true) => kept.push(byte),
                (None, false) => kept = Some(bytes[..index].to_vec()),
                _ => {}
            }
        }
        kept.map_or(std::borrow::Cow::Borrowed(bytes), std::borrow::Cow::Owned)
    }

    fn after_end(byte: u8) -> OscState {
        if byte == ESCAPE {
            OscState::AfterEscape
        } else {
            OscState::Outside
        }
    }
}

#[cfg(test)]
mod tests {
    use tick_protocol::BubbledKey;

    use super::*;

    fn parse(reads: &[&[u8]]) -> (Vec<KeySignal>, String) {
        let mut limit = OscLimit::default();
        let mut parser = vt100::Parser::new_with_callbacks(4, 40, 0, SignalCollector::default());
        let mut signals = Vec::new();
        for read in reads {
            parser.process(&limit.filter(read));
            signals.extend(parser.callbacks_mut().take());
        }
        (signals, parser.screen().contents())
    }

    #[test]
    fn signals_end_with_bel_or_st_and_never_reach_the_screen() {
        let (signals, screen) =
            parse(&[b"a\x1b]7777;tickloop;bubble=on\x07b\x1b]7777;tickloop;key=down\x1b\\c"]);
        assert_eq!(
            signals,
            [KeySignal::Bubbling(true), KeySignal::Key(BubbledKey::Down)]
        );
        assert_eq!(screen, "abc");
    }

    #[test]
    fn a_signal_split_across_reads_is_read_once_whole() {
        let sequence = b"x\x1b]7777;tickloop;key=backtab\x1b\\y";
        for split in 1..sequence.len() {
            let (signals, screen) = parse(&[&sequence[..split], &sequence[split..]]);
            assert_eq!(signals, [KeySignal::Key(BubbledKey::Backtab)], "{split}");
            assert_eq!(screen, "xy", "{split}");
        }
        let bytewise: Vec<&[u8]> = sequence.chunks(1).collect();
        assert_eq!(parse(&bytewise).0, [KeySignal::Key(BubbledKey::Backtab)]);
    }

    #[test]
    fn unknown_names_other_oscs_and_garbage_are_ignored() {
        let (signals, screen) = parse(&[
            b"\x1b]7777;tickloop;key=enter\x07",
            b"\x1b]0;title\x07\x1b]7777;other;key=esc\x07",
            b"\x1b]7777;tickloop;bubble=maybe\x07\x1b]7777\x07\x1b]\x07",
            b"\x1b[31mred\x1b[m \xff\x1b]7777;tickloop;key=up\x07",
        ]);
        assert_eq!(signals, [KeySignal::Key(BubbledKey::Up)]);
        assert_eq!(screen.trim_end(), "red");
    }

    #[test]
    fn an_overlong_osc_is_cut_off_and_the_output_after_it_kept() {
        let mut limit = OscLimit::default();
        let mut long = b"before\x1b]7777;tickloop;key=".to_vec();
        long.extend(std::iter::repeat_n(b'x', 10 * MAX_OSC_BYTES));
        let kept = limit.filter(&long).into_owned();
        assert!(kept.len() <= b"before\x1b]".len() + MAX_OSC_BYTES + 1);
        assert_eq!(kept.last(), Some(&CANCEL));
        assert!(
            limit.filter(&[b'y'; 4096]).is_empty(),
            "later reads of it are dropped too"
        );
        let after = limit.filter(b"\x07after\x1b]7777;tickloop;key=esc\x07");
        assert_eq!(&*after, b"after\x1b]7777;tickloop;key=esc\x07");

        let (signals, screen) = parse(&[&long, b"\x07after\x1b]7777;tickloop;key=esc\x07"]);
        assert_eq!(signals, [KeySignal::Key(BubbledKey::Esc)]);
        assert_eq!(screen, "beforeafter");
    }

    #[test]
    fn a_read_keeps_a_bounded_number_of_signals() {
        let flood = KeySignal::Key(BubbledKey::Tab)
            .sequence()
            .repeat(MAX_SIGNALS_PER_READ * 2);
        assert_eq!(parse(&[flood.as_bytes()]).0.len(), MAX_SIGNALS_PER_READ);
    }
}

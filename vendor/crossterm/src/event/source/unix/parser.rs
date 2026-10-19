use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use crate::event::{sys::unix::parse::parse_event, Event, InternalEvent, KeyCode};

const ESCAPE_TIMEOUT: Duration = Duration::from_millis(50);
const ESCAPE: u8 = 0x1b;

#[derive(Debug, Default)]
pub(super) struct Parser {
    buffer: Vec<u8>,
    internal_events: VecDeque<InternalEvent>,
    escape_started_at: Option<Instant>,
    escape_deferred: bool,
    input_closed: bool,
}

impl Parser {
    pub(super) fn input_buffer_is_empty(&self) -> bool {
        self.buffer.is_empty() && self.internal_events.is_empty()
    }

    pub(super) fn has_events(&self) -> bool {
        !self.internal_events.is_empty()
    }

    pub(super) fn has_deferred_escape(&self) -> bool {
        self.escape_deferred && self.escape_started_at.is_some()
    }

    pub(super) fn is_input_closed(&self) -> bool {
        self.input_closed
    }

    pub(super) fn finish_input(&mut self) {
        if self.escape_started_at.take().is_some() {
            self.internal_events
                .push_back(InternalEvent::Event(Event::Key(KeyCode::Esc.into())));
        }
        self.buffer.clear();
        self.escape_deferred = false;
        self.input_closed = true;
    }

    pub(super) fn advance(&mut self, bytes: &[u8], now: Instant) {
        // Consumer backpressure makes the arrival time of queued continuations unknown.
        if !self.has_deferred_escape() {
            self.expire_escape(now);
        }
        for byte in bytes {
            if self.buffer == [ESCAPE] && *byte == ESCAPE {
                self.internal_events
                    .push_back(InternalEvent::Event(Event::Key(KeyCode::Esc.into())));
                self.buffer.clear();
            }
            self.buffer.push(*byte);
            self.escape_started_at = (self.buffer == [ESCAPE]).then_some(now);
            self.escape_deferred =
                self.escape_started_at.is_some() && !self.internal_events.is_empty();
            // A read boundary cannot distinguish a lone Esc from a fragmented prefix.
            match parse_event(&self.buffer, true) {
                Ok(Some(event)) => {
                    self.internal_events.push_back(event);
                    self.buffer.clear();
                }
                Ok(None) => {}
                Err(_) => self.buffer.clear(),
            }
        }
    }

    pub(super) fn expire_escape(&mut self, now: Instant) {
        if self.has_deferred_escape() && !self.internal_events.is_empty() {
            return;
        }
        if self
            .escape_started_at
            .is_some_and(|started| now.saturating_duration_since(started) >= ESCAPE_TIMEOUT)
        {
            self.internal_events
                .push_back(InternalEvent::Event(Event::Key(KeyCode::Esc.into())));
            self.buffer.clear();
            self.escape_started_at = None;
            self.escape_deferred = false;
        }
    }

    pub(super) fn poll_timeout(&self, timeout: Option<Duration>, now: Instant) -> Option<Duration> {
        match self.escape_started_at {
            Some(started) => {
                let remaining =
                    ESCAPE_TIMEOUT.saturating_sub(now.saturating_duration_since(started));
                Some(timeout.map_or(remaining, |timeout| timeout.min(remaining)))
            }
            None => timeout,
        }
    }
}

impl Iterator for Parser {
    type Item = InternalEvent;

    fn next(&mut self) -> Option<Self::Item> {
        self.internal_events.pop_front()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> InternalEvent {
        InternalEvent::Event(Event::Key(code.into()))
    }

    #[test]
    fn adjacent_escape_preserves_both_page_up_sequences() {
        let mut parser = Parser::default();
        parser.advance(b"\x1b\x1b[5~\x1b[5~", Instant::now());
        assert_eq!(
            parser.collect::<Vec<_>>(),
            vec![
                key(KeyCode::Esc),
                key(KeyCode::PageUp),
                key(KeyCode::PageUp)
            ]
        );
    }

    #[test]
    fn adjacent_escape_is_independent_of_read_fragmentation() {
        let input = b"\x1b\x1b[5~\x1b[5~";
        let now = Instant::now();
        for mask in 0..(1 << (input.len() - 1)) {
            let mut parser = Parser::default();
            let mut start = 0;
            for end in 1..=input.len() {
                if end == input.len() || mask & (1 << (end - 1)) != 0 {
                    parser.advance(&input[start..end], now);
                    start = end;
                }
            }
            assert_eq!(
                parser.collect::<Vec<_>>(),
                vec![
                    key(KeyCode::Esc),
                    key(KeyCode::PageUp),
                    key(KeyCode::PageUp)
                ],
                "fragment mask {mask}"
            );
        }
    }

    #[test]
    fn each_escape_is_delivered_once_and_the_final_one_has_a_deadline() {
        let now = Instant::now();
        let mut parser = Parser::default();
        parser.advance(b"\x1b\x1b\x1b", now);
        assert_eq!(
            parser.by_ref().collect::<Vec<_>>(),
            vec![key(KeyCode::Esc), key(KeyCode::Esc)]
        );
        parser.expire_escape(now + ESCAPE_TIMEOUT - Duration::from_millis(1));
        assert_eq!(parser.next(), None);
        parser.expire_escape(now + ESCAPE_TIMEOUT);
        assert_eq!(parser.next(), Some(key(KeyCode::Esc)));
        parser.expire_escape(now + ESCAPE_TIMEOUT * 2);
        assert_eq!(parser.next(), None);
    }

    #[test]
    fn escape_deadline_caps_but_does_not_extend_the_callers_poll() {
        let now = Instant::now();
        let mut parser = Parser::default();
        assert_eq!(parser.poll_timeout(None, now), None);
        parser.advance(b"\x1b", now);
        assert_eq!(parser.poll_timeout(None, now), Some(ESCAPE_TIMEOUT));
        assert_eq!(
            parser.poll_timeout(Some(Duration::ZERO), now),
            Some(Duration::ZERO)
        );
        assert_eq!(
            parser.poll_timeout(Some(Duration::from_secs(1)), now),
            Some(ESCAPE_TIMEOUT)
        );
        assert_eq!(
            parser.poll_timeout(Some(Duration::from_millis(5)), now),
            Some(Duration::from_millis(5))
        );
        let deadline = now + ESCAPE_TIMEOUT;
        assert_eq!(parser.poll_timeout(None, deadline), Some(Duration::ZERO));
        parser.expire_escape(deadline);
        assert_eq!(parser.next(), Some(key(KeyCode::Esc)));
        assert_eq!(parser.poll_timeout(None, deadline), None);
    }

    #[test]
    fn alt_characters_use_the_window_and_expired_escape_remains_separate() {
        let now = Instant::now();
        let mut parser = Parser::default();
        parser.advance(b"\x1b", now);
        parser.advance(b"x", now + ESCAPE_TIMEOUT - Duration::from_millis(1));
        assert_eq!(
            parser.next(),
            Some(InternalEvent::Event(Event::Key(KeyEvent::new(
                KeyCode::Char('x'),
                KeyModifiers::ALT
            ))))
        );
        parser.advance(b"\x1b", now);
        parser.advance(b"x", now + ESCAPE_TIMEOUT);
        assert_eq!(
            parser.collect::<Vec<_>>(),
            vec![key(KeyCode::Esc), key(KeyCode::Char('x'))]
        );
    }

    #[test]
    fn escape_prefix_preserves_ss3_csi_modifiers_and_keyboard_enhancements() {
        let now = Instant::now();
        let cases = [
            (b"\x1bOH".as_slice(), key(KeyCode::Home)),
            (b"\x1bOF".as_slice(), key(KeyCode::End)),
            (b"\x1bOP".as_slice(), key(KeyCode::F(1))),
            (b"\x1b[6~".as_slice(), key(KeyCode::PageDown)),
            (
                b"\x1b[1;3A".as_slice(),
                InternalEvent::Event(Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT))),
            ),
            (
                b"\x1b[5;3~".as_slice(),
                InternalEvent::Event(Event::Key(KeyEvent::new(
                    KeyCode::PageUp,
                    KeyModifiers::ALT,
                ))),
            ),
            (b"\x1b[27u".as_slice(), key(KeyCode::Esc)),
            (
                b"\x1b[120;3u".as_slice(),
                InternalEvent::Event(Event::Key(KeyEvent::new(
                    KeyCode::Char('x'),
                    KeyModifiers::ALT,
                ))),
            ),
            (
                b"\x1b[I".as_slice(),
                InternalEvent::Event(Event::FocusGained),
            ),
            (b"\x1b[O".as_slice(), InternalEvent::Event(Event::FocusLost)),
        ];
        for (sequence, expected) in cases {
            let mut parser = Parser::default();
            parser.advance(b"\x1b", now);
            for byte in sequence {
                parser.advance(&[*byte], now + Duration::from_millis(1));
            }
            assert_eq!(
                parser.collect::<Vec<_>>(),
                vec![key(KeyCode::Esc), expected],
                "sequence {sequence:?}"
            );
        }
    }

    #[test]
    fn fragmented_alt_unicode_and_plain_utf8_keep_exact_key_identity() {
        let now = Instant::now();
        let mut parser = Parser::default();
        parser.advance(b"\x1b\x1b", now);
        for byte in "文".as_bytes() {
            parser.advance(&[*byte], now + Duration::from_millis(1));
        }
        for byte in "é".as_bytes() {
            parser.advance(&[*byte], now + Duration::from_millis(1));
        }
        assert_eq!(
            parser.collect::<Vec<_>>(),
            vec![
                key(KeyCode::Esc),
                InternalEvent::Event(Event::Key(KeyEvent::new(
                    KeyCode::Char('文'),
                    KeyModifiers::ALT
                ))),
                key(KeyCode::Char('é')),
            ]
        );
    }

    #[cfg(feature = "bracketed-paste")]
    #[test]
    fn paste_contents_are_not_interpreted_as_adjacent_key_sequences() {
        let now = Instant::now();
        let mut parser = Parser::default();
        let pasted = "literal [5~ 中文\x1b\x1b[5~";
        let bytes = format!("\x1b\x1b[200~{pasted}\x1b[201~");
        for byte in bytes.as_bytes() {
            parser.advance(&[*byte], now);
        }
        assert_eq!(
            parser.collect::<Vec<_>>(),
            vec![
                key(KeyCode::Esc),
                InternalEvent::Event(Event::Paste(pasted.into()))
            ]
        );
    }
}

//! The kitty keyboard protocol as a terminal such as kitty or Ghostty keeps it for the output it
//! shows: a stack of enhancement flags for each screen, kept across screen switches, and the keys
//! the terminal sends under the flags in force.

/// The flag that makes the terminal report Esc, Ctrl-keys and Alt-keys as `CSI … u`.
pub const DISAMBIGUATE_ESCAPE_CODES: u8 = 1;

const ALTERNATE_SCREEN_MODES: [&str; 3] = ["47", "1047", "1049"];

#[derive(Debug, Default)]
pub struct KittyKeyboard {
    main: Vec<u8>,
    alternate: Vec<u8>,
    on_alternate: bool,
}

impl KittyKeyboard {
    /// The state of a terminal that showed `output` from its start. Pushes, pops and screen
    /// switches count; setting flags in place is not modelled.
    pub fn after(output: &[u8]) -> Self {
        let mut keyboard = Self::default();
        let mut rest = output;
        while let Some(start) = rest.windows(2).position(|pair| pair == b"\x1b[") {
            let sequence = &rest[start + 2..];
            let Some(end) = sequence
                .iter()
                .position(|byte| (0x40..=0x7e).contains(byte))
            else {
                break;
            };
            keyboard.apply(&sequence[..end], sequence[end]);
            rest = &sequence[end + 1..];
        }
        keyboard
    }

    fn apply(&mut self, parameters: &[u8], final_byte: u8) {
        let Ok(parameters) = std::str::from_utf8(parameters) else {
            return;
        };
        let number = |text: &str, default: usize| text.parse().unwrap_or(default);
        match (final_byte, parameters.chars().next()) {
            (b'h' | b'l', Some('?')) => {
                if parameters[1..]
                    .split(';')
                    .any(|mode| ALTERNATE_SCREEN_MODES.contains(&mode))
                {
                    self.on_alternate = final_byte == b'h';
                }
            }
            (b'u', Some('>')) => {
                let flags = number(&parameters[1..], 0) as u8;
                self.stack_mut().push(flags);
            }
            (b'u', Some('<')) => {
                let count = number(&parameters[1..], 1);
                let stack = self.stack_mut();
                stack.truncate(stack.len().saturating_sub(count));
            }
            _ => {}
        }
    }

    fn stack_mut(&mut self) -> &mut Vec<u8> {
        if self.on_alternate {
            &mut self.alternate
        } else {
            &mut self.main
        }
    }

    /// The flags pushed on the main screen and not popped, oldest first.
    pub fn main(&self) -> &[u8] {
        &self.main
    }

    /// The flags pushed on the alternate screen and not popped, oldest first.
    pub fn alternate(&self) -> &[u8] {
        &self.alternate
    }

    /// The flags in force on the screen shown now.
    pub fn flags(&self) -> u8 {
        let stack = if self.on_alternate {
            &self.alternate
        } else {
            &self.main
        };
        stack.last().copied().unwrap_or(0)
    }

    fn disambiguates(&self) -> bool {
        self.flags() & DISAMBIGUATE_ESCAPE_CODES != 0
    }

    /// What the terminal sends now for a press of Esc.
    pub fn escape(&self) -> &'static [u8] {
        if self.disambiguates() {
            b"\x1b[27u"
        } else {
            b"\x1b"
        }
    }

    /// What the terminal sends now for a press of Ctrl+B.
    pub fn control_b(&self) -> &'static [u8] {
        if self.disambiguates() {
            b"\x1b[98;5u"
        } else {
            b"\x02"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_screen_keeps_its_own_flags_across_screen_switches() {
        let keyboard = KittyKeyboard::after(b"\x1b[?1049h\x1b[>1u\x1b[?1049l\x1b[<1u");
        assert!(keyboard.main().is_empty(), "the pop found nothing there");
        assert_eq!(keyboard.alternate(), [DISAMBIGUATE_ESCAPE_CODES]);
        assert_eq!(keyboard.escape(), b"\x1b");

        let back = KittyKeyboard::after(b"\x1b[?1049h\x1b[>1u\x1b[?1049l\x1b[?1049h");
        assert_eq!(
            back.escape(),
            b"\x1b[27u",
            "the alternate screen still has them"
        );
        assert_eq!(back.control_b(), b"\x1b[98;5u");

        let popped = KittyKeyboard::after(b"\x1b[?1049h\x1b[>1u\x1b[>0u\x1b[<u\x1b[<5u");
        assert!(popped.alternate().is_empty() && popped.flags() == 0);
    }
}

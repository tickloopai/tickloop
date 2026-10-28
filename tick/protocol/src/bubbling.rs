//! Key bubbling: a Tick shown in a card tells the client, through private OSC sequences in its
//! own terminal output, which of the navigation keys it leaves unused, so the client can act on
//! them instead.

/// The private OSC number and namespace that start every key-bubbling sequence:
/// `ESC ] 7777 ; tickloop ; <field>`, terminated by BEL or ST.
const OSC_NUMBER: &[u8] = b"7777";
const OSC_NAMESPACE: &[u8] = b"tickloop";

/// A key a Tick can hand back to the client that shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BubbledKey {
    Esc,
    Up,
    Down,
    Left,
    Right,
    Tab,
    Backtab,
}

impl BubbledKey {
    pub const ALL: [Self; 7] = [
        Self::Esc,
        Self::Up,
        Self::Down,
        Self::Left,
        Self::Right,
        Self::Tab,
        Self::Backtab,
    ];

    /// The name a `key=` sequence carries.
    pub fn name(self) -> &'static str {
        match self {
            Self::Esc => "esc",
            Self::Up => "up",
            Self::Down => "down",
            Self::Left => "left",
            Self::Right => "right",
            Self::Tab => "tab",
            Self::Backtab => "backtab",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|key| key.name() == name)
    }
}

/// One key-bubbling sequence a Tick writes to its terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeySignal {
    /// `bubble=on` or `bubble=off`: whether the Tick bubbles the keys it does not use. It holds
    /// for the rest of the instance until the Tick says otherwise.
    Bubbling(bool),
    /// `key=<name>`: the Tick did not use this key.
    Key(BubbledKey),
}

impl KeySignal {
    /// The sequence a Tick writes to its terminal, terminated by BEL.
    pub fn sequence(self) -> String {
        let field = match self {
            Self::Bubbling(true) => "bubble=on".to_owned(),
            Self::Bubbling(false) => "bubble=off".to_owned(),
            Self::Key(key) => format!("key={}", key.name()),
        };
        format!("\x1b]7777;tickloop;{field}\x07")
    }

    /// Reads an OSC sequence from its parameters, the bytes between `ESC ]` and its terminator
    /// split at each `;`. Any other OSC, an unknown field or an unknown key name is `None`.
    pub fn parse(params: &[&[u8]]) -> Option<Self> {
        let [OSC_NUMBER, OSC_NAMESPACE, field] = params else {
            return None;
        };
        match *field {
            b"bubble=on" => Some(Self::Bubbling(true)),
            b"bubble=off" => Some(Self::Bubbling(false)),
            _ => {
                let name = field.strip_prefix(b"key=")?;
                BubbledKey::from_name(std::str::from_utf8(name).ok()?).map(Self::Key)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(sequence: &str) -> Vec<&[u8]> {
        let body = sequence
            .strip_prefix("\x1b]")
            .and_then(|body| body.strip_suffix('\x07'))
            .unwrap();
        body.as_bytes().split(|byte| *byte == b';').collect()
    }

    #[test]
    fn every_signal_reads_back_from_the_sequence_it_writes() {
        assert_eq!(
            KeySignal::Bubbling(true).sequence(),
            "\x1b]7777;tickloop;bubble=on\x07"
        );
        assert_eq!(
            KeySignal::Key(BubbledKey::Backtab).sequence(),
            "\x1b]7777;tickloop;key=backtab\x07"
        );
        let signals = [KeySignal::Bubbling(true), KeySignal::Bubbling(false)]
            .into_iter()
            .chain(BubbledKey::ALL.map(KeySignal::Key));
        for signal in signals {
            assert_eq!(KeySignal::parse(&params(&signal.sequence())), Some(signal));
        }
    }

    #[test]
    fn other_oscs_fields_and_names_are_not_signals() {
        for ignored in [
            &[&b"7777"[..], b"tickloop", b"key=enter"][..],
            &[b"7777", b"tickloop", b"key=ESC"],
            &[b"7777", b"tickloop", b"key="],
            &[b"7777", b"tickloop", b"bubble=yes"],
            &[b"7777", b"tickloop", b"key=esc", b"extra"],
            &[b"7777", b"other", b"key=esc"],
            &[b"7778", b"tickloop", b"key=esc"],
            &[b"7777", b"tickloop", b"key=\xff"],
            &[b"0", b"title"],
            &[],
        ] {
            assert_eq!(KeySignal::parse(ignored), None, "{ignored:?}");
        }
    }
}

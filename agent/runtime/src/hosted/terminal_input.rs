//! The bytes ModifyTick `send_input` writes to a hosted terminal Tick: its text as it is, then its
//! logical keys as a terminal sends them.

use tickhost_connect::hosted::ATTACH_INPUT_BYTES;

/// The logical key names `send_input` accepts, ignoring case; `Ctrl-` takes one letter or one of
/// `@[\]^_`.
pub const KEY_NAMES: &str = "Enter, Tab, Esc, Backspace, Delete, Up, Down, Left, Right, Home, End, PageUp, PageDown, Space, Ctrl-<letter>";

const NAMED_KEYS: [(&str, &[u8]); 16] = [
    ("enter", b"\r"),
    ("tab", b"\t"),
    ("esc", b"\x1b"),
    ("escape", b"\x1b"),
    ("backspace", b"\x7f"),
    ("delete", b"\x1b[3~"),
    ("up", b"\x1b[A"),
    ("down", b"\x1b[B"),
    ("right", b"\x1b[C"),
    ("left", b"\x1b[D"),
    ("home", b"\x1b[H"),
    ("end", b"\x1b[F"),
    ("pageup", b"\x1b[5~"),
    ("pagedown", b"\x1b[6~"),
    ("space", b" "),
    ("return", b"\r"),
];
const CONTROL_PREFIX: &str = "ctrl-";
/// The bits a control key keeps of its character.
const CONTROL_MASK: u8 = 0x1f;

/// The bytes `text` and then `keys` type, refused when a key is unknown, nothing is typed or the
/// input exceeds what one write to the Tick takes.
pub fn encode(text: Option<&str>, keys: &[String]) -> Result<Vec<u8>, String> {
    let mut bytes = text.unwrap_or_default().as_bytes().to_vec();
    for key in keys {
        bytes.extend(key_bytes(key)?);
    }
    if bytes.is_empty() {
        return Err("send_input requires text or keys".into());
    }
    if bytes.len() > ATTACH_INPUT_BYTES {
        return Err(format!(
            "send_input types at most {ATTACH_INPUT_BYTES} bytes at once"
        ));
    }
    Ok(bytes)
}

fn key_bytes(key: &str) -> Result<Vec<u8>, String> {
    let name = key.to_ascii_lowercase();
    if let Some((_, bytes)) = NAMED_KEYS.iter().find(|(known, _)| *known == name) {
        return Ok(bytes.to_vec());
    }
    if let Some(character) = name.strip_prefix(CONTROL_PREFIX)
        && let [character] = character.as_bytes()
        && (character.is_ascii_lowercase() || b"@[\\]^_".contains(character))
    {
        return Ok(vec![character.to_ascii_uppercase() & CONTROL_MASK]);
    }
    Err(format!("unknown key {key:?}; use {KEY_NAMES}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_comes_first_and_keys_type_what_a_terminal_sends() {
        let keys = ["Enter", "ctrl-c", "Ctrl-D", "UP", "Esc", "Ctrl-["].map(String::from);
        assert_eq!(
            encode(Some("ls -l"), &keys).unwrap(),
            b"ls -l\r\x03\x04\x1b[A\x1b\x1b".to_vec()
        );
        assert_eq!(encode(None, &["Space".into()]).unwrap(), b" ".to_vec());
    }

    #[test]
    fn unknown_keys_empty_and_oversized_input_are_refused() {
        for key in ["F13", "Ctrl-", "Ctrl-ab", "Ctrl-1", "Alt-x"] {
            let refused = encode(None, &[key.into()]).unwrap_err();
            assert!(refused.contains(KEY_NAMES), "{refused}");
        }
        assert!(encode(Some(""), &[]).is_err());
        assert!(encode(Some(&"x".repeat(ATTACH_INPUT_BYTES)), &[]).is_ok());
        assert!(encode(Some(&"x".repeat(ATTACH_INPUT_BYTES)), &["Enter".into()]).is_err());
    }
}

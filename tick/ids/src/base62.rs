//! The 22-character base62 text form of a UUID, byte for byte the public id of Tickloop Cloud:
//! the UUID read as a big-endian u128, written in an ASCII-ordered alphabet and left-padded with
//! `0`, so string order matches numeric (and for v7, time) order.

use uuid::Uuid;

pub const LENGTH: usize = 22;
const HYPHENATED_UUID_LENGTH: usize = 36;
const ALPHABET: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
const BASE: u128 = ALPHABET.len() as u128;

pub fn encode(id: Uuid) -> String {
    let mut value = id.as_u128();
    let mut digits = [ALPHABET[0]; LENGTH];
    for digit in digits.iter_mut().rev() {
        *digit = ALPHABET[(value % BASE) as usize];
        value /= BASE;
    }
    digits.iter().map(|&byte| byte as char).collect()
}

/// Exactly 22 base62 digits, case-sensitive.
pub fn decode(text: &str) -> Option<Uuid> {
    if text.len() != LENGTH {
        return None;
    }
    let mut value: u128 = 0;
    for byte in text.bytes() {
        let digit = digit_value(byte)?;
        value = value.checked_mul(BASE)?.checked_add(digit)?;
    }
    Some(Uuid::from_u128(value))
}

/// Either text form, told apart by length: 22 characters are base62, 36 a hyphenated UUID in any
/// case, anything else is rejected. The version is never checked.
pub fn decode_either(text: &str) -> Option<Uuid> {
    match text.len() {
        LENGTH => decode(text),
        HYPHENATED_UUID_LENGTH => Uuid::try_parse(text).ok(),
        _ => None,
    }
}

fn digit_value(byte: u8) -> Option<u128> {
    let value = match byte {
        b'0'..=b'9' => byte - b'0',
        b'A'..=b'Z' => byte - b'A' + 10,
        b'a'..=b'z' => byte - b'a' + 36,
        _ => return None,
    };
    Some(value.into())
}

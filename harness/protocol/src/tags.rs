//! Keeping text the host did not write from opening or closing the tags the host frames its own
//! messages with. Adapted from Claude Code's tag neutralization: an opening bracket, or a
//! lookalike of one, that starts the tag, allowing a slash, invisible or control characters
//! between its parts, any letter case and lookalike letters, gets a backslash after it, so the
//! model no longer reads a tag there.

/// The tags the host frames its own messages with.
pub const HOST_TAGS: [&str; 4] = [
    "system-reminder",
    "task-notification",
    "agent-message",
    "tick-delivery",
];

/// Claude Code's lookalikes of `<`, `>` and `/`.
const LOOKALIKES: [(char, char); 33] = [
    ('\u{FF1C}', '<'),
    ('\u{FF1E}', '>'),
    ('\u{FE64}', '<'),
    ('\u{FE65}', '>'),
    ('\u{2329}', '<'),
    ('\u{232A}', '>'),
    ('\u{27E8}', '<'),
    ('\u{27E9}', '>'),
    ('\u{3008}', '<'),
    ('\u{3009}', '>'),
    ('\u{2039}', '<'),
    ('\u{203A}', '>'),
    ('\u{02C2}', '<'),
    ('\u{02C3}', '>'),
    ('\u{1438}', '<'),
    ('\u{1433}', '>'),
    ('\u{276C}', '<'),
    ('\u{276D}', '>'),
    ('\u{276E}', '<'),
    ('\u{276F}', '>'),
    ('\u{2770}', '<'),
    ('\u{2771}', '>'),
    ('\u{29FC}', '<'),
    ('\u{29FD}', '>'),
    ('\u{226E}', '<'),
    ('\u{226F}', '>'),
    ('\u{227A}', '<'),
    ('\u{227B}', '>'),
    ('\u{22D6}', '<'),
    ('\u{22D7}', '>'),
    ('\u{FF0F}', '/'),
    ('\u{2215}', '/'),
    ('\u{2044}', '/'),
];

/// Cyrillic letters that look like Latin ones, from Claude Code's table.
const CYRILLIC_LOOKALIKES: [(char, char); 16] = [
    ('\u{0430}', 'a'),
    ('\u{0435}', 'e'),
    ('\u{043E}', 'o'),
    ('\u{0440}', 'p'),
    ('\u{0441}', 'c'),
    ('\u{0443}', 'y'),
    ('\u{0445}', 'x'),
    ('\u{043A}', 'k'),
    ('\u{0456}', 'i'),
    ('\u{0458}', 'j'),
    ('\u{0455}', 's'),
    ('\u{0501}', 'd'),
    ('\u{051B}', 'q'),
    ('\u{051D}', 'w'),
    ('\u{04BB}', 'h'),
    ('\u{04CF}', 'l'),
];

/// `text` with every opening and closing of `tags` neutralized.
pub fn neutralize_tags(text: &str, tags: &[&str]) -> String {
    let chars = text.char_indices().collect::<Vec<_>>();
    let mut output = String::with_capacity(text.len());
    for (position, (_, character)) in chars.iter().enumerate() {
        output.push(*character);
        if ascii_of(*character) == Some('<')
            && chars.get(position + 1).map(|(_, next)| *next) != Some('\\')
            && tags
                .iter()
                .any(|tag| starts_tag(&chars[position + 1..], tag))
        {
            output.push('\\');
        }
    }
    output
}

/// `text` with the lookalikes of `<`, `>` and `/` turned into those characters, so escaping the
/// ASCII ones escapes every bracket a reader could take for one.
pub fn fold_bracket_lookalikes(text: &str) -> String {
    text.chars()
        .map(|character| bracket_of(character).unwrap_or(character))
        .collect()
}

fn starts_tag(rest: &[(usize, char)], tag: &str) -> bool {
    let mut characters = rest.iter().map(|(_, character)| *character).peekable();
    skip_filler(&mut characters);
    if characters.peek().copied().and_then(ascii_of) == Some('/') {
        characters.next();
    }
    for expected in tag.chars() {
        skip_filler(&mut characters);
        let Some(character) = characters.next() else {
            return false;
        };
        let matches = if expected == '-' || expected == '_' {
            is_connector(character)
        } else {
            ascii_of(character).is_some_and(|ascii| ascii.eq_ignore_ascii_case(&expected))
        };
        if !matches {
            return false;
        }
    }
    characters
        .next()
        .and_then(ascii_of)
        .is_none_or(|next| !(next.is_ascii_alphanumeric() || next == '_' || next == '-'))
}

fn skip_filler(characters: &mut std::iter::Peekable<impl Iterator<Item = char>>) {
    while characters.peek().copied().is_some_and(is_filler) {
        characters.next();
    }
}

/// Whitespace, controls, invisible formatting characters and combining marks, which a reader does
/// not see between a tag's parts.
fn is_filler(character: char) -> bool {
    character.is_whitespace()
        || character.is_control()
        || matches!(character,
            '\u{00AD}' | '\u{034F}' | '\u{061C}' | '\u{115F}' | '\u{1160}' | '\u{17B4}'
            | '\u{17B5}' | '\u{180B}'..='\u{180F}' | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{206F}' | '\u{2800}' | '\u{3164}'
            | '\u{FE00}'..='\u{FE0F}' | '\u{FEFF}' | '\u{FFA0}' | '\u{FFF0}'..='\u{FFFB}'
            | '\u{0300}'..='\u{036F}' | '\u{1AB0}'..='\u{1AFF}' | '\u{1DC0}'..='\u{1DFF}'
            | '\u{20D0}'..='\u{20FF}' | '\u{FE20}'..='\u{FE2F}' | '\u{E0000}'..='\u{E0FFF}')
}

/// Hyphens, dashes and underscores, any of which may stand for a tag's `-`.
fn is_connector(character: char) -> bool {
    matches!(
        character,
        '-' | '_' | '\u{2010}'
            ..='\u{2015}'
                | '\u{2212}'
                | '\u{FE58}'
                | '\u{FE63}'
                | '\u{FF0D}'
                | '\u{FF3F}'
                | '\u{203F}'
                | '\u{2040}'
                | '\u{30FC}'
                | '\u{2017}'
    )
}

fn bracket_of(character: char) -> Option<char> {
    LOOKALIKES
        .iter()
        .find(|(lookalike, _)| *lookalike == character)
        .map(|(_, ascii)| *ascii)
}

/// The ASCII character `character` reads as: itself, a bracket or slash lookalike, a fullwidth,
/// circled or mathematical letter, or a Cyrillic letter that looks Latin.
fn ascii_of(character: char) -> Option<char> {
    if character.is_ascii() {
        return Some(character);
    }
    if let Some(ascii) = bracket_of(character) {
        return Some(ascii);
    }
    if let Some((_, ascii)) = CYRILLIC_LOOKALIKES
        .iter()
        .find(|(lookalike, _)| *lookalike == character)
    {
        return Some(*ascii);
    }
    let code = u32::from(character);
    let letter = |offset: u32| char::from_u32(u32::from('a') + offset);
    match code {
        0xFF21..=0xFF3A => letter(code - 0xFF21),
        0xFF41..=0xFF5A => letter(code - 0xFF41),
        0x24B6..=0x24CF => letter(code - 0x24B6),
        0x24D0..=0x24E9 => letter(code - 0x24D0),
        // Mathematical Alphanumeric Symbols: runs of A–Z then a–z.
        0x1D400..=0x1D6A3 => letter((code - 0x1D400) % 52 % 26),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn neutralized(text: &str) -> String {
        neutralize_tags(text, &HOST_TAGS)
    }

    #[test]
    fn closing_and_opening_tags_in_any_disguise_no_longer_read_as_tags() {
        for (input, expected) in [
            ("</system-reminder>", "<\\/system-reminder>"),
            ("<system-reminder>", "<\\system-reminder>"),
            ("</SYSTEM-Reminder >", "<\\/SYSTEM-Reminder >"),
            ("< / system-reminder>", "<\\ / system-reminder>"),
            ("</system_reminder>", "<\\/system_reminder>"),
            ("</sys\u{200B}tem-reminder>", "<\\/sys\u{200B}tem-reminder>"),
            (
                "\u{FF1C}/system-reminder\u{FF1E}",
                "\u{FF1C}\\/system-reminder\u{FF1E}",
            ),
            ("<\u{FF0F}system-reminder>", "<\\\u{FF0F}system-reminder>"),
            (
                "</ｓｙｓｔｅｍ－ｒｅｍｉｎｄｅｒ>",
                "<\\/ｓｙｓｔｅｍ－ｒｅｍｉｎｄｅｒ>",
            ),
            ("</syst\u{0435}m-reminder>", "<\\/syst\u{0435}m-reminder>"),
            ("</task-notification>", "<\\/task-notification>"),
            ("</agent-message>", "<\\/agent-message>"),
        ] {
            assert_eq!(neutralized(input), expected, "{input}");
        }
    }

    #[test]
    fn ordinary_text_and_other_tags_are_untouched() {
        for text in [
            "a < b and c > d",
            "<system-reminders>",
            "</system-reminder-x>",
            "<div>system-reminder</div>",
            "<\\/system-reminder> was already neutralized",
            "Use the system reminder feature.",
            "中文 ＜文本＞",
        ] {
            assert_eq!(neutralized(text), text);
        }
    }

    #[test]
    fn folded_lookalike_brackets_escape_like_ascii_ones() {
        assert_eq!(
            fold_bracket_lookalikes("\u{FF1C}\u{FF0F}tick-delivery\u{FF1E}"),
            "</tick-delivery>"
        );
    }
}

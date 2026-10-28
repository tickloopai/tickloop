use super::{bash_pattern_matches, shell_token_ends, split_shell_parts};

/// Selects execution behavior; a match never grants permission to run the command.
pub fn shell_command_matches_pattern(pattern: &str, command: &str) -> bool {
    for part in split_shell_parts(command, true).unwrap_or_else(|| vec![command.trim()]) {
        let mut candidate = part;
        loop {
            if bash_pattern_matches(pattern, candidate) {
                return true;
            }
            let Some(rest) = strip_selection_prefix(candidate) else {
                break;
            };
            candidate = rest;
        }
    }
    false
}

fn strip_selection_prefix(command: &str) -> Option<&str> {
    let ends = shell_token_ends(command)?;
    let mut start = 0;
    let words = ends
        .iter()
        .map(|end| {
            let word = command[start..*end].trim();
            start = *end;
            word
        })
        .collect::<Vec<_>>();
    let is_assignment = is_literal_assignment(words[0]);
    let mut consumed = if is_assignment {
        1
    } else {
        match words[0] {
            "time" | "nohup" => 1,
            "nice" => match words.get(1).copied() {
                Some("-n") => {
                    words.get(2)?.parse::<i64>().ok()?;
                    3
                }
                Some(option) if option.starts_with('-') && option != "--" => {
                    option.parse::<i64>().ok()?;
                    2
                }
                _ => 1,
            },
            "timeout" => timeout_command_index(&words)?,
            "stdbuf" => {
                let mut index = 1;
                while words.get(index).is_some_and(|word| {
                    let bytes = word.as_bytes();
                    bytes.len() > 2
                        && bytes[0] == b'-'
                        && matches!(bytes[1], b'i' | b'o' | b'e')
                        && bytes[2..]
                            .iter()
                            .all(|byte| matches!(byte, b'L' | b'N' | b'0'..=b'9'))
                }) {
                    index += 1;
                }
                if index == 1 {
                    return None;
                }
                index
            }
            _ => return None,
        }
    };
    if !is_assignment && words[0] != "timeout" && words.get(consumed) == Some(&"--") {
        consumed += 1;
    }
    let rest = command[*ends.get(consumed - 1)?..].trim_start();
    (!rest.is_empty()).then_some(rest)
}

fn timeout_command_index(words: &[&str]) -> Option<usize> {
    let mut index = 1;
    loop {
        let word = *words.get(index)?;
        match word {
            "--foreground" | "--preserve-status" | "--verbose" | "-v" => index += 1,
            "--kill-after" | "--signal" | "-k" | "-s" => {
                if !is_literal_flag_value(words.get(index + 1)?) {
                    return None;
                }
                index += 2;
            }
            "--" => {
                index += 1;
                break;
            }
            _ if word.starts_with('-') => {
                let value = word
                    .strip_prefix("--kill-after=")
                    .or_else(|| word.strip_prefix("--signal="))
                    .or_else(|| word.strip_prefix("-k"))
                    .or_else(|| word.strip_prefix("-s"))?;
                if !is_literal_flag_value(value) {
                    return None;
                }
                index += 1;
            }
            _ => break,
        }
    }
    let duration = words.get(index)?;
    let duration = duration
        .strip_suffix('s')
        .or_else(|| duration.strip_suffix('m'))
        .or_else(|| duration.strip_suffix('h'))
        .or_else(|| duration.strip_suffix('d'))
        .unwrap_or(duration);
    let mut decimal = duration.split('.');
    let digits = |value: &str| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit());
    if !digits(decimal.next()?)
        || decimal.next().is_some_and(|part| !digits(part))
        || decimal.next().is_some()
    {
        return None;
    }
    Some(index + 1)
}

fn is_literal_flag_value(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'+' | b'-'))
}

fn is_literal_assignment(word: &str) -> bool {
    let Some((name, value)) = word.split_once('=') else {
        return false;
    };
    let name = name.strip_suffix('+').unwrap_or(name);
    let name = if let Some((base, index)) = name.split_once('[') {
        if !index.strip_suffix(']').is_some_and(|index| {
            !index.is_empty()
                && index
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        }) {
            return false;
        }
        base
    } else {
        name
    };
    if name == "PATH"
        || name.starts_with("LD_")
        || name.starts_with("DYLD_")
        || !name
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return false;
    }
    let mut quote = None;
    let mut escaped = false;
    for byte in value.bytes() {
        if escaped {
            escaped = false;
            continue;
        }
        if byte == b'\\' && quote != Some(b'\'') {
            escaped = true;
            continue;
        }
        if matches!(byte, b'\'' | b'"') {
            if quote == Some(byte) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(byte);
            }
        } else if matches!(byte, b'\n' | b'\r')
            || quote != Some(b'\'') && matches!(byte, b'$' | b'`')
            || quote.is_none() && matches!(byte, b';' | b'|' | b'&' | b'(' | b')' | b'<' | b'>')
        {
            return false;
        }
    }
    !escaped && quote.is_none()
}

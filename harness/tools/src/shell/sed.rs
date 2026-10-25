//! Port of Claude Code's `tools/BashTool/sedValidation.ts` allowlist.

use std::sync::LazyLock;

use regex::Regex;

use super::syntax::{QuoteToken, quote_tokens};

static SED_PREFIX: LazyLock<Regex> = LazyLock::new(|| pattern(r"^\s*sed\s+"));
static PRINT_COMMAND: LazyLock<Regex> = LazyLock::new(|| pattern(r"^(?:[0-9]+|[0-9]+,[0-9]+)?p$"));
static SUBSTITUTION_FLAGS: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"^[gpimIM]*[1-9]?[gpimIM]*$"));
static DANGEROUS_FLAG_COMBINATION: LazyLock<Regex> = LazyLock::new(|| pattern(r"-e[wWe]|-w[eE]"));
static NEGATION: LazyLock<Regex> = LazyLock::new(|| pattern(r"^!|[/0-9$]!"));
static STEP_ADDRESS: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"[0-9]\s*~\s*[0-9]|,\s*~\s*[0-9]|\$\s*~\s*[0-9]"));
static OFFSET_ADDRESS: LazyLock<Regex> = LazyLock::new(|| pattern(r",\s*[+-]"));
static BACKSLASH_DELIMITER: LazyLock<Regex> = LazyLock::new(|| pattern(r"s\\|\\[|#%@]"));
static ESCAPED_SLASH_WRITE: LazyLock<Regex> = LazyLock::new(|| pattern(r"\\/.*[wW]"));
static PATTERN_THEN_COMMAND: LazyLock<Regex> = LazyLock::new(|| pattern(r"/[^/]*\s+[wWeE]"));
static WELL_FORMED_SLASH_SUBSTITUTION: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"^s/[^/]*/[^/]*/[^/]*$"));
static WRITE_COMMANDS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"^[wW]\s*\S+",
        r"^[0-9]+\s*[wW]\s*\S+",
        r"^\$\s*[wW]\s*\S+",
        r"^/[^/]*/[IMim]*\s*[wW]\s*\S+",
        r"^[0-9]+,[0-9]+\s*[wW]\s*\S+",
        r"^[0-9]+,\$\s*[wW]\s*\S+",
        r"^/[^/]*/[IMim]*,/[^/]*/[IMim]*\s*[wW]\s*\S+",
    ]
    .into_iter()
    .map(pattern)
    .collect()
});
static EXECUTE_COMMANDS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"^e",
        r"^[0-9]+\s*e",
        r"^\$\s*e",
        r"^/[^/]*/[IMim]*\s*e",
        r"^[0-9]+,[0-9]+\s*e",
        r"^[0-9]+,\$\s*e",
        r"^/[^/]*/[IMim]*,/[^/]*/[IMim]*\s*e",
    ]
    .into_iter()
    .map(pattern)
    .collect()
});

const LINE_PRINTING_FLAGS: &[&str] = &[
    "-n",
    "--quiet",
    "--silent",
    "-E",
    "--regexp-extended",
    "-r",
    "-z",
    "--zero-terminated",
    "--posix",
];

fn pattern(source: &str) -> Regex {
    Regex::new(source).expect("static sed validation pattern")
}

/// Port of `sedCommandIsAllowedByAllowlist`.
pub(super) fn sed_command_is_allowed(command: &str, allow_file_writes: bool) -> bool {
    let Some(expressions) = extract_sed_expressions(command) else {
        return false;
    };
    let has_file_arguments = has_file_arguments(command);
    let (is_line_printing, is_substitution) = if allow_file_writes {
        (
            false,
            is_substitution_command(command, &expressions, has_file_arguments, true),
        )
    } else {
        (
            is_line_printing_command(command, &expressions),
            is_substitution_command(command, &expressions, has_file_arguments, false),
        )
    };
    if !is_line_printing && !is_substitution {
        return false;
    }
    if is_substitution
        && expressions
            .iter()
            .any(|expression| expression.contains(';'))
    {
        return false;
    }
    !expressions
        .iter()
        .any(|expression| contains_dangerous_operations(expression))
}

fn arguments_after_sed(command: &str) -> Option<Vec<QuoteToken>> {
    let prefix = SED_PREFIX.find(command)?;
    quote_tokens(&command[prefix.end()..])
}

fn flags_of(tokens: &[QuoteToken]) -> Vec<&str> {
    tokens
        .iter()
        .filter_map(|token| match token {
            QuoteToken::Word(word) if word.starts_with('-') && word != "--" => Some(word.as_str()),
            _ => None,
        })
        .collect()
}

fn flags_are_allowed(flags: &[&str], allowed: &[&str]) -> bool {
    flags.iter().all(|flag| {
        if flag.starts_with('-') && !flag.starts_with("--") && flag.len() > 2 {
            flag[1..]
                .chars()
                .all(|single| allowed.contains(&format!("-{single}").as_str()))
        } else {
            allowed.contains(flag)
        }
    })
}

fn is_line_printing_command(command: &str, expressions: &[String]) -> bool {
    let Some(tokens) = arguments_after_sed(command) else {
        return false;
    };
    let flags = flags_of(&tokens);
    if !flags_are_allowed(&flags, LINE_PRINTING_FLAGS) {
        return false;
    }
    let has_quiet_flag = flags.iter().any(|flag| {
        matches!(*flag, "-n" | "--quiet" | "--silent")
            || (flag.starts_with('-') && !flag.starts_with("--") && flag.contains('n'))
    });
    has_quiet_flag
        && !expressions.is_empty()
        && expressions.iter().all(|expression| {
            expression
                .split(';')
                .all(|command| PRINT_COMMAND.is_match(command.trim()))
        })
}

fn is_substitution_command(
    command: &str,
    expressions: &[String],
    has_file_arguments: bool,
    allow_file_writes: bool,
) -> bool {
    if !allow_file_writes && has_file_arguments {
        return false;
    }
    let Some(tokens) = arguments_after_sed(command) else {
        return false;
    };
    let mut allowed_flags = vec!["-E", "--regexp-extended", "-r", "--posix"];
    if allow_file_writes {
        allowed_flags.extend(["-i", "--in-place"]);
    }
    if !flags_are_allowed(&flags_of(&tokens), &allowed_flags) || expressions.len() != 1 {
        return false;
    }
    let expression = expressions[0].trim();
    let Some(rest) = expression.strip_prefix("s/") else {
        return false;
    };
    if rest.contains(['\n', '\r', '\u{2028}', '\u{2029}']) {
        return false;
    }
    let chars = rest.chars().collect::<Vec<_>>();
    let mut delimiter_count = 0;
    let mut last_delimiter = None;
    let mut index = 0;
    while index < chars.len() {
        if chars[index] == '\\' {
            index += 2;
            continue;
        }
        if chars[index] == '/' {
            delimiter_count += 1;
            last_delimiter = Some(index);
        }
        index += 1;
    }
    let Some(last_delimiter) = last_delimiter.filter(|_| delimiter_count == 2) else {
        return false;
    };
    let flags = chars[last_delimiter + 1..].iter().collect::<String>();
    SUBSTITUTION_FLAGS.is_match(&flags)
}

/// Port of `hasFileArgs`.
fn has_file_arguments(command: &str) -> bool {
    if !SED_PREFIX.is_match(command) {
        return false;
    }
    let Some(tokens) = arguments_after_sed(command) else {
        return true;
    };
    let mut argument_count = 0;
    let mut has_expression_flag = false;
    let mut index = 0;
    while index < tokens.len() {
        match &tokens[index] {
            QuoteToken::Glob(_) => return true,
            QuoteToken::Word(argument) => {
                if matches!(argument.as_str(), "-e" | "--expression") && index + 1 < tokens.len() {
                    has_expression_flag = true;
                    index += 2;
                    continue;
                }
                if argument.starts_with("--expression=") || argument.starts_with("-e=") {
                    has_expression_flag = true;
                } else if !argument.starts_with('-') {
                    argument_count += 1;
                    if has_expression_flag || argument_count > 1 {
                        return true;
                    }
                }
            }
            QuoteToken::Operator(_) | QuoteToken::Comment => {}
        }
        index += 1;
    }
    false
}

/// Port of `extractSedExpressions`; `None` stands for the thrown parse errors.
fn extract_sed_expressions(command: &str) -> Option<Vec<String>> {
    let Some(prefix) = SED_PREFIX.find(command) else {
        return Some(Vec::new());
    };
    let without_sed = &command[prefix.end()..];
    if DANGEROUS_FLAG_COMBINATION.is_match(without_sed) {
        return None;
    }
    let tokens = quote_tokens(without_sed)?;
    let mut expressions = Vec::new();
    let mut found_expression_flag = false;
    let mut found_expression = false;
    let mut index = 0;
    while index < tokens.len() {
        let QuoteToken::Word(argument) = &tokens[index] else {
            index += 1;
            continue;
        };
        if matches!(argument.as_str(), "-e" | "--expression") && index + 1 < tokens.len() {
            found_expression_flag = true;
            if let QuoteToken::Word(next) = &tokens[index + 1] {
                expressions.push(next.clone());
                index += 1;
            }
        } else if let Some(value) = argument.strip_prefix("--expression=") {
            found_expression_flag = true;
            expressions.push(value.to_owned());
        } else if let Some(value) = argument.strip_prefix("-e=") {
            found_expression_flag = true;
            expressions.push(value.to_owned());
        } else if !argument.starts_with('-') {
            if found_expression_flag || found_expression {
                break;
            }
            expressions.push(argument.clone());
            found_expression = true;
        }
        index += 1;
    }
    Some(expressions)
}

/// `s([^\\\n]).*?\1.*?\1(.*?)$`: flags after the third delimiter of the first substitution.
fn substitution_flags(command: &str) -> Option<String> {
    let chars = command.chars().collect::<Vec<_>>();
    (0..chars.len()).find_map(|start| {
        if chars[start] != 's' {
            return None;
        }
        let delimiter = *chars.get(start + 1)?;
        if matches!(delimiter, '\\' | '\n') {
            return None;
        }
        let first = start + 2 + chars[start + 2..].iter().position(|c| *c == delimiter)?;
        let second = first + 1 + chars[first + 1..].iter().position(|c| *c == delimiter)?;
        Some(chars[second + 1..].iter().collect())
    })
}

/// `^s([^\\\n]).*?\1.*?\1[^wWeE]*$`.
fn is_proper_substitution(command: &str) -> bool {
    let chars = command.chars().collect::<Vec<_>>();
    let Some(&delimiter) = chars.get(1) else {
        return false;
    };
    if chars[0] != 's' || matches!(delimiter, '\\' | '\n') {
        return false;
    }
    (2..chars.len())
        .filter(|first| chars[*first] == delimiter)
        .any(|first| {
            (first + 1..chars.len())
                .filter(|second| chars[*second] == delimiter)
                .any(|second| {
                    chars[second + 1..]
                        .iter()
                        .all(|character| !matches!(character, 'w' | 'W' | 'e' | 'E'))
                })
        })
}

fn contains_dangerous_operations(expression: &str) -> bool {
    let command = expression.trim();
    if command.is_empty() {
        return false;
    }
    if command
        .chars()
        .any(|character| !('\u{1}'..='\u{7F}').contains(&character))
        || command.contains(['{', '}', '\n'])
    {
        return true;
    }
    if let Some(hash) = command.find('#')
        && !(hash > 0 && command.as_bytes()[hash - 1] == b's')
    {
        return true;
    }
    if NEGATION.is_match(command)
        || STEP_ADDRESS.is_match(command)
        || command.starts_with(',')
        || OFFSET_ADDRESS.is_match(command)
        || BACKSLASH_DELIMITER.is_match(command)
        || ESCAPED_SLASH_WRITE.is_match(command)
        || PATTERN_THEN_COMMAND.is_match(command)
    {
        return true;
    }
    if command.starts_with("s/") && !WELL_FORMED_SLASH_SUBSTITUTION.is_match(command) {
        return true;
    }
    let ends_with_dangerous = command.ends_with(['w', 'W', 'e', 'E']);
    if command.len() >= 2
        && command.starts_with('s')
        && ends_with_dangerous
        && !is_proper_substitution(command)
    {
        return true;
    }
    if WRITE_COMMANDS
        .iter()
        .any(|pattern| pattern.is_match(command))
        || EXECUTE_COMMANDS
            .iter()
            .any(|pattern| pattern.is_match(command))
    {
        return true;
    }
    if substitution_flags(command).is_some_and(|flags| flags.contains(['w', 'W', 'e', 'E'])) {
        return true;
    }
    let chars = command.chars().collect::<Vec<_>>();
    let has_transliteration = chars
        .windows(2)
        .any(|pair| pair[0] == 'y' && !matches!(pair[1], '\\' | '\n'));
    has_transliteration && command.contains(['w', 'W', 'e', 'E'])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_sed_forms_are_allowed_without_file_writes() {
        for command in [
            "sed -n '1,5p' notes.txt",
            "sed -n '3p;7p' notes.txt",
            "sed -nE 10p notes.txt",
            "sed 's/foo/bar/g'",
            "sed -E 's/a+/b/2'",
            "sed -n '1p' *.txt",
        ] {
            assert!(sed_command_is_allowed(command, false), "{command}");
        }
    }

    #[test]
    fn dangerous_or_writing_sed_forms_are_rejected_without_file_writes() {
        for command in [
            "sed 's/foo/bar/' notes.txt",
            "sed -i 's/foo/bar/' notes.txt",
            "sed -n '1p' -i notes.txt",
            "sed 's/a/b/w out.txt'",
            "sed 's/a/b/e'",
            "sed '1e id'",
            "sed 'w out.txt'",
            "sed -n '/x/w out.txt' notes.txt",
            "sed 's|a|b|'",
            "sed 's/a/b/;s/c/d/'",
            "sed '1!d'",
            "sed '{p}'",
            "sed 'y/abc/xyw/'",
            "sed -ew 'p'",
            "sed -n '1,+3p' notes.txt",
            "sed -n '0~2p' notes.txt",
            "sed -n 'l' notes.txt",
            "sed -e 's/a/b/'",
        ] {
            assert!(!sed_command_is_allowed(command, false), "{command}");
        }
    }

    #[test]
    fn in_place_substitutions_are_allowed_only_with_file_writes() {
        assert!(sed_command_is_allowed(
            "sed -i 's/foo/bar/g' notes.txt",
            true
        ));
        assert!(sed_command_is_allowed("sed 's/foo/bar/' notes.txt", true));
        assert!(!sed_command_is_allowed(
            "sed -i 's/foo/bar/w x' notes.txt",
            true
        ));
        assert!(!sed_command_is_allowed("sed -n '1p' notes.txt", true));
        assert!(!sed_command_is_allowed(
            "sed -i.bak 's/a/b/' notes.txt",
            true
        ));
    }
}

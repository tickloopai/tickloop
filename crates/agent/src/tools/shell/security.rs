//! Port of Claude Code's legacy injection validators (`tools/BashTool/bashSecurity.ts`,
//! `bashCommandIsSafe_DEPRECATED`) and the command normalizers from
//! `tools/BashTool/bashPermissions.ts` (`stripSafeWrappers`, `isNormalizedCdCommand`,
//! `isNormalizedGitCommand`). Heredoc and `git commit` early exits are omitted because the
//! syntax layer rejects heredocs and neither form can be auto-allowed.

use std::sync::LazyLock;

use regex::Regex;

use super::syntax::{QuoteToken, quote_tokens};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Safety {
    Passthrough,
    Ask { misparsing: bool },
}

fn regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("static shell validation pattern")
}

macro_rules! static_regex {
    ($name:ident, $pattern:expr) => {
        static $name: LazyLock<Regex> = LazyLock::new(|| regex($pattern));
    };
}

static_regex!(CONTROL_CHARACTERS, r"[\x00-\x08\x0B\x0C\x0E-\x1F\x7F]");
static_regex!(STDERR_TO_STDOUT, r"\s+2\s*>&\s*1");
static_regex!(OUTPUT_TO_DEV_NULL, r"[012]?\s*>\s*/dev/null");
static_regex!(INPUT_FROM_DEV_NULL, r"\s*<\s*/dev/null");
static_regex!(STARTS_WITH_TAB, r"^\s*\t");
static_regex!(STARTS_WITH_OPERATOR, r"^\s*(&&|\|\||;|>>?|<)");
static_regex!(JQ_SYSTEM, r"(?-u:\b)system\s*\(");
static_regex!(
    JQ_FILE_FLAGS,
    r"(?:^|\s)(?:-f(?-u:\b)|--from-file|--rawfile|--slurpfile|-L(?-u:\b)|--library-path)"
);
static_regex!(
    QUOTED_METACHARACTER,
    r#"(?:^|\s)["'][^"']*[;&][^"']*["'](?:\s|$)"#
);
static_regex!(
    FIND_NAME_METACHARACTER,
    r#"-(?:name|path|iname)\s+["'][^"']*[;|&][^"']*["']"#
);
static_regex!(
    FIND_REGEX_METACHARACTER,
    r#"-regex\s+["'][^"']*[;&][^"']*["']"#
);
static_regex!(VARIABLE_AFTER_OPERATOR, r"[<>|]\s*\$[A-Za-z_]");
static_regex!(
    VARIABLE_BEFORE_OPERATOR,
    r"\$[A-Za-z_][A-Za-z0-9_]*\s*[|<>]"
);
static_regex!(IFS_USAGE, r"\$IFS|\$\{[^}]*IFS");
static_regex!(PROC_ENVIRON, r"/proc/.*/environ");
static_regex!(ANSI_C_QUOTE, r"\$'[^']*'");
static_regex!(LOCALE_QUOTE, r#"\$"[^"]*""#);
static_regex!(EMPTY_SPECIAL_QUOTE_DASH, r#"\$['"]{2}\s*-"#);
static_regex!(EMPTY_QUOTES_DASH, r#"(?:^|\s)(?:''|"")+\s*-"#);
static_regex!(EMPTY_PAIR_QUOTED_DASH, r#"(?:""|'')+['"]-"#);
static_regex!(CONSECUTIVE_QUOTES, r#"(?:^|\s)['"]{3,}"#);
static_regex!(SPACE_QUOTE_DASH, r#"\s['"`]-"#);
static_regex!(DOUBLE_QUOTE_DASH, r#"['"`]{2}-"#);
static_regex!(FLAG_CHARS_INSIDE, r"^-+[a-zA-Z0-9$`]");
static_regex!(ONLY_DASHES, r"^-+$");
static_regex!(QUOTED_SINGLE_BRACE, r#"['"][{}]['"]"#);
static_regex!(
    UNICODE_WHITESPACE,
    r"[\u00A0\u1680\u2000-\u200A\u2028\u2029\u202F\u205F\u3000\uFEFF]"
);
static_regex!(ENV_ASSIGNMENT_TOKEN, r"^[A-Za-z_][A-Za-z0-9_]*=");
static_regex!(FC_EDITOR_FLAG, r"\s-\S*e");
static_regex!(
    SAFE_ENV_VAR_PREFIX,
    r"^([A-Za-z_][A-Za-z0-9_]*)=([A-Za-z0-9_./:-]+)[ \t]+"
);
static_regex!(
    TIMEOUT_WRAPPER,
    r"^timeout[ \t]+(?:(?:--(?:foreground|preserve-status|verbose)|--(?:kill-after|signal)=[A-Za-z0-9_.+-]+|--(?:kill-after|signal)[ \t]+[A-Za-z0-9_.+-]+|-v|-[ks][ \t]+[A-Za-z0-9_.+-]+|-[ks][A-Za-z0-9_.+-]+)[ \t]+)*(?:--[ \t]+)?[0-9]+(?:\.[0-9]+)?[smhd]?[ \t]+"
);
static_regex!(TIME_WRAPPER, r"^time[ \t]+(?:--[ \t]+)?");
static_regex!(
    NICE_WRAPPER,
    r"^nice(?:[ \t]+-n[ \t]+-?[0-9]+|[ \t]+-[0-9]+)?[ \t]+(?:--[ \t]+)?"
);
static_regex!(
    STDBUF_WRAPPER,
    r"^stdbuf(?:[ \t]+-[ioe][LN0-9]+)+[ \t]+(?:--[ \t]+)?"
);
static_regex!(NOHUP_WRAPPER, r"^nohup[ \t]+(?:--[ \t]+)?");
static_regex!(GIT_COMMAND_FALLBACK, r"^git(?:\s|$)");
static_regex!(CD_COMMAND_FALLBACK, r"^(?:cd|pushd|popd)(?:\s|$)");

/// Command substitution forms checked in `validateDangerousPatterns`; backticks are separate.
const COMMAND_SUBSTITUTION_PATTERNS: &[&str] = &[
    r"<\(",
    r">\(",
    r"=\(",
    r"(?:^|[\s;&|])=[a-zA-Z_]",
    r"\$\(",
    r"\$\{",
    r"\$\[",
    r"~\[",
    r"\(e:",
    r"\(\+",
    r"\}\s*always\s*\{",
    r"<#",
];
static COMMAND_SUBSTITUTIONS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    COMMAND_SUBSTITUTION_PATTERNS
        .iter()
        .map(|pattern| regex(pattern))
        .collect()
});

const ZSH_DANGEROUS_COMMANDS: &[&str] = &[
    "zmodload", "emulate", "sysopen", "sysread", "syswrite", "sysseek", "zpty", "ztcp", "zsocket",
    "mapfile", "zf_rm", "zf_mv", "zf_ln", "zf_chmod", "zf_chown", "zf_mkdir", "zf_rmdir",
    "zf_chgrp",
];
const ZSH_PRECOMMAND_MODIFIERS: &[&str] = &["command", "builtin", "noglob", "nocorrect"];
const SAFE_ENV_VARS: &[&str] = &[
    "GOEXPERIMENT",
    "GOOS",
    "GOARCH",
    "CGO_ENABLED",
    "GO111MODULE",
    "RUST_BACKTRACE",
    "RUST_LOG",
    "NODE_ENV",
    "PYTHONUNBUFFERED",
    "PYTHONDONTWRITEBYTECODE",
    "PYTEST_DISABLE_PLUGIN_AUTOLOAD",
    "PYTEST_DEBUG",
    "ANTHROPIC_API_KEY",
    "LANG",
    "LANGUAGE",
    "LC_ALL",
    "LC_CTYPE",
    "LC_TIME",
    "CHARSET",
    "TERM",
    "COLORTERM",
    "NO_COLOR",
    "FORCE_COLOR",
    "TZ",
    "LS_COLORS",
    "LSCOLORS",
    "GREP_COLOR",
    "GREP_COLORS",
    "GCC_COLORS",
    "TIME_STYLE",
    "BLOCK_SIZE",
    "BLOCKSIZE",
];

struct QuoteExtraction {
    with_double_quotes: String,
    fully_unquoted: String,
    unquoted_keep_quote_chars: String,
}

type Validator = fn(&ValidationContext) -> bool;

struct ValidationContext<'a> {
    original: &'a str,
    chars: Vec<char>,
    base_command: &'a str,
    unquoted_content: String,
    fully_unquoted_content: String,
    fully_unquoted_pre_strip: String,
    unquoted_keep_quote_chars: String,
}

/// Port of `bashCommandIsSafe_DEPRECATED`.
pub(super) fn bash_command_is_safe(command: &str) -> Safety {
    if CONTROL_CHARACTERS.is_match(command) || has_shell_quote_single_quote_bug(command) {
        return Safety::Ask { misparsing: true };
    }
    let base_command = command.split(' ').next().unwrap_or_default();
    let extraction = extract_quoted_content(command, base_command == "jq");
    let context = ValidationContext {
        original: command,
        chars: command.chars().collect(),
        base_command,
        unquoted_content: extraction.with_double_quotes,
        fully_unquoted_content: strip_safe_redirections(&extraction.fully_unquoted),
        fully_unquoted_pre_strip: extraction.fully_unquoted,
        unquoted_keep_quote_chars: extraction.unquoted_keep_quote_chars,
    };
    if command.trim().is_empty() {
        return Safety::Passthrough;
    }
    if STARTS_WITH_TAB.is_match(command)
        || command.trim().starts_with('-')
        || STARTS_WITH_OPERATOR.is_match(command)
    {
        return Safety::Ask { misparsing: true };
    }

    let validators: [(Validator, bool); 19] = [
        (has_dangerous_jq, true),
        (has_obfuscated_flags, true),
        (has_quoted_shell_metacharacters, true),
        (has_dangerous_variables, true),
        (has_comment_quote_desync, true),
        (has_quoted_newline_before_hash, true),
        (has_carriage_return_outside_double_quotes, true),
        (has_command_separating_newline, false),
        (|context| IFS_USAGE.is_match(context.original), true),
        (|context| PROC_ENVIRON.is_match(context.original), true),
        (has_dangerous_patterns, true),
        (has_redirection, false),
        (
            |context| has_backslash_escaped_whitespace(context.original),
            true,
        ),
        (
            |context| has_backslash_escaped_operator(context.original),
            true,
        ),
        (
            |context| UNICODE_WHITESPACE.is_match(context.original),
            true,
        ),
        (has_mid_word_hash, true),
        (has_brace_expansion, true),
        (has_zsh_dangerous_command, true),
        (has_malformed_token_injection, true),
    ];
    let mut deferred = Safety::Passthrough;
    for (validator, misparsing) in validators {
        if validator(&context) {
            if misparsing {
                return Safety::Ask { misparsing: true };
            }
            deferred = Safety::Ask { misparsing: false };
        }
    }
    deferred
}

fn extract_quoted_content(command: &str, is_jq: bool) -> QuoteExtraction {
    let mut extraction = QuoteExtraction {
        with_double_quotes: String::new(),
        fully_unquoted: String::new(),
        unquoted_keep_quote_chars: String::new(),
    };
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut escaped = false;
    for character in command.chars() {
        if escaped || (character == '\\' && !in_single_quote) {
            escaped = !escaped;
            if !in_single_quote {
                extraction.with_double_quotes.push(character);
            }
            if !in_single_quote && !in_double_quote {
                extraction.fully_unquoted.push(character);
                extraction.unquoted_keep_quote_chars.push(character);
            }
            continue;
        }
        if character == '\'' && !in_double_quote {
            in_single_quote = !in_single_quote;
            extraction.unquoted_keep_quote_chars.push(character);
            continue;
        }
        if character == '"' && !in_single_quote {
            in_double_quote = !in_double_quote;
            extraction.unquoted_keep_quote_chars.push(character);
            if !is_jq {
                continue;
            }
        }
        if !in_single_quote {
            extraction.with_double_quotes.push(character);
        }
        if !in_single_quote && !in_double_quote {
            extraction.fully_unquoted.push(character);
            extraction.unquoted_keep_quote_chars.push(character);
        }
    }
    extraction
}

/// Removes every match of `pattern` that is followed by whitespace or the end of `text`,
/// emulating the `(?=\s|$)` lookahead of the original JavaScript expressions.
fn remove_matches_before_boundary(text: &str, pattern: &Regex) -> String {
    let mut result = String::new();
    let mut copied_until = 0;
    let mut search_from = 0;
    while search_from <= text.len() {
        let Some(found) = pattern.find_at(text, search_from) else {
            break;
        };
        let is_boundary = text[found.end()..]
            .chars()
            .next()
            .is_none_or(char::is_whitespace);
        if is_boundary && found.end() > found.start() {
            result.push_str(&text[copied_until..found.start()]);
            copied_until = found.end();
            search_from = found.end();
        } else {
            search_from = text[found.start()..]
                .chars()
                .next()
                .map_or(text.len() + 1, |character| {
                    found.start() + character.len_utf8()
                });
        }
    }
    result.push_str(&text[copied_until..]);
    result
}

fn strip_safe_redirections(content: &str) -> String {
    let content = remove_matches_before_boundary(content, &STDERR_TO_STDOUT);
    let content = remove_matches_before_boundary(&content, &OUTPUT_TO_DEV_NULL);
    remove_matches_before_boundary(&content, &INPUT_FROM_DEV_NULL)
}

fn has_unescaped_char(content: &str, target: char) -> bool {
    let chars = content.chars().collect::<Vec<_>>();
    let mut index = 0;
    while index < chars.len() {
        if chars[index] == '\\' && index + 1 < chars.len() {
            index += 2;
            continue;
        }
        if chars[index] == target {
            return true;
        }
        index += 1;
    }
    false
}

fn has_dangerous_jq(context: &ValidationContext) -> bool {
    context.base_command == "jq"
        && (JQ_SYSTEM.is_match(context.original)
            || JQ_FILE_FLAGS.is_match(context.original.get(3..).unwrap_or_default().trim()))
}

fn is_quote(character: char) -> bool {
    matches!(character, '\'' | '"' | '`')
}

fn is_flag_continuation(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '\\' | '$' | '{' | '`' | '-')
}

/// Port of `validateObfuscatedFlags`.
fn has_obfuscated_flags(context: &ValidationContext) -> bool {
    let original = context.original;
    let has_shell_operators = original.contains(['|', '&', ';']);
    if context.base_command == "echo" && !has_shell_operators {
        return false;
    }
    if ANSI_C_QUOTE.is_match(original)
        || LOCALE_QUOTE.is_match(original)
        || EMPTY_SPECIAL_QUOTE_DASH.is_match(original)
        || EMPTY_QUOTES_DASH.is_match(original)
        || EMPTY_PAIR_QUOTED_DASH.is_match(original)
        || CONSECUTIVE_QUOTES.is_match(original)
    {
        return true;
    }
    let chars = &context.chars;
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut escaped = false;
    for index in 0..chars.len().saturating_sub(1) {
        let current = chars[index];
        let next = chars[index + 1];
        if escaped {
            escaped = false;
            continue;
        }
        if current == '\\' && !in_single_quote {
            escaped = true;
            continue;
        }
        if current == '\'' && !in_double_quote {
            in_single_quote = !in_single_quote;
            continue;
        }
        if current == '"' && !in_single_quote {
            in_double_quote = !in_double_quote;
            continue;
        }
        if in_single_quote || in_double_quote {
            continue;
        }
        if current.is_whitespace() && is_quote(next) && quoted_segment_forms_flag(chars, index + 1)
        {
            return true;
        }
        if current.is_whitespace() && next == '-' {
            let mut flag_content = String::new();
            let mut cursor = index + 1;
            while let Some(&flag_char) = chars.get(cursor) {
                if flag_char.is_whitespace() || flag_char == '=' {
                    break;
                }
                if is_quote(flag_char) {
                    if context.base_command == "cut" && flag_content == "-d" {
                        break;
                    }
                    if let Some(&after) = chars.get(cursor + 1)
                        && !(after.is_ascii_alphanumeric()
                            || matches!(after, '_' | '\'' | '"' | '-'))
                    {
                        break;
                    }
                }
                flag_content.push(flag_char);
                cursor += 1;
            }
            if flag_content.contains(['"', '\'']) {
                return true;
            }
        }
    }
    SPACE_QUOTE_DASH.is_match(&context.fully_unquoted_content)
        || DOUBLE_QUOTE_DASH.is_match(&context.fully_unquoted_content)
}

fn quoted_segment_forms_flag(chars: &[char], quote_index: usize) -> bool {
    let quote_char = chars[quote_index];
    let mut end = quote_index + 1;
    let mut inside_quote = String::new();
    while end < chars.len() && chars[end] != quote_char {
        inside_quote.push(chars[end]);
        end += 1;
    }
    if end >= chars.len() {
        return false;
    }
    let char_after_quote = chars.get(end + 1).copied();
    let has_flag_chars_inside = FLAG_CHARS_INSIDE.is_match(&inside_quote);
    let has_flag_chars_continuing =
        ONLY_DASHES.is_match(&inside_quote) && char_after_quote.is_some_and(is_flag_continuation);
    let has_flag_chars_in_next_quote = (inside_quote.is_empty()
        || ONLY_DASHES.is_match(&inside_quote))
        && char_after_quote.is_some_and(is_quote)
        && chained_quotes_form_flag(chars, end + 1, inside_quote.clone());
    has_flag_chars_inside || has_flag_chars_continuing || has_flag_chars_in_next_quote
}

fn chained_quotes_form_flag(chars: &[char], start: usize, mut combined: String) -> bool {
    let mut position = start;
    while position < chars.len() && is_quote(chars[position]) {
        let segment_quote = chars[position];
        let mut end = position + 1;
        while end < chars.len() && chars[end] != segment_quote {
            end += 1;
        }
        let segment = chars[position + 1..end.min(chars.len())]
            .iter()
            .collect::<String>();
        let prior = combined.clone();
        combined.push_str(&segment);
        if FLAG_CHARS_INSIDE.is_match(&combined) {
            return true;
        }
        if ONLY_DASHES.is_match(&prior)
            && segment.chars().any(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '$' | '`')
            })
        {
            return true;
        }
        if end >= chars.len() {
            break;
        }
        position = end + 1;
    }
    if let Some(&next) = chars.get(position)
        && is_flag_continuation(next)
    {
        if ONLY_DASHES.is_match(&combined) || combined.is_empty() {
            if next == '-' {
                return true;
            }
            if (next.is_ascii_alphanumeric() || matches!(next, '\\' | '$' | '{' | '`'))
                && !combined.is_empty()
            {
                return true;
            }
        }
        if combined.starts_with('-') {
            return true;
        }
    }
    false
}

fn has_quoted_shell_metacharacters(context: &ValidationContext) -> bool {
    QUOTED_METACHARACTER.is_match(&context.unquoted_content)
        || FIND_NAME_METACHARACTER.is_match(&context.unquoted_content)
        || FIND_REGEX_METACHARACTER.is_match(&context.unquoted_content)
}

fn has_dangerous_variables(context: &ValidationContext) -> bool {
    VARIABLE_AFTER_OPERATOR.is_match(&context.fully_unquoted_content)
        || VARIABLE_BEFORE_OPERATOR.is_match(&context.fully_unquoted_content)
}

fn has_comment_quote_desync(context: &ValidationContext) -> bool {
    let chars = &context.chars;
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut escaped = false;
    let mut index = 0;
    while index < chars.len() {
        let character = chars[index];
        index += 1;
        if escaped {
            escaped = false;
            continue;
        }
        if in_single_quote {
            in_single_quote = character != '\'';
            continue;
        }
        if character == '\\' {
            escaped = true;
            continue;
        }
        if in_double_quote {
            in_double_quote = character != '"';
            continue;
        }
        match character {
            '\'' => in_single_quote = true,
            '"' => in_double_quote = true,
            '#' => {
                let line_end = chars[index..]
                    .iter()
                    .position(|character| *character == '\n')
                    .map_or(chars.len(), |offset| index + offset);
                if chars[index..line_end]
                    .iter()
                    .any(|character| matches!(character, '\'' | '"'))
                {
                    return true;
                }
                index = line_end + 1;
            }
            _ => {}
        }
    }
    false
}

fn has_quoted_newline_before_hash(context: &ValidationContext) -> bool {
    let original = context.original;
    if !original.contains('\n') || !original.contains('#') {
        return false;
    }
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut escaped = false;
    for (index, character) in original.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if character == '\\' && !in_single_quote {
            escaped = true;
            continue;
        }
        if character == '\'' && !in_double_quote {
            in_single_quote = !in_single_quote;
            continue;
        }
        if character == '"' && !in_single_quote {
            in_double_quote = !in_double_quote;
            continue;
        }
        if character == '\n' && (in_single_quote || in_double_quote) {
            let next_line = original[index + 1..].split('\n').next().unwrap_or_default();
            if next_line.trim().starts_with('#') {
                return true;
            }
        }
    }
    false
}

fn has_carriage_return_outside_double_quotes(context: &ValidationContext) -> bool {
    if !context.original.contains('\r') {
        return false;
    }
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut escaped = false;
    for character in context.original.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        if character == '\\' && !in_single_quote {
            escaped = true;
            continue;
        }
        if character == '\'' && !in_double_quote {
            in_single_quote = !in_single_quote;
            continue;
        }
        if character == '"' && !in_single_quote {
            in_double_quote = !in_double_quote;
            continue;
        }
        if character == '\r' && !in_double_quote {
            return true;
        }
    }
    false
}

/// `(?<![\s]\\)[\n\r]\s*\S` over the unquoted content.
fn has_command_separating_newline(context: &ValidationContext) -> bool {
    let chars = context.fully_unquoted_pre_strip.chars().collect::<Vec<_>>();
    chars.iter().enumerate().any(|(index, character)| {
        matches!(character, '\n' | '\r')
            && !(index >= 2 && chars[index - 2].is_whitespace() && chars[index - 1] == '\\')
            && chars[index + 1..]
                .iter()
                .any(|character| !character.is_whitespace())
    })
}

fn has_dangerous_patterns(context: &ValidationContext) -> bool {
    has_unescaped_char(&context.unquoted_content, '`')
        || COMMAND_SUBSTITUTIONS
            .iter()
            .any(|pattern| pattern.is_match(&context.unquoted_content))
}

fn has_redirection(context: &ValidationContext) -> bool {
    context.fully_unquoted_content.contains(['<', '>'])
}

fn has_backslash_escaped_whitespace(command: &str) -> bool {
    let chars = command.chars().collect::<Vec<_>>();
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut index = 0;
    while index < chars.len() {
        let character = chars[index];
        if character == '\\' && !in_single_quote {
            if !in_double_quote && matches!(chars.get(index + 1), Some(' ' | '\t')) {
                return true;
            }
            index += 2;
            continue;
        }
        if character == '"' && !in_single_quote {
            in_double_quote = !in_double_quote;
        } else if character == '\'' && !in_double_quote {
            in_single_quote = !in_single_quote;
        }
        index += 1;
    }
    false
}

fn has_backslash_escaped_operator(command: &str) -> bool {
    let chars = command.chars().collect::<Vec<_>>();
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut index = 0;
    while index < chars.len() {
        let character = chars[index];
        if character == '\\' && !in_single_quote {
            if !in_double_quote && matches!(chars.get(index + 1), Some(';' | '|' | '&' | '<' | '>'))
            {
                return true;
            }
            index += 2;
            continue;
        }
        if character == '\'' && !in_double_quote {
            in_single_quote = !in_single_quote;
        } else if character == '"' && !in_single_quote {
            in_double_quote = !in_double_quote;
        }
        index += 1;
    }
    false
}

/// `\S(?<!\$\{)#` on the quote-stripped content, before and after joining continuations.
fn has_mid_word_hash(context: &ValidationContext) -> bool {
    let mid_word_hash = |text: &str| {
        let chars = text.chars().collect::<Vec<_>>();
        chars.iter().enumerate().any(|(index, character)| {
            *character == '#'
                && index >= 1
                && !chars[index - 1].is_whitespace()
                && !(index >= 2 && chars[index - 2] == '$' && chars[index - 1] == '{')
        })
    };
    let joined = join_line_continuations(&context.unquoted_keep_quote_chars);
    mid_word_hash(&context.unquoted_keep_quote_chars) || mid_word_hash(&joined)
}

fn join_line_continuations(text: &str) -> String {
    let mut result = String::new();
    let mut backslashes = 0;
    for character in text.chars() {
        match character {
            '\\' => backslashes += 1,
            '\n' if backslashes % 2 == 1 => {
                result.push_str(&"\\".repeat(backslashes - 1));
                backslashes = 0;
            }
            _ => {
                result.push_str(&"\\".repeat(backslashes));
                backslashes = 0;
                result.push(character);
            }
        }
    }
    result.push_str(&"\\".repeat(backslashes));
    result
}

fn is_escaped_at(chars: &[char], position: usize) -> bool {
    chars[..position]
        .iter()
        .rev()
        .take_while(|character| **character == '\\')
        .count()
        % 2
        == 1
}

fn has_brace_expansion(context: &ValidationContext) -> bool {
    let chars = context.fully_unquoted_pre_strip.chars().collect::<Vec<_>>();
    let unescaped = |target: char| {
        (0..chars.len())
            .filter(|index| chars[*index] == target && !is_escaped_at(&chars, *index))
            .count()
    };
    let open_braces = unescaped('{');
    let close_braces = unescaped('}');
    if open_braces > 0 && close_braces > open_braces {
        return true;
    }
    if open_braces > 0 && QUOTED_SINGLE_BRACE.is_match(context.original) {
        return true;
    }
    for open in 0..chars.len() {
        if chars[open] != '{' || is_escaped_at(&chars, open) {
            continue;
        }
        let mut depth = 1;
        let mut matching_close = None;
        for (index, character) in chars.iter().enumerate().skip(open + 1) {
            if *character == '{' && !is_escaped_at(&chars, index) {
                depth += 1;
            } else if *character == '}' && !is_escaped_at(&chars, index) {
                depth -= 1;
                if depth == 0 {
                    matching_close = Some(index);
                    break;
                }
            }
        }
        let Some(close) = matching_close else {
            continue;
        };
        let mut inner_depth = 0;
        for index in open + 1..close {
            let character = chars[index];
            if character == '{' && !is_escaped_at(&chars, index) {
                inner_depth += 1;
            } else if character == '}' && !is_escaped_at(&chars, index) {
                inner_depth -= 1;
            } else if inner_depth == 0
                && (character == ','
                    || (character == '.' && index + 1 < close && chars[index + 1] == '.'))
            {
                return true;
            }
        }
    }
    false
}

fn has_zsh_dangerous_command(context: &ValidationContext) -> bool {
    let trimmed = context.original.trim();
    let base_command = trimmed
        .split_whitespace()
        .find(|token| {
            !ENV_ASSIGNMENT_TOKEN.is_match(token) && !ZSH_PRECOMMAND_MODIFIERS.contains(token)
        })
        .unwrap_or_default();
    ZSH_DANGEROUS_COMMANDS.contains(&base_command)
        || (base_command == "fc" && FC_EDITOR_FLAG.is_match(trimmed))
}

fn has_malformed_token_injection(context: &ValidationContext) -> bool {
    let Some(tokens) = quote_tokens(context.original) else {
        return false;
    };
    let has_command_separator = tokens
        .iter()
        .any(|token| matches!(token, QuoteToken::Operator(";" | "&&" | "||")));
    has_command_separator && has_malformed_tokens(context.original, &tokens)
}

/// Port of `hasMalformedTokens` (`utils/bash/shellQuote.ts`).
fn has_malformed_tokens(command: &str, tokens: &[QuoteToken]) -> bool {
    let chars = command.chars().collect::<Vec<_>>();
    let mut in_single = false;
    let mut in_double = false;
    let mut double_count = 0;
    let mut single_count = 0;
    let mut index = 0;
    while index < chars.len() {
        let character = chars[index];
        if character == '\\' && !in_single {
            index += 2;
            continue;
        }
        if character == '"' && !in_single {
            double_count += 1;
            in_double = !in_double;
        } else if character == '\'' && !in_double {
            single_count += 1;
            in_single = !in_single;
        }
        index += 1;
    }
    if double_count % 2 != 0 || single_count % 2 != 0 {
        return true;
    }
    tokens.iter().any(|token| {
        let QuoteToken::Word(entry) = token else {
            return false;
        };
        let count = |target: char| {
            entry
                .chars()
                .filter(|character| *character == target)
                .count()
        };
        let unescaped_quotes = |target: char| {
            let entry_chars = entry.chars().collect::<Vec<_>>();
            (0..entry_chars.len())
                .filter(|index| {
                    entry_chars[*index] == target && (*index == 0 || entry_chars[index - 1] != '\\')
                })
                .count()
        };
        count('{') != count('}')
            || count('(') != count(')')
            || count('[') != count(']')
            || unescaped_quotes('"') % 2 != 0
            || unescaped_quotes('\'') % 2 != 0
    })
}

/// Port of `hasShellQuoteSingleQuoteBug` (`utils/bash/shellQuote.ts`).
fn has_shell_quote_single_quote_bug(command: &str) -> bool {
    let chars = command.chars().collect::<Vec<_>>();
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut index = 0;
    while index < chars.len() {
        let character = chars[index];
        if character == '\\' && !in_single_quote {
            index += 2;
            continue;
        }
        if character == '"' && !in_single_quote {
            in_double_quote = !in_double_quote;
        } else if character == '\'' && !in_double_quote {
            in_single_quote = !in_single_quote;
            if !in_single_quote {
                let backslashes = chars[..index]
                    .iter()
                    .rev()
                    .take_while(|character| **character == '\\')
                    .count();
                if backslashes % 2 == 1 || (backslashes > 0 && chars[index + 1..].contains(&'\'')) {
                    return true;
                }
            }
        }
        index += 1;
    }
    false
}

fn strip_comment_lines(command: &str) -> String {
    let lines = command
        .split('\n')
        .filter(|line| {
            let trimmed = line.trim();
            !trimmed.is_empty() && !trimmed.starts_with('#')
        })
        .collect::<Vec<_>>();
    if lines.is_empty() {
        command.to_owned()
    } else {
        lines.join("\n")
    }
}

/// Port of `stripSafeWrappers`.
pub(super) fn strip_safe_wrappers(command: &str) -> String {
    let mut stripped = command.to_owned();
    let mut previous = String::new();
    while stripped != previous {
        previous = stripped.clone();
        stripped = strip_comment_lines(&stripped);
        if let Some(captures) = SAFE_ENV_VAR_PREFIX.captures(&stripped)
            && SAFE_ENV_VARS.contains(&&captures[1])
        {
            stripped = stripped[captures[0].len()..].to_owned();
        }
    }
    previous.clear();
    while stripped != previous {
        previous = stripped.clone();
        stripped = strip_comment_lines(&stripped);
        for wrapper in [
            &*TIMEOUT_WRAPPER,
            &*TIME_WRAPPER,
            &*NICE_WRAPPER,
            &*STDBUF_WRAPPER,
            &*NOHUP_WRAPPER,
        ] {
            stripped = wrapper.replace(&stripped, "").into_owned();
        }
    }
    stripped.trim().to_owned()
}

/// Port of `isNormalizedGitCommand`.
pub(super) fn is_normalized_git_command(command: &str) -> bool {
    if command.starts_with("git ") || command == "git" {
        return true;
    }
    let stripped = strip_safe_wrappers(command);
    match quote_tokens(&stripped) {
        Some(tokens) if !tokens.is_empty() => match &tokens[0] {
            QuoteToken::Word(first) if first == "git" => true,
            QuoteToken::Word(first) if first == "xargs" => tokens
                .iter()
                .any(|token| matches!(token, QuoteToken::Word(word) if word == "git")),
            _ => false,
        },
        _ => GIT_COMMAND_FALLBACK.is_match(&stripped),
    }
}

/// Port of `isNormalizedCdCommand`.
pub(super) fn is_normalized_cd_command(command: &str) -> bool {
    let stripped = strip_safe_wrappers(command);
    match quote_tokens(&stripped) {
        Some(tokens) if !tokens.is_empty() => {
            matches!(&tokens[0], QuoteToken::Word(first) if matches!(first.as_str(), "cd" | "pushd" | "popd"))
        }
        _ => CD_COMMAND_FALLBACK.is_match(&stripped),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_commands_pass_every_validator() {
        for command in [
            "git diff --cached --stat | head -30",
            "cat a.txt | grep foo",
            "ls -la && git status",
            "grep -rn \"x\" src 2>/dev/null",
            "cut -d',' -f1 data.csv",
            "sort -t, -k2 data.csv",
            "sed -n '1,5p' notes.txt",
            "find . -name '*.rs'",
            "echo 'a;b'",
            "grep \"a;b\" file",
            "ls > /dev/null 2>&1",
        ] {
            assert_eq!(
                bash_command_is_safe(command),
                Safety::Passthrough,
                "{command}"
            );
        }
    }

    #[test]
    fn redirections_and_newlines_ask_without_misparsing() {
        for command in ["ls > out.txt", "cat < in.txt", "ls\npwd"] {
            assert_eq!(
                bash_command_is_safe(command),
                Safety::Ask { misparsing: false },
                "{command}"
            );
        }
    }

    #[test]
    fn misparsing_patterns_are_flagged() {
        for command in [
            "echo $(id)",
            "echo `id`",
            "ls \"-\"exec",
            "find . -e\"xec\" x",
            "jq \".a;b\" data.json",
            "cat safe.txt \\; echo x",
            "echo\\ test",
            "echo a#b",
            "git diff {a,b}",
            "git diff {1..3}",
            "zmodload zsh/system",
            "fc -e vi",
            "jq 'system(\"id\")'",
            "jq -f prog.jq",
            "cat /proc/self/environ",
            "echo $IFS",
            "ls '\\' x '\\'",
            "echo \"x\" # it's",
            "cat\u{00A0}file",
            "ls\x07",
            "\tls",
            "-rf /",
            "&& ls",
            "echo \"a\n#b\"",
            "ls $'-la'",
            "grep ''-e x",
            "ls \"\"\"-f\"",
            "echo {a; ls",
            "ls\r",
        ] {
            assert_eq!(
                bash_command_is_safe(command),
                Safety::Ask { misparsing: true },
                "{command:?}"
            );
        }
    }

    #[test]
    fn safe_wrappers_and_environment_prefixes_are_stripped() {
        assert_eq!(strip_safe_wrappers("timeout 5 rm -rf x"), "rm -rf x");
        assert_eq!(strip_safe_wrappers("NO_COLOR=1 nice -n 5 ls"), "ls");
        assert_eq!(strip_safe_wrappers("nohup -- rm -- -x"), "rm -- -x");
        assert_eq!(strip_safe_wrappers("FOO=1 ls"), "FOO=1 ls");
        assert_eq!(strip_safe_wrappers("# note\nls"), "ls");
    }

    #[test]
    fn cd_and_git_detection_sees_through_wrappers_and_quotes() {
        assert!(is_normalized_git_command("'git' status"));
        assert!(is_normalized_git_command("NO_COLOR=1 git status"));
        assert!(is_normalized_git_command("xargs git log"));
        assert!(!is_normalized_git_command("gitk"));
        assert!(is_normalized_cd_command("FORCE_COLOR=1 cd sub"));
        assert!(is_normalized_cd_command("pushd /tmp"));
        assert!(!is_normalized_cd_command("cdrecord"));
    }
}

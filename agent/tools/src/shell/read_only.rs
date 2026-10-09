//! Port of Claude Code's read-only command validation: `isCommandReadOnly`,
//! `isCommandSafeViaFlagParsing` and `checkReadOnlyConstraints`
//! (`tools/BashTool/readOnlyValidation.ts`) with `validateFlags`
//! (`utils/shell/readOnlyCommandValidation.ts`). Windows UNC checks and sandbox-only rules do
//! not apply on macOS/Linux without a sandbox and are omitted.

use std::{path::Path, sync::LazyLock};

use regex::Regex;

use super::{
    command_tables::{
        ARGUMENT_ONLY_XARGS_TARGETS, COMMAND_ALLOWLIST, CommandConfig, FlagArgument,
        READONLY_COMMANDS,
    },
    security::{Safety, bash_command_is_safe, is_normalized_git_command},
    syntax::{QuoteToken, quote_arguments, quote_tokens},
};

const STDERR_TO_STDOUT_SUFFIX: &str = " 2>&1";

fn regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("static read-only pattern")
}

static FLAG_PATTERN: LazyLock<Regex> = LazyLock::new(|| regex(r"^-[a-zA-Z0-9_-]"));
static DIGITS: LazyLock<Regex> = LazyLock::new(|| regex(r"^[0-9]+$"));
static GIT_NUMERIC_SHORTHAND: LazyLock<Regex> = LazyLock::new(|| regex(r"^-[0-9]+$"));
static GIT_REVERSE_SORT: LazyLock<Regex> = LazyLock::new(|| regex(r"^-[a-zA-Z]"));
static GIT_INLINE_CONFIG: LazyLock<Regex> = LazyLock::new(|| regex(r"\s-c[\s=]"));
static GIT_EXEC_PATH: LazyLock<Regex> = LazyLock::new(|| regex(r"\s--exec-path[\s=]"));
static GIT_CONFIG_ENV: LazyLock<Regex> = LazyLock::new(|| regex(r"\s--config-env[\s=]"));

static READONLY_COMMAND_REGEXES: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    let mut regexes = READONLY_COMMANDS
        .iter()
        .map(|command| regex(&format!(r"^{command}(?:\s|$)[^<>()$`|{{}}&;\n\r]*$")))
        .collect::<Vec<_>>();
    regexes.extend(
        [
            r#"^echo(?:\s+(?:'[^']*'|"[^"$<>\n\r]*"|[^|;&`$(){}><#\\!"'\s]+))*(?:\s+2>&1)?\s*$"#,
            r"^claude -h$",
            r"^claude --help$",
            r"^uniq(?:\s+(?:-[a-zA-Z]+|--[a-zA-Z-]+(?:=\S+)?|-[fsw]\s+[0-9]+))*(?:\s|$)\s*$",
            r"^pwd$",
            r"^whoami$",
            r"^node -v$",
            r"^node --version$",
            r"^python --version$",
            r"^python3 --version$",
            r"^history(?:\s+[0-9]+)?\s*$",
            r"^alias$",
            r"^arch(?:\s+(?:--help|-h))?\s*$",
            r"^ip addr$",
            r"^ifconfig(?:\s+[a-zA-Z][a-zA-Z0-9_-]*)?\s*$",
            r#"^cd(?:\s+(?:'[^']*'|"[^"]*"|[^\s;|&`$(){}><#\\]+))?$"#,
            r"^ls(?:\s+[^<>()$`|{}&;\n\r]*)?$",
        ]
        .into_iter()
        .map(regex),
    );
    regexes
});

/// `jq` without the lookahead: dangerous flags are rejected by `JQ_DANGEROUS_ARGUMENTS`.
static JQ_ARGUMENTS: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r#"^jq(?:\s+(?:-[a-zA-Z]+|--[a-zA-Z-]+(?:=\S+)?))*(?:\s+'[^'`]*'|\s+"[^"`]*"|\s+[^-\s'"][^\s]*)+\s*$"#,
    )
});
static JQ_DANGEROUS_ARGUMENTS: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r"^jq\s+.*(?:-f(?-u:\b)|--from-file|--rawfile|--slurpfile|--run-tests|-L(?-u:\b)|--library-path|(?-u:\b)env(?-u:\b)|\$ENV(?-u:\b))",
    )
});
/// `find` without the per-character lookahead: dangerous actions are rejected separately.
static FIND_ARGUMENTS: LazyLock<Regex> =
    LazyLock::new(|| regex(r"^find(?:\s+(?:\\[()]|[^<>()$`|{}&;\n\r\s]|\s)+)?$"));
static FIND_DANGEROUS_ACTIONS: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r"-delete(?-u:\b)|-exec(?-u:\b)|-execdir(?-u:\b)|-ok(?-u:\b)|-okdir(?-u:\b)|-fprint0?(?-u:\b)|-fls(?-u:\b)|-fprintf(?-u:\b)",
    )
});

/// Port of `checkReadOnlyConstraints` for one subcommand.
pub(super) fn is_read_only_subcommand(command: &str, compound_has_cd: bool, cwd: &Path) -> bool {
    if quote_tokens(command).is_none() || bash_command_is_safe(command) != Safety::Passthrough {
        return false;
    }
    let has_git_command = is_normalized_git_command(command.trim());
    if has_git_command && (compound_has_cd || is_bare_git_repository(cwd)) {
        return false;
    }
    is_command_read_only(command)
}

/// Port of `isCurrentDirectoryBareGitRepo` (`utils/git.ts`).
fn is_bare_git_repository(cwd: &Path) -> bool {
    let git_path = cwd.join(".git");
    if let Ok(metadata) = std::fs::metadata(&git_path) {
        if metadata.is_file() {
            return false;
        }
        if metadata.is_dir()
            && std::fs::metadata(git_path.join("HEAD")).is_ok_and(|head| head.is_file())
        {
            return false;
        }
    }
    std::fs::metadata(cwd.join("HEAD")).is_ok_and(|head| head.is_file())
        || std::fs::metadata(cwd.join("objects")).is_ok_and(|objects| objects.is_dir())
        || std::fs::metadata(cwd.join("refs")).is_ok_and(|refs| refs.is_dir())
}

/// Port of `isCommandReadOnly`.
pub(super) fn is_command_read_only(command: &str) -> bool {
    let mut command = command.trim();
    if let Some(stripped) = command.strip_suffix(STDERR_TO_STDOUT_SUFFIX) {
        command = stripped.trim();
    }
    let printf = printf_arguments(command);
    if contains_unquoted_expansion(command, printf == Some(PrintfArguments::TextWithHome)) {
        return false;
    }
    if printf.is_some() {
        return true;
    }
    if has_unproven_symlink_traversal(command) || has_input_derived_file_targets(command) {
        return false;
    }
    if is_command_safe_via_flag_parsing(command) {
        return true;
    }
    if !matches_readonly_regex(command) {
        return false;
    }
    !(command.contains("git")
        && (GIT_INLINE_CONFIG.is_match(command)
            || GIT_EXEC_PATH.is_match(command)
            || GIT_CONFIG_ENV.is_match(command)))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PrintfArguments {
    Literal,
    TextWithHome,
}

fn printf_arguments(command: &str) -> Option<PrintfArguments> {
    let tokens = quote_tokens(command)?;
    let arguments = tokens
        .iter()
        .map(|token| match token {
            QuoteToken::Word(word) => Some(word.as_str()),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    if arguments.first() != Some(&"printf") {
        return None;
    }
    let format_index = if arguments.get(1) == Some(&"--") {
        2
    } else {
        1
    };
    let format = arguments
        .get(format_index)
        .filter(|format| format_index == 2 || !format.starts_with('-'))?;
    let static_format = !format.contains('$');
    let mut argument_is_numeric = Vec::new();
    let mut format = format.chars();
    while let Some(character) = format.next() {
        if character != '%' {
            continue;
        }
        let mut conversion = format.next()?;
        if conversion == '%' {
            continue;
        }
        while conversion.is_ascii_digit()
            || matches!(conversion, '#' | '-' | '+' | ' ' | '.' | '\'' | '*')
        {
            if conversion == '*' {
                argument_is_numeric.push(true);
            }
            conversion = format.next()?;
        }
        match conversion {
            'b' | 's' | 'c' | 'q' => argument_is_numeric.push(false),
            'd' | 'i' | 'o' | 'u' | 'x' | 'X' | 'a' | 'A' | 'e' | 'E' | 'f' | 'F' | 'g' | 'G' => {
                argument_is_numeric.push(true)
            }
            _ => return None,
        }
    }
    if !argument_is_numeric.iter().any(|numeric| *numeric) {
        return Some(if static_format {
            PrintfArguments::TextWithHome
        } else {
            PrintfArguments::Literal
        });
    }
    // zsh evaluates numeric operands as arithmetic, including variable references.
    arguments[format_index + 1..]
        .iter()
        .enumerate()
        .all(|(index, value)| {
            let unsigned = value.strip_prefix(['+', '-']).unwrap_or(value);
            let literal_hex = unsigned
                .strip_prefix("0x")
                .or_else(|| unsigned.strip_prefix("0X"))
                .is_some_and(|digits| {
                    !digits.is_empty() && digits.chars().all(|digit| digit.is_ascii_hexdigit())
                });
            !argument_is_numeric[index % argument_is_numeric.len()]
                || value.parse::<f64>().is_ok_and(f64::is_finite)
                || literal_hex
                || value.starts_with(['\'', '"']) && value.len() > 1
        })
        .then_some(PrintfArguments::Literal)
}

const SYMLINK_FOLLOWING_LONG_FLAGS: &[&str] = &[
    "--follow",
    "--follow-symlinks",
    "--dereference",
    "--dereference-recursive",
];

/// Per program, the short flags that make it follow symbolic links while walking directories.
const SYMLINK_FOLLOWING_SHORT_FLAGS: &[(&str, &str)] = &[
    ("rg", "L"),
    ("fd", "L"),
    ("find", "LH"),
    ("du", "LH"),
    ("grep", "RS"),
    ("ls", "L"),
    ("ag", "f"),
    ("tree", "l"),
];

fn has_unproven_symlink_traversal(command: &str) -> bool {
    let Some(arguments) = quote_arguments(command) else {
        return true;
    };
    let Some((program, flags)) = arguments.split_first() else {
        return false;
    };
    let following_short_flags = SYMLINK_FOLLOWING_SHORT_FLAGS
        .iter()
        .find(|(name, _)| name == program)
        .map_or("", |(_, flags)| flags);
    flags
        .iter()
        .take_while(|flag| flag.as_str() != "--")
        .any(|flag| {
            SYMLINK_FOLLOWING_LONG_FLAGS.contains(&flag.as_str())
                || flag.starts_with('-')
                    && !flag.starts_with("--")
                    && flag[1..]
                        .chars()
                        .any(|character| following_short_flags.contains(character))
        })
}

/// `wc` and `du` are read-only with any flags, but `--files0-from` takes their targets from a
/// file, so the paths they read cannot be checked.
fn has_input_derived_file_targets(command: &str) -> bool {
    let Some(arguments) = quote_arguments(command) else {
        return true;
    };
    matches!(arguments.first().map(String::as_str), Some("wc" | "du"))
        && arguments
            .iter()
            .skip(1)
            .take_while(|flag| flag.as_str() != "--")
            .any(|flag| flag.split('=').next() == Some("--files0-from"))
}

fn matches_readonly_regex(command: &str) -> bool {
    READONLY_COMMAND_REGEXES
        .iter()
        .any(|pattern| pattern.is_match(command))
        || (JQ_ARGUMENTS.is_match(command) && !JQ_DANGEROUS_ARGUMENTS.is_match(command))
        || (FIND_ARGUMENTS.is_match(command)
            && !FIND_DANGEROUS_ACTIONS.is_match(&command["find".len()..]))
}

/// Port of `containsUnquotedExpansion`.
fn contains_unquoted_expansion(command: &str, allow_home: bool) -> bool {
    let chars = command.chars().collect::<Vec<_>>();
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut escaped = false;
    for (index, character) in chars.iter().copied().enumerate() {
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
        if in_single_quote {
            continue;
        }
        if character == '$'
            && chars
                .get(index + 1)
                .is_some_and(|next| next.is_ascii_alphanumeric() || "_@*#?!$-({".contains(*next))
        {
            let is_home_expansion = allow_home
                && in_double_quote
                && chars[index + 1..].starts_with(&['H', 'O', 'M', 'E'])
                && chars
                    .get(index + 5)
                    .is_none_or(|next| !next.is_ascii_alphanumeric() && *next != '_');
            if !is_home_expansion {
                return true;
            }
        }
        if character == '`' {
            return true;
        }
        if !in_double_quote && matches!(character, '?' | '*' | '[' | ']') {
            return true;
        }
    }
    false
}

/// Port of `isCommandSafeViaFlagParsing`.
fn is_command_safe_via_flag_parsing(command: &str) -> bool {
    let Some(tokens) = quote_tokens(command) else {
        return false;
    };
    let Some(tokens) = tokens
        .into_iter()
        .map(|token| match token {
            QuoteToken::Word(word) | QuoteToken::Glob(word) => Some(word),
            QuoteToken::Operator(_) | QuoteToken::Comment => None,
        })
        .collect::<Option<Vec<_>>>()
    else {
        return false;
    };
    if tokens.is_empty() {
        return false;
    }
    let Some(config) = COMMAND_ALLOWLIST.iter().find(|config| {
        let words = config.command.split(' ').collect::<Vec<_>>();
        tokens.len() >= words.len() && words.iter().zip(&tokens).all(|(word, token)| word == token)
    }) else {
        return false;
    };
    let command_token_count = config.command.split(' ').count();
    if tokens[0] == "git"
        && tokens.get(1).is_some_and(|token| token == "ls-remote")
        && tokens[2..].iter().any(|token| {
            !token.starts_with('-')
                && !token.is_empty()
                && (token.contains("://") || token.contains(['@', ':', '$']))
        })
    {
        return false;
    }
    if tokens[command_token_count..].iter().any(|token| {
        token.contains('$')
            || (token.contains('{') && (token.contains(',') || token.contains("..")))
    }) {
        return false;
    }
    if !validate_flags(&tokens, command_token_count, config) {
        return false;
    }
    match config.regex {
        Some(pattern) if !pattern.is_match(command) => return false,
        None if command.contains('`') => return false,
        None if matches!(tokens[0].as_str(), "rg" | "grep") && command.contains(['\n', '\r']) => {
            return false;
        }
        _ => {}
    }
    config
        .is_dangerous
        .is_none_or(|is_dangerous| !is_dangerous(command, &tokens[command_token_count..]))
}

fn validate_flag_argument(value: &str, argument: FlagArgument) -> bool {
    match argument {
        FlagArgument::NoValue => false,
        FlagArgument::Number => DIGITS.is_match(value),
        FlagArgument::Text => true,
        FlagArgument::Char => value.chars().count() == 1,
        FlagArgument::Braces => value == "{}",
        FlagArgument::Eof => value == "EOF",
    }
}

fn looks_like_flag(token: &str) -> bool {
    token.starts_with('-') && token.len() > 1 && FLAG_PATTERN.is_match(token)
}

/// Port of `validateFlags`.
fn validate_flags(tokens: &[String], start: usize, config: &CommandConfig) -> bool {
    let command_name = tokens[0].as_str();
    let mut index = start;
    while index < tokens.len() {
        let mut token = tokens[index].as_str();
        if token.is_empty() {
            index += 1;
            continue;
        }
        if command_name == "xargs" && (!token.starts_with('-') || token == "--") {
            if token == "--" && index + 1 < tokens.len() {
                index += 1;
                token = &tokens[index];
            }
            return ARGUMENT_ONLY_XARGS_TARGETS.contains(&token);
        }
        if token == "--" {
            index += 1;
            if config.respects_double_dash {
                break;
            }
            continue;
        }
        if !looks_like_flag(token) {
            index += 1;
            continue;
        }
        let has_equals = token.contains('=');
        let (flag, inline_value) = token.split_once('=').unwrap_or((token, ""));
        if has_equals && !flag.starts_with("--") {
            return false;
        }
        if flag.is_empty() {
            return false;
        }
        let Some(argument) = config.flag(flag) else {
            if command_name == "git" && GIT_NUMERIC_SHORTHAND.is_match(flag) {
                index += 1;
                continue;
            }
            if matches!(command_name, "grep" | "rg") && !flag.starts_with("--") && flag.len() > 2 {
                let (short_flag, attached) = flag.split_at(2);
                if let Some(short_argument) = config.flag(short_flag)
                    && DIGITS.is_match(attached)
                    && matches!(short_argument, FlagArgument::Number | FlagArgument::Text)
                {
                    if !validate_flag_argument(attached, short_argument) {
                        return false;
                    }
                    index += 1;
                    continue;
                }
            }
            if !flag.starts_with("--") && flag.len() > 2 {
                let all_bundled_flags_take_no_value = flag[1..].chars().all(|single| {
                    config.flag(&format!("-{single}")) == Some(FlagArgument::NoValue)
                });
                if !all_bundled_flags_take_no_value {
                    return false;
                }
                index += 1;
                continue;
            }
            return false;
        };
        if argument == FlagArgument::NoValue {
            if has_equals {
                return false;
            }
            index += 1;
            continue;
        }
        let value = if has_equals {
            index += 1;
            inline_value
        } else {
            match tokens.get(index + 1) {
                Some(next) if !looks_like_flag(next) => {
                    index += 2;
                    next.as_str()
                }
                _ => return false,
            }
        };
        if argument == FlagArgument::Text
            && value.starts_with('-')
            && !(flag == "--sort" && command_name == "git" && GIT_REVERSE_SORT.is_match(value))
        {
            return false;
        }
        if !validate_flag_argument(value, argument) {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn printf_outputs_are_distinct_from_assignments_and_numeric_expression_evaluation() {
        for command in [
            "printf hello",
            "printf '%s\\n' plain",
            "printf '%%n'",
            "printf -- '-%s' plain",
            "printf '%s %04d' label 42",
            "printf '%*.*f' 8 2 1.5",
            "printf '%x' 0xff",
            "printf '%s' '$(touch marker)'",
            "printf '%s %d' one 1 two 2",
            "printf '%s' \"$HOME\"",
            "printf '%s' \"$HOME/test\"",
            "printf '$HOME'",
            "printf '%s' --follow -v",
        ] {
            assert!(is_command_read_only(command), "{command}");
        }
        for command in [
            "printf -v value '%s' plain",
            "printf '%n' value",
            "printf '%%%n' value",
            "printf '%2n' value",
            "printf '%d' 'array[$(touch marker)]'",
            "printf '%*s' 'array[$(touch marker)]' plain",
            "printf '%d' numeric_variable",
            "printf '%d' '1+2'",
            "printf '%s' \"$(touch marker)\"",
            "printf hello > marker",
            "printf \"$HOME\"",
            "printf '%s' \"${HOME}\"",
            "printf '%s' \"$HOME_KEY\"",
            "printf '%s' \"$PWD\"",
            "printf '%s' \"$ANTHROPIC_API_KEY\"",
            "printf '%d' \"$HOME\"",
        ] {
            assert!(!is_command_read_only(command), "{command}");
        }
    }

    #[test]
    fn allowlisted_command_forms_are_read_only() {
        for command in [
            "git diff --cached --stat",
            "git diff HEAD~1 -- src/main.rs",
            "git log --oneline -n 5",
            "git log -20 --format=%h",
            "git show HEAD:README.md",
            "git status --short",
            "git blame -L 1,20 src/lib.rs",
            "git ls-files -z",
            "git rev-parse --show-toplevel",
            "git branch -a",
            "git tag -l",
            "git remote -v",
            "git remote show origin",
            "git config --get user.name",
            "git stash list",
            "git for-each-ref --sort=-committerdate refs/heads",
            "git ls-remote --tags origin",
            "cat src/main.rs",
            "head -30",
            "tail -n 50 log.txt",
            "wc -l src/main.rs",
            "grep -rn \"x\" src",
            "grep -A5 foo notes.txt",
            "rg -n pattern src",
            "ls -la",
            "ls",
            "pwd",
            "whoami",
            "find . -name '*.rs' -type f",
            "find . \\( -name a -o -name b \\)",
            "sed -n '1,5p' notes.txt",
            "jq '.name' package.json",
            "sort -u names.txt",
            "uniq -c",
            "cut -d, -f1 data.csv",
            "diff a.txt b.txt",
            "stat Cargo.toml",
            "echo hello",
            "echo 'a b' \"c\"",
            "cd src",
            "xargs -I {} echo {}",
            "date +%Y-%m-%d",
            "tree -L 2",
            "ps aux",
            "file -b notes.txt",
            "sha256sum Cargo.lock",
            "base64 -d payload.txt",
            "docker ps",
            "hostname -s",
            "fd -e rs",
            "cat notes.txt 2>&1",
            "git stash show stash@{0}",
        ] {
            assert!(is_command_read_only(command), "{command}");
        }
    }

    #[test]
    fn dangerous_command_forms_are_not_read_only() {
        for command in [
            "git diff --output=/tmp/pwned",
            "git -c core.fsmonitor=x status",
            "git status --exec-path=/tmp",
            "git log --config-env=x=y",
            "git branch new-branch",
            "git tag v1",
            "git reflog expire --all",
            "git remote add origin x",
            "git ls-remote https://evil.example",
            "git ls-remote git@host:repo",
            "git diff {@'{'0},--output=/tmp/pwned}",
            "git diff -S -- --output=x",
            "git push",
            "grep 'foo$' notes.txt",
            "rg . --pre=bash notes.txt",
            "ls *.rs",
            "cat ?",
            "echo $HOME",
            "echo \"$HOME\"",
            "uniq --skip-chars=0$_",
            "find . -exec rm {} ;",
            "find . -delete",
            "find . -fprintf out x",
            "find . -execdir id ;",
            "sed -i 's/a/b/' notes.txt",
            "sed 's/a/b/w out'",
            "jq -f prog.jq data.json",
            "jq 'env' data.json",
            "jq '$ENV' data.json",
            "tree -R -H . -L 2",
            "tree -o out.txt",
            "xargs sh -c id",
            "xargs -rI echo sh -c id",
            "xargs -E= EOF echo foo",
            "xargs -i echo",
            "ps axe",
            "date 0101000026",
            "hostname evil",
            "tput reset",
            "lsof +m/tmp/x",
            "pyright --watch",
            "pyright -- --createstub os",
            "sort -o out.txt names.txt",
            "fd -x rm",
            "node -v --run task",
            "cat a > b",
            "echo `id`",
            "man -P id ls",
            "python3 script.py",
            "rm -rf build",
            "env",
            "printenv",
        ] {
            assert!(!is_command_read_only(command), "{command}");
        }
    }

    #[test]
    fn bundled_flags_must_all_take_no_value() {
        assert!(is_command_read_only("grep -rni foo src"));
        assert!(!is_command_read_only("grep -rne foo src"));
        assert!(!is_command_read_only("sort -rk2 names.txt"));
        for command in ["xargs grep foo", "xargs head", "xargs wc -l", "xargs tail"] {
            assert!(!is_command_read_only(command), "{command}");
        }
    }

    #[test]
    fn subcommand_constraints_reject_git_after_cd_and_bare_repositories() {
        let directory = tempfile::tempdir().unwrap();
        assert!(is_read_only_subcommand(
            "git status",
            false,
            directory.path()
        ));
        assert!(!is_read_only_subcommand(
            "git status",
            true,
            directory.path()
        ));
        std::fs::create_dir(directory.path().join("objects")).unwrap();
        assert!(!is_read_only_subcommand(
            "git status",
            false,
            directory.path()
        ));
        assert!(is_read_only_subcommand("ls", true, directory.path()));
        assert!(!is_read_only_subcommand(
            "ls a\\;b",
            false,
            directory.path()
        ));
    }
}

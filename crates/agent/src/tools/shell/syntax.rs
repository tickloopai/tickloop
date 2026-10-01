//! Static shell syntax used by the permission classifier.
//!
//! `parse` accepts only plain command lists: words joined by `|`, `&&`, `||`, `;` or newlines,
//! plus stdout/stderr redirections to static targets. Everything Claude Code's
//! `utils/bash/ast.ts` treats as too complex (substitutions, expansions, subshells, groups,
//! control flow, heredocs, here-strings, background jobs, comments) is rejected.
//!
//! `quote_tokens` mirrors the `shell-quote` tokenizer that Claude Code's legacy validators
//! (`utils/bash/shellQuote.ts`) run on subcommand text.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Separator {
    AndIf,
    OrIf,
    Sequence,
    Pipe,
}

impl Separator {
    fn text(self) -> &'static str {
        match self {
            Self::AndIf => " && ",
            Self::OrIf => " || ",
            Self::Sequence => " ; ",
            Self::Pipe => " | ",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct OutputRedirect {
    pub(super) target: String,
}

#[derive(Clone, Debug, Default)]
pub(super) struct SimpleCommand {
    /// Words re-joined the way Claude Code's `splitCommand_DEPRECATED` renders a subcommand;
    /// see `render_unquoted_escape` for the quoting quirks it preserves.
    pub(super) text: String,
    pub(super) redirects: Vec<OutputRedirect>,
    pub(super) separator: Option<Separator>,
}

pub(super) fn join_commands(commands: &[SimpleCommand]) -> String {
    let mut text = String::new();
    for (index, command) in commands.iter().enumerate() {
        text.push_str(&command.text);
        if index + 1 < commands.len()
            && let Some(separator) = command.separator
        {
            text.push_str(separator.text());
        }
    }
    text
}

const SPECIAL_PARAMETER_START: &str = "_@*#?!$-{([";
const REDIRECT_FILE_DESCRIPTORS: [&str; 3] = ["0", "1", "2"];

struct WordBuilder {
    text: String,
    is_empty: bool,
    is_digits: bool,
}

impl WordBuilder {
    fn new() -> Self {
        Self {
            text: String::new(),
            is_empty: true,
            is_digits: true,
        }
    }

    fn push_raw(&mut self, text: &str, is_digit: bool) {
        self.text.push_str(text);
        self.is_empty = false;
        self.is_digits &= is_digit;
    }
}

struct Parser {
    chars: Vec<char>,
    position: usize,
    commands: Vec<SimpleCommand>,
    current: SimpleCommand,
    current_has_words: bool,
    current_has_redirection: bool,
    current_ends_with_duplication: bool,
}

/// Parses `command` into simple commands, or `None` when the command is not a plain list.
pub(super) fn parse(command: &str) -> Option<Vec<SimpleCommand>> {
    let mut parser = Parser {
        chars: command.chars().collect(),
        position: 0,
        commands: Vec::new(),
        current: SimpleCommand::default(),
        current_has_words: false,
        current_has_redirection: false,
        current_ends_with_duplication: false,
    };
    parser.run()?;
    Some(parser.commands)
}

impl Parser {
    fn peek(&self, offset: usize) -> Option<char> {
        self.chars.get(self.position + offset).copied()
    }

    fn run(&mut self) -> Option<()> {
        loop {
            self.skip_blanks();
            let Some(character) = self.peek(0) else {
                break;
            };
            match character {
                '\n' => {
                    // Claude Code's newline placeholder survives as a bogus subcommand unless the
                    // newline directly ends a command.
                    self.position += 1;
                    self.finish_command(Separator::Sequence)?;
                }
                ';' => {
                    if self.peek(1) == Some(';') {
                        return None;
                    }
                    self.position += 1;
                    self.finish_command(Separator::Sequence)?;
                }
                '&' => {
                    if self.peek(1) != Some('&') {
                        return None;
                    }
                    self.position += 2;
                    self.finish_command(Separator::AndIf)?;
                }
                '|' => {
                    let separator = match self.peek(1) {
                        Some('|') => Separator::OrIf,
                        Some('&') => return None,
                        _ => Separator::Pipe,
                    };
                    self.position += if separator == Separator::Pipe { 1 } else { 2 };
                    self.finish_command(separator)?;
                }
                '(' | ')' | '<' | '`' => return None,
                '>' => self.redirect(None)?,
                _ => {
                    let word = self.word()?;
                    if word.is_digits && self.peek(0) == Some('>') {
                        self.redirect(Some(word.text))?;
                    } else if self.current_has_redirection {
                        // Claude Code folds words after a redirect into its target.
                        return None;
                    } else {
                        if !self.current.text.is_empty() {
                            self.current.text.push(' ');
                        }
                        self.current.text.push_str(&word.text);
                        self.current_has_words = true;
                    }
                }
            }
        }
        if self.current_has_words {
            self.commands.push(std::mem::take(&mut self.current));
        } else if let Some(last) = self.commands.last_mut() {
            if last.separator != Some(Separator::Sequence) {
                return None;
            }
            last.separator = None;
        }
        (!self.commands.is_empty()).then_some(())
    }

    fn finish_command(&mut self, separator: Separator) -> Option<()> {
        if !self.current_has_words {
            return None;
        }
        let mut command = std::mem::take(&mut self.current);
        command.separator = Some(separator);
        self.commands.push(command);
        self.current_has_words = false;
        self.current_has_redirection = false;
        self.current_ends_with_duplication = false;
        Some(())
    }

    fn skip_blanks(&mut self) {
        while let Some(character) = self.peek(0) {
            if character == ' ' || character == '\t' {
                self.position += 1;
            } else if character == '\\' && self.peek(1) == Some('\n') {
                self.position += 2;
            } else {
                break;
            }
        }
    }

    fn redirect(&mut self, file_descriptor: Option<String>) -> Option<()> {
        if file_descriptor
            .as_deref()
            .is_some_and(|descriptor| !REDIRECT_FILE_DESCRIPTORS.contains(&descriptor))
        {
            return None;
        }
        // Claude Code folds a descriptor prefix that follows `N>&M` into the duplication target.
        if !self.current_has_words
            || (file_descriptor.is_some() && self.current_ends_with_duplication)
        {
            return None;
        }
        self.current_has_redirection = true;
        self.position += 1;
        let is_duplication = match self.peek(0) {
            Some('>') => {
                self.position += 1;
                false
            }
            Some('&') => {
                self.position += 1;
                true
            }
            Some('|' | '(') => return None,
            _ => false,
        };
        self.skip_blanks();
        let target = self.word()?;
        self.current_ends_with_duplication = is_duplication;
        if is_duplication {
            return REDIRECT_FILE_DESCRIPTORS
                .contains(&target.text.as_str())
                .then_some(());
        }
        if !is_static_redirect_target(&target.text) {
            return None;
        }
        self.current.redirects.push(OutputRedirect {
            target: target.text,
        });
        Some(())
    }

    fn word(&mut self) -> Option<WordBuilder> {
        let mut word = WordBuilder::new();
        while let Some(character) = self.peek(0) {
            match character {
                ' ' | '\t' | '\n' | ';' | '&' | '|' | '<' | '>' | '(' | ')' => break,
                '`' => return None,
                '\\' => {
                    let escaped = self.peek(1)?;
                    if matches!(escaped, ' ' | '\t' | '\n') {
                        return None;
                    }
                    self.position += 2;
                    word.push_raw(&render_unquoted_escape(escaped), false);
                }
                '\'' => {
                    self.position += 1;
                    let mut text = String::from('\'');
                    loop {
                        match self.peek(0)? {
                            '\'' => break,
                            '"' => text.push_str("\"\""),
                            quoted => text.push(quoted),
                        }
                        self.position += 1;
                    }
                    self.position += 1;
                    text.push('\'');
                    word.push_raw(&text, false);
                }
                '"' => {
                    self.position += 1;
                    let mut text = String::from('"');
                    loop {
                        let quoted = self.peek(0)?;
                        match quoted {
                            '"' => break,
                            '\\' => {
                                let escaped = self.peek(1)?;
                                self.position += 2;
                                text.push_str(&render_double_quoted_escape(escaped));
                            }
                            '`' => return None,
                            '$' if self.peek(1).is_some_and(starts_expansion) => return None,
                            '\'' => {
                                self.position += 1;
                                text.push_str("''");
                            }
                            _ => {
                                self.position += 1;
                                text.push(quoted);
                            }
                        }
                    }
                    self.position += 1;
                    text.push('"');
                    word.push_raw(&text, false);
                }
                '$' if self
                    .peek(1)
                    .is_some_and(|next| starts_expansion(next) || next == '\'' || next == '"') =>
                {
                    return None;
                }
                '#' if word.is_empty => return None,
                _ => {
                    self.position += 1;
                    word.push_raw(&character.to_string(), character.is_ascii_digit());
                }
            }
        }
        (!word.is_empty).then_some(word)
    }
}

/// Claude Code renders subcommands through quote placeholders, so quote characters that are
/// escaped or nested in the other quote kind come back doubled (`\"` becomes `""`) while other
/// unquoted escapes lose their backslash.
fn render_unquoted_escape(escaped: char) -> String {
    match escaped {
        '(' | ')' => format!("\\{escaped}"),
        '"' | '\'' => format!("{escaped}{escaped}"),
        _ => escaped.to_string(),
    }
}

fn render_double_quoted_escape(escaped: char) -> String {
    match escaped {
        '"' => "\"\"".to_owned(),
        '\\' | '$' => escaped.to_string(),
        _ => format!("\\{escaped}"),
    }
}

fn starts_expansion(character: char) -> bool {
    character.is_ascii_alphanumeric() || SPECIAL_PARAMETER_START.contains(character)
}

/// Port of `isStaticRedirectTarget` (`utils/bash/commands.ts`).
fn is_static_redirect_target(target: &str) -> bool {
    !target.is_empty()
        && !target.chars().any(|character| {
            character.is_whitespace()
                || matches!(
                    character,
                    '\'' | '"' | '$' | '`' | '*' | '?' | '[' | '{' | '~' | '(' | '<' | '\\'
                )
        })
        && !target.starts_with(['#', '!', '=', '&'])
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum QuoteToken {
    Word(String),
    Glob(String),
    Operator(&'static str),
    Comment,
}

impl QuoteToken {
    /// Strings and glob patterns, the way Claude Code maps `shell-quote` entries to argv.
    pub(super) fn as_argument(&self) -> Option<&str> {
        match self {
            Self::Word(word) | Self::Glob(word) => Some(word),
            Self::Operator(_) | Self::Comment => None,
        }
    }
}

const QUOTE_OPERATORS: [&str; 14] = [
    "||", "&&", ";;", "|&", "<(", ">>", ">&", "&", ";", "(", ")", "|", "<", ">",
];

/// Tokenizes like `shell-quote`'s `parse` with variables preserved as literal text.
/// Returns `None` for unterminated quotes, which callers treat as a parse failure.
pub(super) fn quote_tokens(text: &str) -> Option<Vec<QuoteToken>> {
    let chars = text.chars().collect::<Vec<_>>();
    let mut tokens = Vec::new();
    let mut position = 0;
    'tokens: while position < chars.len() {
        if chars[position].is_whitespace() {
            position += 1;
            continue;
        }
        let rest = chars[position..].iter().collect::<String>();
        if let Some(operator) = QUOTE_OPERATORS
            .iter()
            .find(|operator| rest.starts_with(**operator))
        {
            tokens.push(QuoteToken::Operator(operator));
            position += operator.chars().count();
            continue;
        }
        let mut word = String::new();
        let mut is_glob = false;
        while position < chars.len() {
            let character = chars[position];
            if character.is_whitespace() || "|&;()<>".contains(character) {
                break;
            }
            match character {
                '\\' => {
                    if let Some(escaped) = chars.get(position + 1) {
                        is_glob |= matches!(escaped, '*' | '?');
                        word.push(*escaped);
                    }
                    position += 2;
                }
                '\'' => {
                    let end = chars[position + 1..]
                        .iter()
                        .position(|character| *character == '\'')?;
                    word.extend(&chars[position + 1..position + 1 + end]);
                    position += end + 2;
                }
                '"' => {
                    position += 1;
                    loop {
                        let character = *chars.get(position)?;
                        match character {
                            '"' => break,
                            '\\' => {
                                let escaped = *chars.get(position + 1)?;
                                if !matches!(escaped, '"' | '\\' | '$') {
                                    word.push('\\');
                                }
                                word.push(escaped);
                                position += 2;
                            }
                            _ => {
                                word.push(character);
                                position += 1;
                            }
                        }
                    }
                    position += 1;
                }
                '#' => {
                    if !word.is_empty() {
                        tokens.push(QuoteToken::Word(word));
                    }
                    tokens.push(QuoteToken::Comment);
                    break 'tokens;
                }
                _ => {
                    is_glob |= matches!(character, '*' | '?');
                    word.push(character);
                    position += 1;
                }
            }
        }
        tokens.push(if is_glob {
            QuoteToken::Glob(word)
        } else {
            QuoteToken::Word(word)
        });
    }
    Some(tokens)
}

/// Words and glob patterns of `text`, dropping operators and comments.
pub(super) fn quote_arguments(text: &str) -> Option<Vec<String>> {
    Some(
        quote_tokens(text)?
            .iter()
            .filter_map(QuoteToken::as_argument)
            .map(str::to_owned)
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(command: &str) -> Option<Vec<String>> {
        parse(command).map(|commands| commands.into_iter().map(|command| command.text).collect())
    }

    #[test]
    fn plain_command_lists_split_into_simple_commands() {
        assert_eq!(texts("ls -la").unwrap(), ["ls -la"]);
        assert_eq!(
            texts("git diff --cached --stat | head -30").unwrap(),
            ["git diff --cached --stat", "head -30"]
        );
        assert_eq!(
            texts("ls -la && git status; pwd || true").unwrap(),
            ["ls -la", "git status", "pwd", "true"]
        );
        assert_eq!(texts("ls\npwd\n").unwrap(), ["ls", "pwd"]);
        assert_eq!(texts("ls >/dev/null\npwd").unwrap(), ["ls", "pwd"]);
        assert_eq!(texts("ls;").unwrap(), ["ls"]);
        assert_eq!(
            texts("grep -rn \"x y\" src 2>/dev/null").unwrap(),
            ["grep -rn \"x y\" src"]
        );
        assert_eq!(texts("cat 'a b' 2>&1").unwrap(), ["cat 'a b'"]);
        assert_eq!(texts("ls > /dev/null 2>&1").unwrap(), ["ls"]);
        assert_eq!(
            texts("echo \"a\\\"b\" x\\\"y \\$z").unwrap(),
            ["echo \"a\"\"b\" x\"\"y $z"]
        );
        assert_eq!(
            texts("git log \\\n  --oneline").unwrap(),
            ["git log --oneline"]
        );
        assert_eq!(
            texts("find . \\( -name x \\)").unwrap(),
            ["find . \\( -name x \\)"]
        );
    }

    #[test]
    fn output_redirects_keep_static_targets() {
        let commands = parse("ls > out.txt && echo hi >> log/a.txt 2>&1").unwrap();
        assert_eq!(commands[0].redirects[0].target, "out.txt");
        assert_eq!(commands[1].redirects[0].target, "log/a.txt");
        assert_eq!(commands[1].text, "echo hi");
    }

    #[test]
    fn complex_shell_constructs_are_rejected() {
        for command in [
            "echo $(pwd)",
            "echo `pwd`",
            "echo \"$(pwd)\"",
            "echo $HOME",
            "echo \"${HOME}\"",
            "echo $'\\x41'",
            "cat <(ls)",
            "ls >(cat)",
            "(ls)",
            "ls &",
            "sleep 1 & ls",
            "cat < file",
            "cat <<EOF\nx\nEOF",
            "cat <<< text",
            "ls # comment",
            "ls |& cat",
            "ls >| out",
            "ls > $OUT",
            "ls > 'out file'",
            "ls > ~/out",
            "ls 3> out",
            "ls >&file",
            "ls &&",
            "ls\n\npwd",
            "\nls",
            "ls &&\npwd",
            "ls;\n",
            "ls 2>/dev/null src",
            "ls > out.txt extra",
            "> out.txt ls",
            "2>&1 ls",
            "ls 2>&1 2>/dev/null",
            "&& ls",
            "ls | | cat",
            "echo 'unterminated",
            "cat\\ test",
            "a\\\nb",
            "f() { ls; }",
        ] {
            assert!(parse(command).is_none(), "accepted {command:?}");
        }
    }

    #[test]
    fn quote_tokens_follow_shell_quote_word_values() {
        assert_eq!(
            quote_tokens("grep -e 'a b' \"c\\\"d\" e\\ f *.rs # x").unwrap(),
            [
                QuoteToken::Word("grep".into()),
                QuoteToken::Word("-e".into()),
                QuoteToken::Word("a b".into()),
                QuoteToken::Word("c\"d".into()),
                QuoteToken::Word("e f".into()),
                QuoteToken::Glob("*.rs".into()),
                QuoteToken::Comment,
            ]
        );
        assert_eq!(
            quote_tokens("a && b | c").unwrap()[1],
            QuoteToken::Operator("&&")
        );
    }
}

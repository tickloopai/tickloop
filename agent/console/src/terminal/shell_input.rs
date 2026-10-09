use std::collections::HashSet;
use std::fs;
use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum InputKind {
    #[default]
    Prompt,
    Shell,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ShellEmptyControl {
    Escape,
    Backspace,
    Clear,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ShellCompletionKey {
    Previous,
    Next,
    Complete,
    Cancel,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ShellCompletionKind {
    History,
    Path,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ShellExecution {
    Foreground,
    Background,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct UserShellSubmission {
    command: String,
    execution: ShellExecution,
}

impl UserShellSubmission {
    pub(super) fn command(&self) -> &str {
        &self.command
    }

    pub(super) fn execution(&self) -> ShellExecution {
        self.execution
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ShellHistoryCandidate {
    pub command: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ShellPathKind {
    File,
    Directory,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ShellPathCandidate {
    pub display_path: String,
    pub replacement: String,
    pub path: PathBuf,
    pub kind: ShellPathKind,
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum ShellInputAction {
    Ignored,
    Consumed,
    EnterShell { command: String, cursor: usize },
    ExitShell,
    ReplaceCommand { command: String, cursor: usize },
    Submit(UserShellSubmission),
}

#[derive(Debug, Default)]
pub(super) struct ShellInputState {
    kind: InputKind,
    command: String,
    cursor: usize,
    history_candidates: Vec<ShellHistoryCandidate>,
    path_candidates: Vec<ShellPathCandidate>,
    path_replacement_range: Option<Range<usize>>,
    active_completion: Option<ShellCompletionKind>,
    selected: Option<usize>,
}

impl ShellInputState {
    pub(super) fn kind(&self) -> InputKind {
        self.kind
    }

    pub(super) fn command(&self) -> &str {
        &self.command
    }

    pub(super) fn cursor(&self) -> usize {
        self.cursor
    }

    pub(super) fn try_tty_character(
        &mut self,
        prompt: &str,
        cursor: usize,
        character: char,
    ) -> ShellInputAction {
        if self.kind != InputKind::Prompt || character != '!' || !prompt.is_empty() || cursor != 0 {
            return ShellInputAction::Ignored;
        }

        self.enter_shell(String::new())
    }

    pub(super) fn try_tty_paste(
        &mut self,
        prompt: &str,
        cursor: usize,
        paste: &str,
    ) -> ShellInputAction {
        if self.kind != InputKind::Prompt
            || !prompt.is_empty()
            || cursor != 0
            || !paste.starts_with('!')
            || !is_safe_command_text(paste)
        {
            return ShellInputAction::Ignored;
        }

        self.enter_shell(paste[1..].to_owned())
    }

    pub(super) fn update_command(&mut self, command: &str, cursor: usize) {
        if self.kind != InputKind::Shell {
            return;
        }
        self.command.clear();
        self.command.push_str(command);
        self.cursor = floor_char_boundary(command, cursor.min(command.len()));
        self.dismiss_completion();
    }

    pub(super) fn handle_empty_control(&mut self, control: ShellEmptyControl) -> ShellInputAction {
        if self.kind != InputKind::Shell {
            return ShellInputAction::Ignored;
        }

        if self.command.is_empty() {
            self.leave_shell();
            return ShellInputAction::ExitShell;
        }

        if control == ShellEmptyControl::Escape && self.active_completion.is_some() {
            self.dismiss_completion();
            return ShellInputAction::Consumed;
        }

        ShellInputAction::Ignored
    }

    pub(super) fn refresh_candidates(
        &mut self,
        command: &str,
        cursor: usize,
        history: &[String],
        cwd: &Path,
        home: Option<&Path>,
        limit: usize,
    ) -> io::Result<()> {
        self.update_command(command, cursor);
        if self.kind != InputKind::Shell || limit == 0 {
            return Ok(());
        }

        let path_completion = complete_shell_paths(cwd, home, command, self.cursor, limit)?;
        self.path_replacement_range = path_completion
            .as_ref()
            .map(|completion| completion.replacement_range.clone());
        self.path_candidates = path_completion
            .map(|completion| completion.candidates)
            .unwrap_or_default();
        self.history_candidates = complete_shell_history(history, command, limit);
        self.active_completion = if self.path_candidates.is_empty() {
            (!self.history_candidates.is_empty()).then_some(ShellCompletionKind::History)
        } else {
            Some(ShellCompletionKind::Path)
        };
        self.selected = None;
        Ok(())
    }

    pub(super) fn history_candidates(&self) -> &[ShellHistoryCandidate] {
        &self.history_candidates
    }

    pub(super) fn path_candidates(&self) -> &[ShellPathCandidate] {
        &self.path_candidates
    }

    pub(super) fn active_completion(&self) -> Option<ShellCompletionKind> {
        self.active_completion
    }

    pub(super) fn selected_index(&self) -> Option<usize> {
        self.selected
    }

    pub(super) fn handle_completion_key(&mut self, key: ShellCompletionKey) -> ShellInputAction {
        let candidate_count = self.active_candidate_count();
        if self.kind != InputKind::Shell || candidate_count == 0 {
            return ShellInputAction::Ignored;
        }

        match key {
            ShellCompletionKey::Previous => {
                self.selected = Some(self.selected.map_or(0, |index| index.saturating_sub(1)));
                ShellInputAction::Consumed
            }
            ShellCompletionKey::Next => {
                self.selected = Some(
                    self.selected
                        .map_or(0, |index| (index + 1).min(candidate_count - 1)),
                );
                ShellInputAction::Consumed
            }
            ShellCompletionKey::Complete => {
                let index = self.selected.unwrap_or(0);
                self.apply_candidate(index)
            }
            ShellCompletionKey::Cancel => {
                self.dismiss_completion();
                ShellInputAction::Consumed
            }
        }
    }

    pub(super) fn submit(&mut self, execution: ShellExecution) -> ShellInputAction {
        if self.kind != InputKind::Shell
            || self.command.trim().is_empty()
            || !is_safe_command_text(&self.command)
        {
            return ShellInputAction::Ignored;
        }

        let submission = UserShellSubmission {
            command: self.command.clone(),
            execution,
        };
        self.leave_shell();
        ShellInputAction::Submit(submission)
    }

    pub(super) fn restore(&mut self, command: String) -> ShellInputAction {
        self.enter_shell(command)
    }

    fn enter_shell(&mut self, command: String) -> ShellInputAction {
        self.kind = InputKind::Shell;
        self.cursor = command.len();
        self.command = command.clone();
        self.dismiss_completion();
        ShellInputAction::EnterShell {
            cursor: command.len(),
            command,
        }
    }

    fn leave_shell(&mut self) {
        self.kind = InputKind::Prompt;
        self.command.clear();
        self.cursor = 0;
        self.dismiss_completion();
    }

    fn dismiss_completion(&mut self) {
        self.history_candidates.clear();
        self.path_candidates.clear();
        self.path_replacement_range = None;
        self.active_completion = None;
        self.selected = None;
    }

    fn active_candidate_count(&self) -> usize {
        match self.active_completion {
            Some(ShellCompletionKind::History) => self.history_candidates.len(),
            Some(ShellCompletionKind::Path) => self.path_candidates.len(),
            None => 0,
        }
    }

    fn apply_candidate(&mut self, index: usize) -> ShellInputAction {
        let replacement = match self.active_completion {
            Some(ShellCompletionKind::History) => {
                let Some(candidate) = self.history_candidates.get(index) else {
                    self.dismiss_completion();
                    return ShellInputAction::Ignored;
                };
                let command = candidate.command.clone();
                self.dismiss_completion();
                self.command.clone_from(&command);
                self.cursor = command.len();
                return ShellInputAction::ReplaceCommand {
                    cursor: command.len(),
                    command,
                };
            }
            Some(ShellCompletionKind::Path) => {
                let Some(candidate) = self.path_candidates.get(index) else {
                    self.dismiss_completion();
                    return ShellInputAction::Ignored;
                };
                candidate.replacement.clone()
            }
            None => return ShellInputAction::Ignored,
        };
        let Some(range) = self.path_replacement_range.clone() else {
            self.dismiss_completion();
            return ShellInputAction::Ignored;
        };

        let prefix = &self.command[..range.start];
        let suffix = &self.command[range.end..];
        let mut command = String::with_capacity(prefix.len() + replacement.len() + suffix.len());
        command.push_str(prefix);
        command.push_str(&replacement);
        let cursor = command.len();
        command.push_str(suffix);
        self.dismiss_completion();
        self.command.clone_from(&command);
        self.cursor = cursor;
        ShellInputAction::ReplaceCommand { command, cursor }
    }
}

#[derive(Debug)]
struct ShellPathCompletion {
    replacement_range: Range<usize>,
    candidates: Vec<ShellPathCandidate>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TokenStyle {
    Unquoted,
    SingleQuoted,
    DoubleQuoted,
}

#[derive(Debug)]
struct PathToken {
    replacement_range: Range<usize>,
    decoded_prefix: String,
    style: TokenStyle,
}

fn complete_shell_history(
    history: &[String],
    command: &str,
    limit: usize,
) -> Vec<ShellHistoryCandidate> {
    let mut seen = HashSet::new();
    history
        .iter()
        .map(|entry| entry.strip_prefix('!').unwrap_or(entry))
        .filter(|entry| {
            is_safe_command_text(entry)
                && entry.starts_with(command)
                && *entry != command
                && seen.insert((*entry).to_owned())
        })
        .take(limit)
        .map(|command| ShellHistoryCandidate {
            command: command.to_owned(),
        })
        .collect()
}

fn complete_shell_paths(
    cwd: &Path,
    home: Option<&Path>,
    command: &str,
    cursor: usize,
    limit: usize,
) -> io::Result<Option<ShellPathCompletion>> {
    let Some(token) = path_token(command, cursor) else {
        return Ok(None);
    };
    let Some(slash) = token.decoded_prefix.rfind('/') else {
        return Ok(None);
    };
    let logical_parent = &token.decoded_prefix[..=slash];
    let query = &token.decoded_prefix[slash + 1..];
    let Some(directory) = completion_directory(cwd, home, logical_parent, token.style) else {
        return Ok(None);
    };

    let children = match fs::read_dir(&directory) {
        Ok(children) => children,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut candidates = children
        .filter_map(Result::ok)
        .filter_map(|entry| path_candidate(entry, logical_parent, query, token.style))
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        path_kind_rank(left.kind)
            .cmp(&path_kind_rank(right.kind))
            .then_with(|| left.display_path.cmp(&right.display_path))
    });
    candidates.truncate(limit);

    Ok(Some(ShellPathCompletion {
        replacement_range: token.replacement_range,
        candidates,
    }))
}

fn path_token(command: &str, cursor: usize) -> Option<PathToken> {
    if cursor > command.len() || !command.is_char_boundary(cursor) {
        return None;
    }
    let replacement_range = shell_word_at(command, cursor)?;
    let word = &command[replacement_range.clone()];
    let prefix_len = cursor.checked_sub(replacement_range.start)?;
    let (decoded_prefix, style) = decode_word_prefix(word, prefix_len)?;
    if decoded_prefix.chars().any(char::is_control) {
        return None;
    }
    Some(PathToken {
        replacement_range,
        decoded_prefix,
        style,
    })
}

fn shell_word_at(command: &str, cursor: usize) -> Option<Range<usize>> {
    let mut word_start = None;
    let mut quote = None;
    let mut escaped = false;
    let mut spans = Vec::new();

    for (index, character) in command.char_indices() {
        if word_start.is_none() && !character.is_whitespace() {
            word_start = Some(index);
        }
        if word_start.is_none() {
            continue;
        }

        if escaped {
            escaped = false;
            continue;
        }
        match quote {
            Some(active) if character == active => quote = None,
            Some('\'') => {}
            Some(_) if character == '\\' => escaped = true,
            Some(_) => {}
            None if character == '\\' => escaped = true,
            None if matches!(character, '\'' | '"') => quote = Some(character),
            None if character.is_whitespace() => {
                spans.push(word_start.take().unwrap()..index);
            }
            None => {}
        }
    }
    if let Some(start) = word_start {
        spans.push(start..command.len());
    }

    spans
        .into_iter()
        .find(|range| range.start <= cursor && cursor <= range.end)
}

fn decode_word_prefix(word: &str, prefix_len: usize) -> Option<(String, TokenStyle)> {
    if prefix_len > word.len() || !word.is_char_boundary(prefix_len) {
        return None;
    }
    let prefix = &word[..prefix_len];
    if let Some(single_quoted_word) = word.strip_prefix('\'') {
        if single_quoted_word.contains('\'') && !word.ends_with('\'') {
            return None;
        }
        let prefix_content = prefix.strip_prefix('\'')?;
        let content = prefix_content.strip_suffix('\'').unwrap_or(prefix_content);
        if content.contains('\'') {
            return None;
        }
        return Some((content.to_owned(), TokenStyle::SingleQuoted));
    }
    if word.starts_with('"') {
        let content = prefix
            .strip_prefix('"')?
            .strip_suffix('"')
            .unwrap_or_else(|| prefix.strip_prefix('"').unwrap());
        return decode_double_quoted(content).map(|value| (value, TokenStyle::DoubleQuoted));
    }
    if has_unescaped_quote(word) {
        return None;
    }
    decode_unquoted(prefix).map(|value| (value, TokenStyle::Unquoted))
}

fn has_unescaped_quote(value: &str) -> bool {
    let mut escaped = false;
    for character in value.chars() {
        if escaped {
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if matches!(character, '\'' | '"') {
            return true;
        }
    }
    false
}

fn decode_unquoted(value: &str) -> Option<String> {
    let mut decoded = String::with_capacity(value.len());
    let mut characters = value.chars();
    while let Some(character) = characters.next() {
        if character == '\\' {
            decoded.push(characters.next()?);
        } else {
            decoded.push(character);
        }
    }
    Some(decoded)
}

fn decode_double_quoted(value: &str) -> Option<String> {
    let mut decoded = String::with_capacity(value.len());
    let mut characters = value.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            if character == '"' {
                return None;
            }
            decoded.push(character);
            continue;
        }
        let next = characters.next()?;
        if matches!(next, '\\' | '"' | '$' | '`') {
            decoded.push(next);
        } else {
            decoded.push('\\');
            decoded.push(next);
        }
    }
    Some(decoded)
}

fn completion_directory(
    cwd: &Path,
    home: Option<&Path>,
    logical_parent: &str,
    style: TokenStyle,
) -> Option<PathBuf> {
    if logical_parent == "~/" || logical_parent.starts_with("~/") {
        if style != TokenStyle::Unquoted {
            return None;
        }
        return home.map(|home| home.join(&logical_parent[2..]));
    }
    if logical_parent.starts_with('~') {
        return None;
    }
    let parent = Path::new(logical_parent);
    Some(if parent.is_absolute() {
        parent.to_owned()
    } else {
        cwd.join(parent)
    })
}

fn path_candidate(
    entry: fs::DirEntry,
    logical_parent: &str,
    query: &str,
    style: TokenStyle,
) -> Option<ShellPathCandidate> {
    let file_name = entry.file_name().into_string().ok()?;
    if !file_name.starts_with(query) || file_name.chars().any(char::is_control) {
        return None;
    }
    let metadata = entry.metadata().ok()?;
    let kind = if metadata.is_dir() {
        ShellPathKind::Directory
    } else if metadata.is_file() {
        ShellPathKind::File
    } else {
        return None;
    };
    let suffix = if kind == ShellPathKind::Directory {
        "/"
    } else {
        ""
    };
    let display_path = format!("{logical_parent}{file_name}{suffix}");
    let replacement = encode_shell_word(&display_path, style);
    Some(ShellPathCandidate {
        display_path,
        replacement,
        path: entry.path(),
        kind,
    })
}

fn encode_shell_word(value: &str, style: TokenStyle) -> String {
    match style {
        TokenStyle::Unquoted => escape_unquoted(value),
        TokenStyle::SingleQuoted if !value.contains('\'') => format!("'{value}'"),
        TokenStyle::SingleQuoted | TokenStyle::DoubleQuoted => quote_double(value),
    }
}

fn escape_unquoted(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if !is_unquoted_safe(character) {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

fn quote_double(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for character in value.chars() {
        if matches!(character, '\\' | '"' | '$' | '`') {
            quoted.push('\\');
        }
        quoted.push(character);
    }
    quoted.push('"');
    quoted
}

fn is_unquoted_safe(character: char) -> bool {
    character.is_alphanumeric()
        || matches!(
            character,
            '_' | '-' | '.' | '/' | '~' | '+' | ':' | ',' | '%' | '@' | '='
        )
}

fn path_kind_rank(kind: ShellPathKind) -> u8 {
    match kind {
        ShellPathKind::Directory => 0,
        ShellPathKind::File => 1,
    }
}

fn is_safe_command_text(command: &str) -> bool {
    !command.chars().any(|character| {
        character == '\0' || (character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    })
}

fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        root: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "tickloop-shell-input-{}-{unique}-{}",
                std::process::id(),
                NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&root).unwrap();
            Self { root }
        }

        fn file(&self, relative: &str) {
            let path = self.root.join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, "fixture").unwrap();
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn enter_shell(state: &mut ShellInputState) {
        assert_eq!(
            state.try_tty_character("", 0, '!'),
            ShellInputAction::EnterShell {
                command: String::new(),
                cursor: 0,
            }
        );
    }

    #[test]
    fn only_an_editable_empty_tty_prompt_enters_shell_mode() {
        let mut state = ShellInputState::default();
        assert!(matches!(
            state.try_tty_character("", 0, '!'),
            ShellInputAction::EnterShell { .. }
        ));
        assert_eq!(state.kind(), InputKind::Shell);
        assert_eq!(state.command(), "");

        for (prompt, cursor, character) in [("echo ", 5, '!'), ("", 0, '?')] {
            let mut state = ShellInputState::default();
            assert_eq!(
                state.try_tty_character(prompt, cursor, character),
                ShellInputAction::Ignored
            );
            assert_eq!(state.kind(), InputKind::Prompt);
        }
    }

    #[test]
    fn bracketed_tty_paste_has_an_explicit_entry_path() {
        let mut state = ShellInputState::default();
        assert_eq!(
            state.try_tty_paste("", 0, "!printf '你好'"),
            ShellInputAction::EnterShell {
                command: "printf '你好'".to_owned(),
                cursor: "printf '你好'".len(),
            }
        );
        assert_eq!(state.command(), "printf '你好'");
        assert_eq!(state.cursor(), "printf '你好'".len());

        let mut mid_prompt = ShellInputState::default();
        assert_eq!(
            mid_prompt.try_tty_paste("explain ", 8, "!pwd"),
            ShellInputAction::Ignored
        );
        assert_eq!(mid_prompt.kind(), InputKind::Prompt);
    }

    #[test]
    fn empty_shell_controls_exit_without_creating_a_submission() {
        for control in [
            ShellEmptyControl::Escape,
            ShellEmptyControl::Backspace,
            ShellEmptyControl::Clear,
        ] {
            let mut state = ShellInputState::default();
            enter_shell(&mut state);
            assert_eq!(
                state.handle_empty_control(control),
                ShellInputAction::ExitShell
            );
            assert_eq!(state.kind(), InputKind::Prompt);
        }
    }

    #[test]
    fn foreground_and_background_submissions_stay_structured() {
        for execution in [ShellExecution::Foreground, ShellExecution::Background] {
            let mut state = ShellInputState::default();
            enter_shell(&mut state);
            state.update_command("printf ok", "printf ok".len());

            let ShellInputAction::Submit(submission) = state.submit(execution) else {
                panic!("shell submission was not produced");
            };
            assert_eq!(submission.command(), "printf ok");
            assert_eq!(submission.execution(), execution);
            assert_eq!(state.kind(), InputKind::Prompt);
        }

        let mut empty = ShellInputState::default();
        enter_shell(&mut empty);
        assert_eq!(
            empty.submit(ShellExecution::Foreground),
            ShellInputAction::Ignored
        );
    }

    #[test]
    fn history_and_path_candidates_remain_separate_and_tab_never_submits() {
        let fixture = Fixture::new();
        fixture.file("src/main.rs");
        let mut state = ShellInputState::default();
        enter_shell(&mut state);
        let history = vec![
            "!cargo test".to_owned(),
            "cargo check".to_owned(),
            "!cargo test".to_owned(),
        ];
        state
            .refresh_candidates("cargo ", 6, &history, &fixture.root, None, 8)
            .unwrap();
        assert_eq!(
            state.active_completion(),
            Some(ShellCompletionKind::History)
        );
        assert_eq!(state.history_candidates().len(), 2);
        assert!(state.path_candidates().is_empty());
        assert_eq!(state.selected_index(), None);
        assert_eq!(
            state.handle_completion_key(ShellCompletionKey::Complete),
            ShellInputAction::ReplaceCommand {
                command: "cargo test".to_owned(),
                cursor: "cargo test".len(),
            }
        );
        assert_eq!(state.kind(), InputKind::Shell);

        state
            .refresh_candidates("cat ./sr", 8, &history, &fixture.root, None, 8)
            .unwrap();
        assert_eq!(state.active_completion(), Some(ShellCompletionKind::Path));
        assert!(!state.path_candidates().is_empty());
        assert!(state.history_candidates().is_empty());
        assert!(matches!(
            state.handle_completion_key(ShellCompletionKey::Complete),
            ShellInputAction::ReplaceCommand { .. }
        ));
        assert_eq!(state.kind(), InputKind::Shell);
    }

    #[test]
    fn unquoted_path_completion_escapes_spaces_metacharacters_and_unicode() {
        let fixture = Fixture::new();
        fixture.file("资料/设计 $草稿.md");
        let mut state = ShellInputState::default();
        enter_shell(&mut state);
        let command = "cat ./资料/设";
        state
            .refresh_candidates(command, command.len(), &[], &fixture.root, None, 8)
            .unwrap();

        let candidate = state.path_candidates().first().unwrap();
        assert_eq!(candidate.display_path, "./资料/设计 $草稿.md");
        assert_eq!(candidate.replacement, "./资料/设计\\ \\$草稿.md");
        assert_eq!(candidate.kind, ShellPathKind::File);
        assert_eq!(
            state.handle_completion_key(ShellCompletionKey::Complete),
            ShellInputAction::ReplaceCommand {
                command: "cat ./资料/设计\\ \\$草稿.md".to_owned(),
                cursor: "cat ./资料/设计\\ \\$草稿.md".len(),
            }
        );
    }

    #[test]
    fn escaped_shell_characters_are_decoded_before_path_matching() {
        let fixture = Fixture::new();
        fixture.file("notes/design review.md");
        fixture.file("notes/author's.md");
        let mut state = ShellInputState::default();
        enter_shell(&mut state);

        let escaped_space = "cat ./notes/design\\ r";
        state
            .refresh_candidates(
                escaped_space,
                escaped_space.len(),
                &[],
                &fixture.root,
                None,
                8,
            )
            .unwrap();
        assert_eq!(
            state.path_candidates()[0].replacement,
            "./notes/design\\ review.md"
        );

        let escaped_quote = "cat ./notes/author\\'";
        state
            .refresh_candidates(
                escaped_quote,
                escaped_quote.len(),
                &[],
                &fixture.root,
                None,
                8,
            )
            .unwrap();
        assert_eq!(
            state.path_candidates()[0].replacement,
            "./notes/author\\'s.md"
        );
    }

    #[test]
    fn quoted_path_completion_preserves_shell_literal_semantics() {
        let fixture = Fixture::new();
        fixture.file("notes/a $draft.md");
        fixture.file("notes/author's.md");
        let mut state = ShellInputState::default();
        enter_shell(&mut state);

        let double_quoted = "cat \"./notes/a";
        state
            .refresh_candidates(
                double_quoted,
                double_quoted.len(),
                &[],
                &fixture.root,
                None,
                8,
            )
            .unwrap();
        assert_eq!(
            state.path_candidates()[0].replacement,
            "\"./notes/a \\$draft.md\""
        );

        let single_quoted = "cat './notes/auth";
        state
            .refresh_candidates(
                single_quoted,
                single_quoted.len(),
                &[],
                &fixture.root,
                None,
                8,
            )
            .unwrap();
        assert_eq!(
            state.path_candidates()[0].replacement,
            "\"./notes/author's.md\""
        );
    }

    #[test]
    fn tilde_and_explicit_relative_paths_use_the_expected_directory() {
        let cwd = Fixture::new();
        let home = Fixture::new();
        cwd.file("src/lib.rs");
        home.file("notes/home.md");
        let mut state = ShellInputState::default();
        enter_shell(&mut state);

        state
            .refresh_candidates("cat ./sr", 8, &[], &cwd.root, Some(&home.root), 8)
            .unwrap();
        assert_eq!(state.path_candidates()[0].display_path, "./src/");

        state
            .refresh_candidates("cat ~/notes/ho", 14, &[], &cwd.root, Some(&home.root), 8)
            .unwrap();
        assert_eq!(state.path_candidates()[0].display_path, "~/notes/home.md");
        assert_eq!(
            state.path_candidates()[0].path,
            home.root.join("notes/home.md")
        );

        state
            .refresh_candidates("cat \"~/notes/ho", 15, &[], &cwd.root, Some(&home.root), 8)
            .unwrap();
        assert!(state.path_candidates().is_empty());
    }

    #[test]
    fn completion_replaces_only_the_word_around_a_utf8_cursor() {
        let fixture = Fixture::new();
        fixture.file("资料/模型.md");
        let mut state = ShellInputState::default();
        enter_shell(&mut state);
        let command = "echo before ./资料/模xx after";
        let cursor = command.find("模").unwrap() + "模".len();
        state
            .refresh_candidates(command, cursor, &[], &fixture.root, None, 8)
            .unwrap();

        assert_eq!(
            state.handle_completion_key(ShellCompletionKey::Complete),
            ShellInputAction::ReplaceCommand {
                command: "echo before ./资料/模型.md after".to_owned(),
                cursor: "echo before ./资料/模型.md".len(),
            }
        );
    }

    #[cfg(unix)]
    #[test]
    fn path_candidates_exclude_non_utf8_and_terminal_control_names() {
        use std::os::unix::ffi::OsStringExt;

        let fixture = Fixture::new();
        fixture.file("safe.txt");
        fs::write(fixture.root.join("line\nbreak.txt"), "unsafe").unwrap();
        let _ = fs::write(
            fixture
                .root
                .join(std::ffi::OsString::from_vec(b"invalid-\xff.txt".to_vec())),
            "unsafe",
        );
        let mut state = ShellInputState::default();
        enter_shell(&mut state);
        state
            .refresh_candidates("cat ./", 6, &[], &fixture.root, None, 100)
            .unwrap();

        assert_eq!(state.path_candidates().len(), 1);
        assert_eq!(state.path_candidates()[0].display_path, "./safe.txt");
    }

    #[test]
    fn navigation_and_cancel_do_not_cross_input_or_execution_boundaries() {
        let fixture = Fixture::new();
        let history = vec!["!echo one".to_owned(), "!echo two".to_owned()];
        let mut state = ShellInputState::default();
        enter_shell(&mut state);
        state
            .refresh_candidates("echo ", 5, &history, &fixture.root, None, 8)
            .unwrap();

        assert_eq!(
            state.handle_completion_key(ShellCompletionKey::Next),
            ShellInputAction::Consumed
        );
        assert_eq!(state.selected_index(), Some(0));
        assert_eq!(
            state.handle_completion_key(ShellCompletionKey::Next),
            ShellInputAction::Consumed
        );
        assert_eq!(state.selected_index(), Some(1));
        assert_eq!(
            state.handle_completion_key(ShellCompletionKey::Previous),
            ShellInputAction::Consumed
        );
        assert_eq!(state.selected_index(), Some(0));
        assert_eq!(
            state.handle_completion_key(ShellCompletionKey::Cancel),
            ShellInputAction::Consumed
        );
        assert_eq!(state.active_completion(), None);
        assert_eq!(state.command(), "echo ");
    }
}

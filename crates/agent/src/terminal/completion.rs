#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CommandGroup {
    Conversation,
    ModelAndSettings,
    ToolsAndPermissions,
    PlanAndExecution,
}

impl CommandGroup {
    pub(super) fn title(self) -> &'static str {
        match self {
            Self::Conversation => "Conversation",
            Self::ModelAndSettings => "Model & settings",
            Self::ToolsAndPermissions => "Tools & permissions",
            Self::PlanAndExecution => "Plan & execution",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct CommandSpec {
    pub command: &'static str,
    pub arguments: &'static str,
    pub description: &'static str,
    pub menu_group: Option<CommandGroup>,
}

impl CommandSpec {
    pub(super) fn syntax(&self) -> String {
        if self.arguments.is_empty() {
            self.command.to_owned()
        } else {
            format!("{} {}", self.command, self.arguments)
        }
    }
}

const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        command: "/logout",
        arguments: "",
        description: "Remove the local Codex subscription login.",
        menu_group: Some(CommandGroup::ModelAndSettings),
    },
    CommandSpec {
        command: "/tickloop",
        arguments: "[REQUEST]",
        description: "Loads the Tick Loop Workspace Skill.",
        menu_group: Some(CommandGroup::Conversation),
    },
    CommandSpec {
        command: "/clear",
        arguments: "",
        description: "Starts a new Chat; this Chat's background work keeps running.",
        menu_group: Some(CommandGroup::Conversation),
    },
    CommandSpec {
        command: "/branch",
        arguments: "[NAME]",
        description: "Branches this conversation at the current point.",
        menu_group: Some(CommandGroup::Conversation),
    },
    CommandSpec {
        command: "/new",
        arguments: "",
        description: "Starts a new goal with the next input in this Chat.",
        menu_group: Some(CommandGroup::Conversation),
    },
    CommandSpec {
        command: "/resume",
        arguments: "[CHAT]",
        description: "Resumes a saved Chat, or opens the Chat picker.",
        menu_group: Some(CommandGroup::Conversation),
    },
    CommandSpec {
        command: "/loops",
        arguments: "",
        description: "Lists or chooses a goal in the current Chat.",
        menu_group: Some(CommandGroup::Conversation),
    },
    CommandSpec {
        command: "/rewind",
        arguments: "",
        description: "Opens the Rewind checkpoint selector.",
        menu_group: Some(CommandGroup::Conversation),
    },
    CommandSpec {
        command: "/checkpoint",
        arguments: "",
        description: "Opens the Rewind checkpoint selector (/rewind alias).",
        menu_group: Some(CommandGroup::Conversation),
    },
    CommandSpec {
        command: "/undo",
        arguments: "",
        description: "Opens the Rewind checkpoint selector (/rewind alias).",
        menu_group: Some(CommandGroup::Conversation),
    },
    CommandSpec {
        command: "/model",
        arguments: "[FILE]",
        description: "Choose a Provider and model for this Chat, or configure the separate Jev (TypeSafe) key under Judgment settings.",
        menu_group: Some(CommandGroup::ModelAndSettings),
    },
    CommandSpec {
        command: "/thinking",
        arguments: "[LEVEL]",
        description: "Choose a Thinking level supported by the current model; no argument opens the picker.",
        menu_group: Some(CommandGroup::ModelAndSettings),
    },
    CommandSpec {
        command: "/settings",
        arguments: "",
        description: "Opens settings for this Chat.",
        menu_group: Some(CommandGroup::ModelAndSettings),
    },
    CommandSpec {
        command: "/theme",
        arguments: "",
        description: "Opens the theme picker for this terminal session.",
        menu_group: Some(CommandGroup::ModelAndSettings),
    },
    CommandSpec {
        command: "/tui",
        arguments: "[inline|fullscreen]",
        description: "Shows or switches the terminal renderer.",
        menu_group: Some(CommandGroup::ModelAndSettings),
    },
    CommandSpec {
        command: "/cwd",
        arguments: "[PATH]",
        description: "Shows or sets the main working directory.",
        menu_group: Some(CommandGroup::ModelAndSettings),
    },
    CommandSpec {
        command: "/commands",
        arguments: "",
        description: "Lists or manages Shell processes.",
        menu_group: Some(CommandGroup::ToolsAndPermissions),
    },
    CommandSpec {
        command: "/details",
        arguments: "",
        description: "Opens the full conversation transcript.",
        menu_group: Some(CommandGroup::ToolsAndPermissions),
    },
    CommandSpec {
        command: "/diff",
        arguments: "",
        description: "Reviews current and saved Agent file changes.",
        menu_group: Some(CommandGroup::ToolsAndPermissions),
    },
    CommandSpec {
        command: "/rules",
        arguments: "",
        description: "Lists permission rules for this Chat.",
        menu_group: Some(CommandGroup::ToolsAndPermissions),
    },
    CommandSpec {
        command: "/permissions",
        arguments: "",
        description: "Manage recent denials, rules, Auto review, and directories.",
        menu_group: Some(CommandGroup::ToolsAndPermissions),
    },
    CommandSpec {
        command: "/plan",
        arguments: "[GOAL]",
        description: "Shows the selected goal's saved plan.",
        menu_group: Some(CommandGroup::PlanAndExecution),
    },
    CommandSpec {
        command: "/compact",
        arguments: "[INSTRUCTIONS]",
        description: "Summarizes context behind a compaction boundary.",
        menu_group: Some(CommandGroup::PlanAndExecution),
    },
    CommandSpec {
        command: "/triggers",
        arguments: "",
        description: "Shows the selected goal's trigger.",
        menu_group: Some(CommandGroup::PlanAndExecution),
    },
    CommandSpec {
        command: "/subscriptions",
        arguments: "",
        description: "Lists this Chat's Tick Subscriptions and changes or cancels one.",
        menu_group: Some(CommandGroup::PlanAndExecution),
    },
    CommandSpec {
        command: "/subs",
        arguments: "",
        description: "Lists this Chat's Tick Subscriptions and changes or cancels one.",
        menu_group: None,
    },
    CommandSpec {
        command: "/interrupt",
        arguments: "",
        description: "Interrupts the current run.",
        menu_group: Some(CommandGroup::PlanAndExecution),
    },
    CommandSpec {
        command: "/stop",
        arguments: "[GOAL]",
        description: "Stops a goal.",
        menu_group: Some(CommandGroup::PlanAndExecution),
    },
    CommandSpec {
        command: "/goal",
        arguments: "[GOAL]",
        description: "Selects a goal in the current Chat.",
        menu_group: None,
    },
    CommandSpec {
        command: "/answer",
        arguments: "QUESTION ANSWER",
        description: "Replies to a clarification question.",
        menu_group: None,
    },
    CommandSpec {
        command: "/config",
        arguments: "",
        description: "Opens settings for this Chat.",
        menu_group: None,
    },
    CommandSpec {
        command: "/mode",
        arguments: "[auto|ask|fullAccess|plan]",
        description: "Changes execution mode; use /plan to plan a goal.",
        menu_group: None,
    },
    CommandSpec {
        command: "/allow",
        arguments: "REQUEST [once|session|persistent]",
        description: "Approves a permission request with an optional scope.",
        menu_group: None,
    },
    CommandSpec {
        command: "/deny",
        arguments: "REQUEST",
        description: "Denies a permission request.",
        menu_group: None,
    },
    CommandSpec {
        command: "/rule",
        arguments: "allow|ask|deny TOOL [session|persistent] | remove UUID",
        description: "Adds or removes a permission rule.",
        menu_group: None,
    },
    CommandSpec {
        command: "/add-dir",
        arguments: "PATH",
        description: "Adds an allowed working directory.",
        menu_group: None,
    },
    CommandSpec {
        command: "/search-config",
        arguments: "FILE|off",
        description: "Configures or disables web search.",
        menu_group: None,
    },
    CommandSpec {
        command: "/approve-plan",
        arguments: "GOAL VERSION MODE",
        description: "Approves a saved plan version for later execution.",
        menu_group: None,
    },
    CommandSpec {
        command: "/tasks",
        arguments: "",
        description: "Lists or manages Shell processes.",
        menu_group: None,
    },
    CommandSpec {
        command: "/bashes",
        arguments: "",
        description: "Lists or manages Shell processes.",
        menu_group: None,
    },
    CommandSpec {
        command: "/background",
        arguments: "OPERATION",
        description: "Moves a Shell process to the background.",
        menu_group: None,
    },
    CommandSpec {
        command: "/command-output",
        arguments: "OPERATION [OFFSET] [LIMIT]",
        description: "Reads a bounded range of Shell process output.",
        menu_group: None,
    },
    CommandSpec {
        command: "/stop-command",
        arguments: "OPERATION",
        description: "Stops a Shell process.",
        menu_group: None,
    },
    CommandSpec {
        command: "/operations",
        arguments: "",
        description: "Shows effects recorded in this process and their outcomes.",
        menu_group: None,
    },
    CommandSpec {
        command: "/events",
        arguments: "PROMPT",
        description: "Enables explicit events for the selected goal.",
        menu_group: None,
    },
    CommandSpec {
        command: "/trigger",
        arguments: "SOURCE [JSON]",
        description: "Delivers an event to the selected goal.",
        menu_group: None,
    },
    CommandSpec {
        command: "/schedule",
        arguments: "SECONDS [--for DURATION_SECONDS] PROMPT",
        description: "Configures a local timer for the selected goal.",
        menu_group: None,
    },
    CommandSpec {
        command: "/clear-trigger",
        arguments: "",
        description: "Removes the selected goal's trigger.",
        menu_group: None,
    },
    CommandSpec {
        command: "/help",
        arguments: "",
        description: "Shows command help.",
        menu_group: None,
    },
    CommandSpec {
        command: "/exit",
        arguments: "",
        description: "Cancels work and exits the host.",
        menu_group: None,
    },
    CommandSpec {
        command: "/quit",
        arguments: "",
        description: "Cancels work and exits the host.",
        menu_group: None,
    },
];

pub(super) fn builtin_commands() -> &'static [CommandSpec] {
    COMMANDS
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CompletionKey {
    Previous,
    Next,
    Complete,
    Choose,
    Cancel,
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum CompletionAction {
    Ignored,
    Consumed,
    ReplaceDraft { draft: String, cursor: usize },
}

#[derive(Debug, Default)]
pub(super) struct Completion {
    matches: Vec<usize>,
    selected: Option<usize>,
    draft: String,
    token_start: usize,
    token_end: usize,
}

impl Completion {
    #[cfg(test)]
    fn update(&mut self, draft: &str) {
        self.update_at(draft, draft.len());
    }

    pub(super) fn update_at(&mut self, draft: &str, cursor: usize) {
        self.dismiss();

        let Some((token_start, token_end)) = command_token(draft, cursor) else {
            return;
        };

        let query = &draft[token_start + 1..cursor];

        self.matches.extend(
            COMMANDS
                .iter()
                .enumerate()
                .filter(|(_, spec)| spec.command[1..].starts_with(query))
                .map(|(index, _)| index),
        );
        self.matches.sort_by(|left, right| {
            let left = &COMMANDS[*left];
            let right = &COMMANDS[*right];
            left.command.cmp(right.command)
        });
        if !self.matches.is_empty() {
            self.draft.push_str(draft);
            self.token_start = token_start;
            self.token_end = token_end;
        }
    }

    pub(super) fn dismiss(&mut self) {
        self.matches.clear();
        self.selected = None;
        self.draft.clear();
        self.token_start = 0;
        self.token_end = 0;
    }

    pub(super) fn is_open(&self) -> bool {
        !self.matches.is_empty()
    }

    pub(super) fn candidates(&self) -> impl ExactSizeIterator<Item = &'static CommandSpec> + '_ {
        self.matches.iter().map(|index| &COMMANDS[*index])
    }

    pub(super) fn selected_index(&self) -> Option<usize> {
        self.selected
    }

    pub(super) fn handle_key(&mut self, key: CompletionKey) -> CompletionAction {
        if !self.is_open() {
            return CompletionAction::Ignored;
        }

        match key {
            CompletionKey::Previous => {
                self.selected = Some(self.selected.map_or(0, |index| index.saturating_sub(1)));
                CompletionAction::Consumed
            }
            CompletionKey::Next => {
                self.selected = Some(
                    self.selected
                        .map_or(0, |index| (index + 1).min(self.matches.len() - 1)),
                );
                CompletionAction::Consumed
            }
            CompletionKey::Complete => {
                let index = self.selected.unwrap_or(0);
                let replacement = self.completed_draft(COMMANDS[self.matches[index]]);
                self.dismiss();
                replacement
            }
            CompletionKey::Choose => {
                let Some(index) = self.selected else {
                    return CompletionAction::Ignored;
                };
                let replacement = self.completed_draft(COMMANDS[self.matches[index]]);
                self.dismiss();
                replacement
            }
            CompletionKey::Cancel => {
                self.dismiss();
                CompletionAction::Consumed
            }
        }
    }

    fn completed_draft(&self, command: CommandSpec) -> CompletionAction {
        let prefix = &self.draft[..self.token_start];
        let suffix = &self.draft[self.token_end..];
        let mut draft = String::with_capacity(
            prefix.len()
                + command.command.len()
                + usize::from(!command.arguments.is_empty())
                + suffix.len(),
        );
        draft.push_str(prefix);
        draft.push_str(command.command);
        let mut cursor = draft.len();

        if !command.arguments.is_empty() {
            if let Some(separator) = suffix
                .chars()
                .next()
                .filter(|character| character.is_whitespace() && !matches!(character, '\r' | '\n'))
            {
                cursor += separator.len_utf8();
            } else {
                draft.push(' ');
                cursor += 1;
            }
        }

        draft.push_str(suffix);
        CompletionAction::ReplaceDraft { draft, cursor }
    }
}

fn command_token(draft: &str, cursor: usize) -> Option<(usize, usize)> {
    if cursor > draft.len() || !draft.is_char_boundary(cursor) || cursor == 0 {
        return None;
    }

    let before_cursor = &draft[..cursor];
    if before_cursor.chars().next_back()?.is_whitespace() {
        return None;
    }
    let token_start = before_cursor
        .char_indices()
        .rev()
        .find(|(_, character)| character.is_whitespace())
        .map_or(0, |(index, character)| index + character.len_utf8());
    if draft.as_bytes().get(token_start) != Some(&b'/') {
        return None;
    }

    let token_end = draft[cursor..]
        .char_indices()
        .find(|(_, character)| character.is_whitespace())
        .map_or(draft.len(), |(index, _)| cursor + index);
    Some((token_start, token_end))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command_names(completion: &Completion) -> Vec<&'static str> {
        completion
            .candidates()
            .map(|candidate| candidate.command)
            .collect()
    }

    #[test]
    fn slash_shows_every_builtin_command() {
        let mut completion = Completion::default();
        completion.update("/");

        assert_eq!(completion.candidates().len(), builtin_commands().len());
        assert_eq!(completion.selected_index(), None);
        assert!(command_names(&completion).contains(&"/help"));
        assert!(command_names(&completion).contains(&"/tasks"));
        assert!(command_names(&completion).contains(&"/bashes"));
        assert!(command_names(&completion).contains(&"/diff"));
        assert!(command_names(&completion).contains(&"/tui"));
        assert_eq!(
            builtin_commands()
                .iter()
                .find(|spec| spec.command == "/answer")
                .unwrap()
                .syntax(),
            "/answer QUESTION ANSWER"
        );
    }

    #[test]
    fn only_prefix_matches_are_returned() {
        let mut completion = Completion::default();
        completion.update("/o");

        let names = command_names(&completion);
        assert_eq!(names.first(), Some(&"/operations"));
        assert!(!names.contains(&"/command-output"));
    }

    #[test]
    fn bashes_alias_is_completed_as_a_builtin_command() {
        let mut completion = Completion::default();
        completion.update("/bash");

        assert_eq!(command_names(&completion), vec!["/bashes"]);
    }

    #[test]
    fn rewind_and_aliases_complete_without_arguments_and_appear_in_conversation_help() {
        for (prefix, command) in [
            ("/rew", "/rewind"),
            ("/checkp", "/checkpoint"),
            ("/und", "/undo"),
        ] {
            let mut completion = Completion::default();
            completion.update(prefix);
            assert_eq!(command_names(&completion), vec![command]);
            let spec = completion.candidates().next().unwrap();
            assert_eq!(spec.syntax(), command);
            assert_eq!(spec.menu_group, Some(CommandGroup::Conversation));
            assert!(
                spec.description
                    .starts_with("Opens the Rewind checkpoint selector")
            );
            assert_eq!(
                completion.handle_key(CompletionKey::Next),
                CompletionAction::Consumed
            );
            assert_eq!(
                completion.handle_key(CompletionKey::Choose),
                CompletionAction::ReplaceDraft {
                    draft: command.into(),
                    cursor: command.len(),
                }
            );
        }
    }

    #[test]
    fn branch_completes_with_an_optional_name_and_has_no_fork_alias() {
        let mut completion = Completion::default();
        completion.update("/bra");

        assert_eq!(command_names(&completion), vec!["/branch"]);
        let spec = completion.candidates().next().unwrap();
        assert_eq!(spec.syntax(), "/branch [NAME]");
        assert_eq!(spec.menu_group, Some(CommandGroup::Conversation));
        assert_eq!(
            completion.handle_key(CompletionKey::Complete),
            CompletionAction::ReplaceDraft {
                draft: "/branch ".into(),
                cursor: "/branch ".len(),
            }
        );

        completion.update("/fork");
        assert!(command_names(&completion).is_empty());
    }

    #[test]
    fn prefix_query_returns_all_matching_commands() {
        let mut completion = Completion::default();
        completion.update("/sto");

        assert_eq!(command_names(&completion), ["/stop", "/stop-command"]);
    }

    #[test]
    fn unknown_command_has_no_candidates() {
        let mut completion = Completion::default();
        completion.update("/not-a-command");

        assert!(!completion.is_open());
        assert_eq!(completion.selected_index(), None);
    }

    #[test]
    fn login_is_not_a_command_or_completion_candidate() {
        assert!(
            builtin_commands()
                .iter()
                .all(|spec| spec.command != "/login")
        );
        let mut completion = Completion::default();
        completion.update("/login");
        assert!(!completion.is_open());
        assert!(command_names(&completion).is_empty());
    }

    #[test]
    fn arrows_move_the_selection_and_enter_only_replaces_the_draft() {
        let mut completion = Completion::default();
        completion.update("/mo");

        assert_eq!(command_names(&completion), ["/mode", "/model"]);
        assert_eq!(
            completion.handle_key(CompletionKey::Next),
            CompletionAction::Consumed
        );
        assert_eq!(completion.selected_index(), Some(0));
        assert_eq!(
            completion.handle_key(CompletionKey::Previous),
            CompletionAction::Consumed
        );
        assert_eq!(completion.selected_index(), Some(0));
        assert_eq!(
            completion.handle_key(CompletionKey::Next),
            CompletionAction::Consumed
        );
        assert_eq!(completion.selected_index(), Some(1));
        assert_eq!(
            completion.handle_key(CompletionKey::Choose),
            CompletionAction::ReplaceDraft {
                draft: "/model ".into(),
                cursor: "/model ".len(),
            }
        );
        assert!(!completion.is_open());
    }

    #[test]
    fn enter_without_a_selection_leaves_submission_to_the_editor() {
        let mut completion = Completion::default();
        completion.update("/mo");

        assert_eq!(
            completion.handle_key(CompletionKey::Choose),
            CompletionAction::Ignored
        );
        assert!(completion.is_open());
        assert_eq!(completion.selected_index(), None);
    }

    #[test]
    fn tab_preserves_a_space_for_command_arguments() {
        let mut completion = Completion::default();
        completion.update("/ans");

        assert_eq!(
            completion.handle_key(CompletionKey::Complete),
            CompletionAction::ReplaceDraft {
                draft: "/answer ".into(),
                cursor: "/answer ".len(),
            }
        );

        completion.update("/hel");
        assert_eq!(
            completion.handle_key(CompletionKey::Complete),
            CompletionAction::ReplaceDraft {
                draft: "/help".into(),
                cursor: "/help".len(),
            }
        );
    }

    #[test]
    fn escape_closes_completion_without_changing_the_draft() {
        let draft = String::from("/mod");
        let mut completion = Completion::default();
        completion.update(&draft);

        assert_eq!(
            completion.handle_key(CompletionKey::Cancel),
            CompletionAction::Consumed
        );
        assert_eq!(draft, "/mod");
        assert!(!completion.is_open());
    }

    #[test]
    fn arguments_close_completion() {
        let mut completion = Completion::default();
        completion.update("/model");
        assert!(completion.is_open());

        completion.update("/model config.json");
        assert!(!completion.is_open());
        assert_eq!(
            completion.handle_key(CompletionKey::Next),
            CompletionAction::Ignored
        );
    }

    #[test]
    fn command_after_unicode_whitespace_replaces_only_the_token() {
        let draft = "研究一下\u{3000}/mo 保留后缀";
        let cursor = "研究一下\u{3000}/mo".len();
        let mut completion = Completion::default();
        completion.update_at(draft, cursor);
        completion.handle_key(CompletionKey::Next);
        completion.handle_key(CompletionKey::Next);

        assert_eq!(
            completion.handle_key(CompletionKey::Choose),
            CompletionAction::ReplaceDraft {
                draft: "研究一下\u{3000}/model 保留后缀".into(),
                cursor: "研究一下\u{3000}/model ".len(),
            }
        );
    }

    #[test]
    fn completion_in_the_middle_replaces_the_whole_token_and_preserves_the_suffix() {
        let draft = "before /modelish after";
        let cursor = "before /mo".len();
        let mut completion = Completion::default();
        completion.update_at(draft, cursor);
        completion.handle_key(CompletionKey::Next);
        completion.handle_key(CompletionKey::Next);

        assert_eq!(
            completion.handle_key(CompletionKey::Complete),
            CompletionAction::ReplaceDraft {
                draft: "before /model after".into(),
                cursor: "before /model ".len(),
            }
        );
    }

    #[test]
    fn line_start_completion_preserves_multiline_text() {
        let draft = "第一行\n/mo\n第二行";
        let cursor = "第一行\n/mo".len();
        let mut completion = Completion::default();
        completion.update_at(draft, cursor);
        completion.handle_key(CompletionKey::Next);
        completion.handle_key(CompletionKey::Next);

        assert_eq!(
            completion.handle_key(CompletionKey::Choose),
            CompletionAction::ReplaceDraft {
                draft: "第一行\n/model \n第二行".into(),
                cursor: "第一行\n/model ".len(),
            }
        );
    }

    #[test]
    fn slash_inside_a_token_and_invalid_utf8_cursor_do_not_open_completion() {
        let mut completion = Completion::default();
        completion.update("prefix/mo");
        assert!(!completion.is_open());

        completion.update_at("中/mo", 1);
        assert!(!completion.is_open());
    }

    #[test]
    fn slash_completion_does_not_claim_file_or_shell_prefixes() {
        let mut completion = Completion::default();

        completion.update("@src/main.rs");
        assert!(!completion.is_open());
        completion.update("!git status");
        assert!(!completion.is_open());
    }

    #[test]
    fn dismiss_prevents_keys_from_crossing_an_approval_menu_seam() {
        let mut completion = Completion::default();
        completion.update("/");
        completion.dismiss();

        assert_eq!(
            completion.handle_key(CompletionKey::Choose),
            CompletionAction::Ignored
        );
        assert_eq!(
            completion.handle_key(CompletionKey::Cancel),
            CompletionAction::Ignored
        );
    }
}

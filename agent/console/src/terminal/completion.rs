#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CommandGroup {
    Conversation,
    ModelAndSettings,
    ToolsAndPermissions,
    PlanAndExecution,
    Advanced,
    Skills,
}

impl CommandGroup {
    pub(super) fn title(self) -> &'static str {
        match self {
            Self::Conversation => "Conversation",
            Self::ModelAndSettings => "Model & settings",
            Self::ToolsAndPermissions => "Tools & permissions",
            Self::PlanAndExecution => "Plan & execution",
            Self::Advanced => "Advanced commands",
            Self::Skills => "Skills",
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

/// Where a typed command would act: a root Agent, a delegated child, or the overview, which has
/// no Agent of its own and dispatches Skills to new Agents.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum CommandScope {
    #[default]
    Root,
    Child,
    Overview,
}

/// Commands the overview handles itself, with what they do there.
const OVERVIEW_COMMANDS: &[(&str, &str)] = &[
    (
        "/acknowledge-create",
        "Stops tracking this overview's unconfirmed New Agent dispatch; nothing is resent.",
    ),
    ("/clear", "Leaves the overview for a new Agent draft."),
    ("/cwd", "Shows or sets the directory for new Agents."),
    ("/exit", "Leaves this View; Agents keep running."),
    ("/help", "Shows overview keys and commands."),
    ("/resume", "Opens saved Agent history."),
    ("/tickloop", "Starts a new Agent with the Tick Loop Skill."),
];

const OVERVIEW_AGENT_COMMAND: &str =
    "acts on an open Agent; enter opens the selected Agent, /clear starts a new draft";
pub(super) const CHILD_HISTORY_FIXED: &str = "its history belongs to the delegation";
pub(super) const CHILD_AUTHORIZATION_FIXED: &str = "its authorization is set by the parent";

pub(super) fn command_refusal(scope: CommandScope, command: &str, reason: &str) -> String {
    let location = if scope == CommandScope::Child {
        "in a child Agent"
    } else {
        "here"
    };
    format!("{command} isn't available {location}: {reason}")
}

impl CommandSpec {
    /// Why this command cannot act in `scope`, when it cannot; the daemon still validates the rest.
    pub(super) fn unavailable_in(&self, scope: CommandScope) -> Option<&'static str> {
        match scope {
            CommandScope::Root => None,
            CommandScope::Overview => (!OVERVIEW_COMMANDS
                .iter()
                .any(|(command, _)| *command == self.command))
            .then_some(OVERVIEW_AGENT_COMMAND),
            CommandScope::Child => match self.command {
                "/clear" | "/branch" | "/rewind" | "/compact" => Some(CHILD_HISTORY_FIXED),
                "/mode" | "/rule" | "/add-dir" | "/auto-mode-setup" => {
                    Some(CHILD_AUTHORIZATION_FIXED)
                }
                _ => None,
            },
        }
    }

    pub(super) fn description_in(&self, scope: CommandScope) -> String {
        if scope == CommandScope::Child && self.command == "/plan" {
            return "Reopens a pending approval or views a saved plan; entering Plan mode is fixed by parent.".into();
        }
        if scope == CommandScope::Child && self.command == "/permissions" {
            return "Shows current mode and recent activity; child permissions are read-only."
                .into();
        }
        if scope == CommandScope::Child && self.command == "/cwd" {
            return format!(
                "Shows the working directory; setting one is unavailable: {CHILD_AUTHORIZATION_FIXED}."
            );
        }
        if scope == CommandScope::Overview
            && let Some((_, description)) = OVERVIEW_COMMANDS
                .iter()
                .find(|(command, _)| *command == self.command)
        {
            return (*description).into();
        }
        match self.unavailable_in(scope) {
            Some(reason) => format!("Unavailable: {reason}."),
            None => self.description.into(),
        }
    }

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
        command: "/monitor",
        arguments: "[add]",
        description: "Show or hide Monitor; add lists Ticks removed here and, in Herdr, Workspace programs.",
        menu_group: Some(CommandGroup::ToolsAndPermissions),
    },
    CommandSpec {
        command: "/status",
        arguments: "",
        description: "Show the selected Chat, model, execution mode and queue.",
        menu_group: Some(CommandGroup::Advanced),
    },
    CommandSpec {
        command: "/tickloop",
        arguments: "[REQUEST]",
        description: "Loads the Tick Loop Skill.",
        menu_group: Some(CommandGroup::Skills),
    },
    CommandSpec {
        command: "/artifacts",
        arguments: "",
        description: "Choose artifacts to add to this Chat; sign in when needed.",
        menu_group: Some(CommandGroup::ToolsAndPermissions),
    },
    CommandSpec {
        command: "/clear",
        arguments: "",
        description: "Starts a New Agent from your next input; this Agent keeps running.",
        menu_group: Some(CommandGroup::Conversation),
    },
    CommandSpec {
        command: "/branch",
        arguments: "[NAME]",
        description: "Branches this conversation at the current point.",
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
        arguments: "[ACTION|GOAL_ID]",
        description: "Manage goals, their triggers and local timers in this Chat.",
        menu_group: Some(CommandGroup::Conversation),
    },
    CommandSpec {
        command: "/rewind",
        arguments: "",
        description: "Opens the Rewind checkpoint selector.",
        menu_group: Some(CommandGroup::Conversation),
    },
    CommandSpec {
        command: "/model",
        arguments: "",
        description: "Choose built-in models; p opens connection details, f switches connection filters.",
        menu_group: Some(CommandGroup::ModelAndSettings),
    },
    CommandSpec {
        command: "/provider",
        arguments: "",
        description: "Browse connections and models; p manages credentials in connection details.",
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
        command: "/tui",
        arguments: "[inline|fullscreen]",
        description: "Shows or switches the terminal renderer.",
        menu_group: Some(CommandGroup::Advanced),
    },
    CommandSpec {
        command: "/cwd",
        arguments: "[PATH]",
        description: "Shows or sets the main working directory.",
        menu_group: Some(CommandGroup::ModelAndSettings),
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
        menu_group: Some(CommandGroup::Advanced),
    },
    CommandSpec {
        command: "/auto-mode-setup",
        arguments: "",
        description: "Generate and review directory-scoped Auto Environment facts.",
        menu_group: Some(CommandGroup::ToolsAndPermissions),
    },
    CommandSpec {
        command: "/permissions",
        arguments: "",
        description: "Choose Auto, Ask or Full Access; configure Auto and review recent decisions.",
        menu_group: Some(CommandGroup::ToolsAndPermissions),
    },
    CommandSpec {
        command: "/plan",
        arguments: "[GOAL_ID]",
        description: "Enter Plan mode and review the selected plan; with an ID, view its saved plan.",
        menu_group: Some(CommandGroup::PlanAndExecution),
    },
    CommandSpec {
        command: "/compact",
        arguments: "[INSTRUCTIONS]",
        description: "Summarizes context behind a compaction boundary.",
        menu_group: Some(CommandGroup::PlanAndExecution),
    },
    CommandSpec {
        command: "/interrupt",
        arguments: "",
        description: "Interrupts the current run.",
        menu_group: Some(CommandGroup::Advanced),
    },
    CommandSpec {
        command: "/stop",
        arguments: "[GOAL]",
        description: "Stops a goal.",
        menu_group: Some(CommandGroup::PlanAndExecution),
    },
    CommandSpec {
        command: "/answer",
        arguments: "QUESTION ANSWER",
        description: "Replies to a clarification question.",
        menu_group: Some(CommandGroup::Advanced),
    },
    CommandSpec {
        command: "/mode",
        arguments: "[auto|ask|fullAccess|plan]",
        description: "Changes execution mode; use /plan to plan a goal.",
        menu_group: Some(CommandGroup::Advanced),
    },
    CommandSpec {
        command: "/allow",
        arguments: "REQUEST [once|session|persistent]",
        description: "Approves a permission request with an optional scope.",
        menu_group: Some(CommandGroup::Advanced),
    },
    CommandSpec {
        command: "/deny",
        arguments: "REQUEST",
        description: "Denies a permission request.",
        menu_group: Some(CommandGroup::Advanced),
    },
    CommandSpec {
        command: "/rule",
        arguments: "allow|ask|deny TOOL [session|persistent] | remove UUID",
        description: "Adds or removes a permission rule.",
        menu_group: Some(CommandGroup::Advanced),
    },
    CommandSpec {
        command: "/add-dir",
        arguments: "PATH",
        description: "Adds an allowed working directory.",
        menu_group: Some(CommandGroup::Advanced),
    },
    CommandSpec {
        command: "/search-config",
        arguments: "FILE|off",
        description: "Configures or disables web search.",
        menu_group: Some(CommandGroup::Advanced),
    },
    CommandSpec {
        command: "/background",
        arguments: "OPERATION",
        description: "Moves a Shell process to the background.",
        menu_group: Some(CommandGroup::Advanced),
    },
    CommandSpec {
        command: "/command-output",
        arguments: "OPERATION [OFFSET] [LIMIT]",
        description: "Reads a bounded range of Shell process output.",
        menu_group: Some(CommandGroup::Advanced),
    },
    CommandSpec {
        command: "/stop-command",
        arguments: "OPERATION",
        description: "Stops a Shell process.",
        menu_group: Some(CommandGroup::Advanced),
    },
    CommandSpec {
        command: "/operations",
        arguments: "",
        description: "Shows effects recorded in this process and their outcomes.",
        menu_group: Some(CommandGroup::Advanced),
    },
    CommandSpec {
        command: "/events",
        arguments: "PROMPT",
        description: "Enables explicit events for the selected goal.",
        menu_group: Some(CommandGroup::Advanced),
    },
    CommandSpec {
        command: "/trigger",
        arguments: "SOURCE [JSON]",
        description: "Delivers an event to the selected goal.",
        menu_group: Some(CommandGroup::Advanced),
    },
    CommandSpec {
        command: "/clear-trigger",
        arguments: "",
        description: "Removes the selected goal's trigger.",
        menu_group: Some(CommandGroup::Advanced),
    },
    CommandSpec {
        command: "/acknowledge-input",
        arguments: "COMMAND_ID",
        description: "Stops waiting on an input whose acceptance is unknown; nothing is resent.",
        menu_group: Some(CommandGroup::Advanced),
    },
    CommandSpec {
        command: "/acknowledge-create",
        arguments: "COMMAND_ID",
        description: "Stops tracking an unknown or failed New Agent command; nothing is resent.",
        menu_group: Some(CommandGroup::Advanced),
    },
    CommandSpec {
        command: "/help",
        arguments: "[advanced]",
        description: "Show public commands and Skills, or the advanced reference.",
        menu_group: Some(CommandGroup::Conversation),
    },
    CommandSpec {
        command: "/exit",
        arguments: "",
        description: "Leaves this View; the Agent keeps running.",
        menu_group: Some(CommandGroup::Conversation),
    },
];

pub(super) fn builtin_commands() -> &'static [CommandSpec] {
    COMMANDS
}

/// The built-in command a submitted line names, and why it cannot act in `scope`.
pub(super) fn unavailable_command(
    line: &str,
    scope: CommandScope,
) -> Option<(&'static str, &'static str)> {
    let mut words = line.split_whitespace();
    let name = words.next()?;
    if scope == CommandScope::Child && name == "/cwd" && words.next().is_some() {
        return Some(("/cwd PATH", CHILD_AUTHORIZATION_FIXED));
    }
    COMMANDS
        .iter()
        .find(|spec| spec.command == name)
        .and_then(|spec| Some((spec.command, spec.unavailable_in(scope)?)))
}

pub(super) fn command_help(advanced: bool) -> String {
    COMMANDS
        .iter()
        .filter(|command| (command.menu_group == Some(CommandGroup::Advanced)) == advanced)
        .map(|command| format!("{} — {}", command.syntax(), command.description))
        .collect::<Vec<_>>()
        .join("\n")
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
    scope: CommandScope,
    matches: Vec<usize>,
    selected: Option<usize>,
    draft: String,
    token_start: usize,
    token_end: usize,
}

impl Completion {
    #[cfg(test)]
    fn update(&mut self, draft: &str) {
        self.update_at(draft, draft.len(), CommandScope::Root);
    }

    pub(super) fn update_at(&mut self, draft: &str, cursor: usize, scope: CommandScope) {
        self.dismiss();
        self.scope = scope;

        let Some((token_start, token_end)) = command_token(draft, cursor) else {
            return;
        };

        let query = &draft[token_start + 1..cursor];

        self.matches.extend(
            COMMANDS
                .iter()
                .enumerate()
                .filter(|(_, spec)| {
                    spec.menu_group != Some(CommandGroup::Advanced)
                        && spec.command[1..].starts_with(query)
                        && (scope != CommandScope::Overview || spec.unavailable_in(scope).is_none())
                })
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

    /// Candidate syntax with the description that holds where the command would act.
    pub(super) fn described_candidates(&self) -> Vec<(String, String)> {
        self.candidates()
            .map(|spec| (spec.syntax(), spec.description_in(self.scope)))
            .collect()
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
    fn discovery_separates_public_commands_advanced_controls_and_skills() {
        let mut completion = Completion::default();
        completion.update("/");
        assert_eq!(completion.candidates().len(), 22);
        assert_eq!(builtin_commands().len(), 42);
        assert_eq!(
            builtin_commands()
                .iter()
                .filter(|spec| spec.menu_group == Some(CommandGroup::Advanced))
                .count(),
            20
        );
        assert_eq!(completion.selected_index(), None);
        for command in ["/help", "/cwd", "/details", "/tickloop", "/artifacts"] {
            assert!(command_names(&completion).contains(&command));
        }
        for command in ["/tui", "/allow", "/operations", "/interrupt"] {
            assert!(!command_names(&completion).contains(&command));
            assert!(
                builtin_commands()
                    .iter()
                    .any(|spec| spec.command == command)
            );
        }
        let unique: std::collections::HashSet<_> =
            builtin_commands().iter().map(|spec| spec.command).collect();
        assert_eq!(unique.len(), builtin_commands().len());
    }

    #[test]
    fn retired_names_are_neither_registered_nor_completed() {
        for command in [
            "/new",
            "/goal",
            "/theme",
            "/logout",
            "/triggers",
            "/schedule",
            "/checkpoint",
            "/undo",
            "/effort",
            "/config",
            "/jev",
            "/commands",
            "/bashes",
            "/subs",
            "/tasks",
            "/subscriptions",
            "/quit",
        ] {
            assert!(
                !builtin_commands()
                    .iter()
                    .any(|spec| spec.command == command)
            );
            let mut completion = Completion::default();
            completion.update(command);
            assert!(!command_names(&completion).contains(&command));
        }
    }

    #[test]
    fn only_public_prefix_matches_are_returned() {
        let mut completion = Completion::default();
        completion.update("/s");
        assert_eq!(command_names(&completion), vec!["/settings", "/stop"]);
        completion.update("/operations");
        assert!(!completion.is_open());
    }

    #[test]
    fn rewind_completes_without_arguments() {
        let mut completion = Completion::default();
        completion.update("/rew");
        assert_eq!(command_names(&completion), vec!["/rewind"]);
        assert_eq!(
            completion.handle_key(CompletionKey::Complete),
            CompletionAction::ReplaceDraft {
                draft: "/rewind".into(),
                cursor: 7,
            }
        );
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

        assert_eq!(command_names(&completion), ["/stop"]);
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
        completion.update("/mod");

        assert_eq!(command_names(&completion), ["/model"]);
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
        assert_eq!(completion.selected_index(), Some(0));
        assert_eq!(
            completion.handle_key(CompletionKey::Choose),
            CompletionAction::ReplaceDraft {
                draft: "/model".into(),
                cursor: "/model".len(),
            }
        );
        assert!(!completion.is_open());
    }

    #[test]
    fn enter_without_a_selection_leaves_submission_to_the_editor() {
        let mut completion = Completion::default();
        completion.update("/mod");

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
        completion.update("/thi");

        assert_eq!(
            completion.handle_key(CompletionKey::Complete),
            CompletionAction::ReplaceDraft {
                draft: "/thinking ".into(),
                cursor: "/thinking ".len(),
            }
        );

        completion.update("/hel");
        assert_eq!(
            completion.handle_key(CompletionKey::Complete),
            CompletionAction::ReplaceDraft {
                draft: "/help ".into(),
                cursor: "/help ".len(),
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
        let draft = "研究一下\u{3000}/mod 保留后缀";
        let cursor = "研究一下\u{3000}/mod".len();
        let mut completion = Completion::default();
        completion.update_at(draft, cursor, CommandScope::Root);
        completion.handle_key(CompletionKey::Next);
        completion.handle_key(CompletionKey::Next);

        assert_eq!(
            completion.handle_key(CompletionKey::Choose),
            CompletionAction::ReplaceDraft {
                draft: "研究一下\u{3000}/model 保留后缀".into(),
                cursor: "研究一下\u{3000}/model".len(),
            }
        );
    }

    #[test]
    fn completion_in_the_middle_replaces_the_whole_token_and_preserves_the_suffix() {
        let draft = "before /modelish after";
        let cursor = "before /mod".len();
        let mut completion = Completion::default();
        completion.update_at(draft, cursor, CommandScope::Root);
        completion.handle_key(CompletionKey::Next);
        completion.handle_key(CompletionKey::Next);

        assert_eq!(
            completion.handle_key(CompletionKey::Complete),
            CompletionAction::ReplaceDraft {
                draft: "before /model after".into(),
                cursor: "before /model".len(),
            }
        );
    }

    #[test]
    fn line_start_completion_preserves_multiline_text() {
        let draft = "第一行\n/mod\n第二行";
        let cursor = "第一行\n/mod".len();
        let mut completion = Completion::default();
        completion.update_at(draft, cursor, CommandScope::Root);
        completion.handle_key(CompletionKey::Next);
        completion.handle_key(CompletionKey::Next);

        assert_eq!(
            completion.handle_key(CompletionKey::Choose),
            CompletionAction::ReplaceDraft {
                draft: "第一行\n/model\n第二行".into(),
                cursor: "第一行\n/model".len(),
            }
        );
    }

    #[test]
    fn slash_inside_a_token_and_invalid_utf8_cursor_do_not_open_completion() {
        let mut completion = Completion::default();
        completion.update("prefix/mo");
        assert!(!completion.is_open());

        completion.update_at("中/mo", 1, CommandScope::Root);
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

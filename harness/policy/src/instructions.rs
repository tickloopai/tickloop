//! Instruction files loaded into a conversation, and the user context message that carries them
//! with the current date, worded as Claude Code words its user context.

use std::path::PathBuf;

const INSTRUCTIONS_PREAMBLE: &str = "Codebase and user instructions are shown below. Be sure to adhere to these instructions. IMPORTANT: These instructions OVERRIDE any default behavior and you MUST follow them exactly as written.";
/// How the user context message begins, which tells it apart from the user's own input.
pub const USER_CONTEXT_OPENING: &str =
    "<system-reminder>\nAs you answer the user's questions, you can use the following context:\n";
const INSTRUCTIONS_CONTEXT_KEY: &str = "claudeMd";
const CURRENT_DATE_CONTEXT_KEY: &str = "currentDate";
const CONTEXT_RELEVANCE_NOTE: &str = "IMPORTANT: this context may or may not be relevant to your tasks. You should not respond to this context unless it is highly relevant to your task.";

/// Claude Code's `date_change` reminder, appended to the conversation when the user's date moved
/// on, so the user context that told an earlier date stays the conversation's unchanged prefix.
pub fn date_change_message(current_date: &str) -> String {
    format!(
        "<system-reminder>\nThe date has changed. Today's date is now {current_date}. DO NOT mention this to the user explicitly because they are already aware.\n</system-reminder>"
    )
}

/// Where an instruction file comes from; an imported file has the scope of the file importing it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InstructionScope {
    /// The user's own instructions for every project.
    User,
    /// Instructions checked into a project directory or one of its ancestors.
    Project,
    /// `CLAUDE.local.md`: the user's private instructions for a directory, not checked in.
    Local,
}

impl InstructionScope {
    fn description(self) -> &'static str {
        match self {
            Self::User => " (user's private global instructions for all projects)",
            Self::Project => " (project instructions, checked into the codebase)",
            Self::Local => " (user's private project instructions, not checked in)",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstructionFile {
    pub path: PathBuf,
    pub scope: InstructionScope,
    pub content: String,
}

/// The context message placed before the conversation: the instruction files with content, in
/// load order with the lowest priority first, then `current_date` as `YYYY-MM-DD`.
pub fn user_context_message(files: &[InstructionFile], current_date: &str) -> String {
    let contents = files
        .iter()
        .filter_map(|file| {
            let content = file.content.trim();
            (!content.is_empty()).then(|| {
                format!(
                    "Contents of {}{}:\n\n{content}",
                    file.path.display(),
                    file.scope.description()
                )
            })
        })
        .collect::<Vec<_>>();
    let mut entries = Vec::new();
    if !contents.is_empty() {
        entries.push(format!(
            "# {INSTRUCTIONS_CONTEXT_KEY}\n{INSTRUCTIONS_PREAMBLE}\n\n{}",
            contents.join("\n\n")
        ));
    }
    entries.push(format!(
        "# {CURRENT_DATE_CONTEXT_KEY}\nToday's date is {current_date}."
    ));
    format!(
        "{USER_CONTEXT_OPENING}{}\n\n      {CONTEXT_RELEVANCE_NOTE}\n</system-reminder>\n",
        entries.join("\n")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, scope: InstructionScope, content: &str) -> InstructionFile {
        InstructionFile {
            path: PathBuf::from(path),
            scope,
            content: content.into(),
        }
    }

    #[test]
    fn the_message_matches_the_user_context_bytes() {
        let message = user_context_message(
            &[
                file(
                    "/home/u/.tickloop/AGENTS.md",
                    InstructionScope::User,
                    "Reply in English.\n",
                ),
                file(
                    "/repo/AGENTS.md",
                    InstructionScope::Project,
                    "  Run make.  ",
                ),
                file("/repo/CLAUDE.local.md", InstructionScope::Local, "Mine."),
            ],
            "2026-10-26",
        );
        assert_eq!(
            message,
            "<system-reminder>\nAs you answer the user's questions, you can use the following context:\n# claudeMd\nCodebase and user instructions are shown below. Be sure to adhere to these instructions. IMPORTANT: These instructions OVERRIDE any default behavior and you MUST follow them exactly as written.\n\nContents of /home/u/.tickloop/AGENTS.md (user's private global instructions for all projects):\n\nReply in English.\n\nContents of /repo/AGENTS.md (project instructions, checked into the codebase):\n\nRun make.\n\nContents of /repo/CLAUDE.local.md (user's private project instructions, not checked in):\n\nMine.\n# currentDate\nToday's date is 2026-10-26.\n\n      IMPORTANT: this context may or may not be relevant to your tasks. You should not respond to this context unless it is highly relevant to your task.\n</system-reminder>\n"
        );
    }

    #[test]
    fn without_instructions_the_message_carries_only_the_date() {
        let expected = "<system-reminder>\nAs you answer the user's questions, you can use the following context:\n# currentDate\nToday's date is 2026-10-26.\n\n      IMPORTANT: this context may or may not be relevant to your tasks. You should not respond to this context unless it is highly relevant to your task.\n</system-reminder>\n";
        assert_eq!(user_context_message(&[], "2026-10-26"), expected);
        assert_eq!(
            user_context_message(
                &[file("/repo/AGENTS.md", InstructionScope::Project, " \n")],
                "2026-10-26"
            ),
            expected
        );
    }
}

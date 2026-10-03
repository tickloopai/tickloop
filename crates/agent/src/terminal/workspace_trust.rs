use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use uuid::Uuid;

use super::{
    console::wrap_text,
    display::{DisplayProfile, Glyph},
    safe_line,
    style::{Role, RoleLine},
};

const CHOICES: [&str; 2] = ["No, exit", "Yes, I trust this folder"];
pub(super) const TRUST_CANCEL_CHOICE_INDEX: usize = 0;
pub(super) const TRUST_ACCEPT_CHOICE_INDEX: usize = 1;
const MIN_TRUST_COLUMNS: usize = 2;
const RULE_MIN_HEIGHT: usize = 16;

pub(super) struct WorkspaceTrustFrame {
    pub(super) lines: Vec<RoleLine>,
    pub(super) cursor: Option<(usize, usize)>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TrustDecision {
    Accept,
    Cancel,
}

pub(super) struct WorkspaceTrust {
    pub(super) id: Uuid,
    workdir: PathBuf,
    selected: usize,
    decision: Option<TrustDecision>,
    error: Option<String>,
}

impl WorkspaceTrust {
    pub(super) fn new(workdir: PathBuf) -> Self {
        Self {
            id: Uuid::new_v4(),
            workdir,
            selected: TRUST_CANCEL_CHOICE_INDEX,
            decision: None,
            error: None,
        }
    }

    pub(super) fn key(
        &mut self,
        key: KeyEvent,
        width: usize,
        height: usize,
        display: DisplayProfile,
    ) -> Option<TrustDecision> {
        if key.kind == KeyEventKind::Release || self.decision.is_some() {
            return None;
        }
        let decision = match (key.code, key.modifiers) {
            (KeyCode::Down | KeyCode::Char('j'), KeyModifiers::NONE)
            | (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
                self.selected = TRUST_ACCEPT_CHOICE_INDEX;
                None
            }
            (KeyCode::Up | KeyCode::Char('k'), KeyModifiers::NONE)
            | (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
                self.selected = TRUST_CANCEL_CHOICE_INDEX;
                None
            }
            (KeyCode::Enter, KeyModifiers::NONE) if self.selected == TRUST_CANCEL_CHOICE_INDEX => {
                Some(TrustDecision::Cancel)
            }
            (KeyCode::Enter, KeyModifiers::NONE)
                if self.frame(width, height, display).cursor.is_some() =>
            {
                Some(TrustDecision::Accept)
            }
            (KeyCode::Esc, _) | (KeyCode::Char('c' | 'd'), KeyModifiers::CONTROL) => {
                Some(TrustDecision::Cancel)
            }
            _ => None,
        };
        self.decision = decision;
        decision
    }

    pub(super) fn failed(&mut self, error: String) {
        self.error = Some(safe_line(&error));
        self.selected = TRUST_CANCEL_CHOICE_INDEX;
        self.decision = None;
    }

    pub(super) fn frame(
        &self,
        width: usize,
        height: usize,
        display: DisplayProfile,
    ) -> WorkspaceTrustFrame {
        let wrap_width = width.max(1);
        let mut heading: Vec<_> = wrap_text("Accessing workspace:", wrap_width)
            .into_iter()
            .map(|line| RoleLine::role(line, Role::DialogTitle))
            .collect();
        heading.extend(
            wrap_text(&safe_line(&self.workdir.to_string_lossy()), wrap_width)
                .into_iter()
                .map(|line| RoleLine::role(line, Role::Strong)),
        );
        let mut actions = Vec::new();
        let mut selected_row = 0;
        for (index, choice) in CHOICES.iter().enumerate() {
            if self.selected == index {
                selected_row = actions.len();
            }
            actions.extend(
                wrap_text(
                    &format!(
                        "{} {choice}",
                        if self.selected == index {
                            display.glyph(Glyph::Prompt)
                        } else {
                            " "
                        }
                    ),
                    wrap_width,
                )
                .into_iter()
                .map(|line| {
                    RoleLine::role(
                        line,
                        if self.selected == index {
                            Role::Suggestion
                        } else {
                            Role::Text
                        },
                    )
                }),
            );
        }
        actions.extend(
            wrap_text(
                match self.decision {
                    Some(TrustDecision::Accept) => "Preparing Agent…",
                    Some(TrustDecision::Cancel) => "Closing…",
                    None => "Enter confirm · Esc cancel",
                },
                wrap_width,
            )
            .into_iter()
            .map(|line| RoleLine::role(line, Role::MutedText)),
        );
        let error_lines: Vec<_> = self
            .error
            .as_ref()
            .into_iter()
            .flat_map(|error| wrap_text(error, wrap_width))
            .map(|line| RoleLine::role(line, Role::Error))
            .collect();
        let required_rows = heading.len() + error_lines.len() + actions.len();
        if width < MIN_TRUST_COLUMNS || required_rows > height {
            let mut lines: Vec<_> = wrap_text(
                "Resize terminal to review directory · Esc cancel",
                wrap_width,
            )
            .into_iter()
            .map(|line| RoleLine::role(line, Role::MutedText))
            .collect();
            lines.extend(error_lines);
            lines.truncate(height);
            for line in &mut lines {
                line.truncate(width);
            }
            return WorkspaceTrustFrame {
                lines,
                cursor: None,
            };
        }
        let mut body = Vec::new();
        let mut remaining_rows = height - required_rows;
        if height >= RULE_MIN_HEIGHT && remaining_rows > 0 {
            body.push(RoleLine::role("─".repeat(width), Role::Accent));
            remaining_rows -= 1;
        }
        body.extend(heading);
        body.extend(error_lines);
        let mut explanation = Vec::new();
        explanation.push(RoleLine::default());
        for text in [
            "Quick safety check: Is this a project you created or a folder you trust? If you're unsure, review its contents before continuing.",
            "Tick Loop Agent can read and edit files and run commands here, subject to your execution permissions.",
        ] {
            explanation.extend(
                wrap_text(text, width)
                    .into_iter()
                    .map(|line| RoleLine::role(line, Role::Text)),
            );
            explanation.push(RoleLine::default());
        }
        explanation.truncate(remaining_rows);
        body.extend(explanation);
        let cursor = self
            .decision
            .is_none()
            .then_some((body.len() + selected_row, 0));
        body.extend(actions);
        for line in &mut body {
            line.truncate(width);
        }
        WorkspaceTrustFrame {
            lines: body,
            cursor,
        }
    }
}

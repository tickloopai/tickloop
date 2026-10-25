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
const WORKSPACE_TRUST_TITLE: &str = "Accessing workspace:";

pub(super) struct WorkspaceTrustFrame {
    pub(super) lines: Vec<RoleLine>,
    pub(super) cursor: Option<(usize, usize)>,
    choice_rows: Vec<(std::ops::Range<usize>, usize)>,
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
            id: tickloop_ids::generate(),
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

    pub(super) fn click(
        &mut self,
        row: usize,
        width: usize,
        height: usize,
        display: DisplayProfile,
    ) -> Option<TrustDecision> {
        if self.decision.is_some() {
            return None;
        }
        let frame = self.frame(width, height, display);
        let selected = frame
            .choice_rows
            .iter()
            .find(|(rows, _)| rows.contains(&row))
            .map(|(_, index)| *index)?;
        self.selected = selected;
        self.key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            width,
            height,
            display,
        )
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
        let sanitized_path = safe_line(&self.workdir.to_string_lossy());
        let mut heading: Vec<_> = wrap_text(WORKSPACE_TRUST_TITLE, wrap_width)
            .into_iter()
            .map(|line| RoleLine::role(line, Role::DialogTitle))
            .collect();
        heading.extend(
            wrap_text(&sanitized_path, wrap_width)
                .into_iter()
                .map(|line| RoleLine::role(line, Role::Strong)),
        );
        let mut actions = Vec::new();
        let mut selected_row = 0;
        let mut choice_rows = Vec::new();
        for (index, choice) in CHOICES.iter().enumerate() {
            let first_row = actions.len();
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
            choice_rows.push((first_row..actions.len(), index));
        }
        actions.extend(
            wrap_text(
                match self.decision {
                    Some(TrustDecision::Accept) => "Preparing Agent…",
                    Some(TrustDecision::Cancel) => "Closing…",
                    None => "enter confirm · esc cancel",
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
        let mut required_rows = heading.len() + error_lines.len() + actions.len();
        let compact_heading = format!("{WORKSPACE_TRUST_TITLE} {sanitized_path}");
        if required_rows > height && wrap_text(&compact_heading, wrap_width).len() == 1 {
            let mut line = RoleLine::role(WORKSPACE_TRUST_TITLE, Role::DialogTitle);
            line.push(" ", None);
            line.push(&sanitized_path, Some(Role::Strong));
            heading = vec![line];
            required_rows = heading.len() + error_lines.len() + actions.len();
        }
        if width < MIN_TRUST_COLUMNS || required_rows > height {
            let mut lines = error_lines;
            lines.extend(
                wrap_text(
                    "Resize terminal to review directory · esc cancel",
                    wrap_width,
                )
                .into_iter()
                .map(|line| RoleLine::role(line, Role::MutedText)),
            );
            lines.truncate(height);
            for line in &mut lines {
                line.truncate(width);
            }
            return WorkspaceTrustFrame {
                lines,
                cursor: None,
                choice_rows: Vec::new(),
            };
        }
        let mut body = Vec::new();
        let remaining_rows = height - required_rows;
        body.extend(heading);
        body.extend(error_lines);
        let mut explanation = Vec::new();
        explanation.push(RoleLine::default());
        for text in [
            "Quick safety check: Is this a project you created or a folder you trust? If you're unsure, review its contents before continuing.",
            "Tickloop can read and edit files and run commands here, subject to your execution permissions.",
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
        let action_start = body.len();
        for (rows, _) in &mut choice_rows {
            rows.start += action_start;
            rows.end += action_start;
        }
        body.extend(actions);
        for line in &mut body {
            line.truncate(width);
        }
        WorkspaceTrustFrame {
            lines: body,
            cursor,
            choice_rows,
        }
    }
}

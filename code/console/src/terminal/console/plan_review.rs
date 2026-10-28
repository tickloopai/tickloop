//! Keys of the plan approval dialog beyond choosing an option, after Claude Code's plan dialog:
//! Ctrl+G edits the plan under review in the external editor, and on the feedback field Shift+Tab
//! approves with that feedback while Enter needs feedback to keep planning.

use super::*;

const FEEDBACK_HINT_OPENING: &str = "Type feedback · enter keep planning · ctrl+g editor";

/// What the visible plan approval dialog offers besides its options.
pub(in super::super) struct PlanReview {
    pub(in super::super) menu_id: Uuid,
    /// The plan under review, as the editor opens it.
    pub(in super::super) markdown: String,
    /// The option Shift+Tab approves with from the feedback field.
    pub(in super::super) feedback_approval: usize,
}

impl Console {
    /// Offers plan editing and approval with feedback in the plan approval dialog `menu_id`.
    pub(in super::super) fn offer_plan_review(&self, review: PlanReview) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        state.plan_review = Some(review);
        state
            .draw(&mut io::stdout().lock())
            .map_err(|error| error.to_string())
    }

    /// Opens a plan in the external editor once the input thread owns the terminal.
    pub(in super::super) fn request_plan_edit(
        &self,
        edit: RequestedPlanEdit,
    ) -> Result<(), String> {
        self.state
            .lock()
            .map_err(|error| error.to_string())?
            .requested_plan_edit = Some(edit);
        Ok(())
    }
}

/// A plan edit `/plan open` asks the input thread for.
pub(in super::super) enum RequestedPlanEdit {
    /// The plan in the open approval dialog, as Ctrl+G edits it there.
    Review,
    /// The plan file itself, when no plan awaits approval.
    File(PathBuf),
}

impl State {
    fn offered_plan_review(&self) -> Option<&PlanReview> {
        let review = self.plan_review.as_ref()?;
        (self.menu_plan && self.menu.as_ref()?.id == review.menu_id).then_some(review)
    }

    /// Handles a plan approval dialog key; `None` leaves the key to the menu.
    pub(super) fn plan_review_key(&mut self, key: KeyEvent) -> Option<Option<InputEvent>> {
        let review = self.offered_plan_review()?;
        let (menu_id, feedback_approval) = (review.menu_id, review.feedback_approval);
        if !self.menu_focused || !self.menu_is_visible() {
            return None;
        }
        let editing = self.menu_comment_is_editing();
        match (key.code, key.modifiers) {
            (KeyCode::Char('g'), KeyModifiers::CONTROL) if editing => {
                self.edit_plan_feedback();
                Some(None)
            }
            (KeyCode::Char('g'), KeyModifiers::CONTROL) => Some(self.edit_offered_plan()),
            (KeyCode::BackTab, _) if editing => {
                let comment = self.selected_menu_comment();
                self.begin_menu_submission(menu_id);
                Some(Some(InputEvent::Choice {
                    menu_id,
                    index: feedback_approval,
                    comment,
                }))
            }
            // Claude Code keeps the dialog open when "No, keep planning" has no feedback.
            (KeyCode::Enter, _) if editing && self.selected_menu_comment().is_none() => Some(None),
            _ => None,
        }
    }

    /// Runs the plan edit `/plan open` asked for; `None` when none was asked for.
    pub(super) fn take_requested_plan_edit(&mut self) -> Option<Option<InputEvent>> {
        match self.requested_plan_edit.take()? {
            RequestedPlanEdit::Review if self.offered_plan_review().is_none() => {
                self.status = "Open the plan review to edit the plan".into();
                Some(None)
            }
            RequestedPlanEdit::Review => Some(self.edit_offered_plan()),
            RequestedPlanEdit::File(path) => {
                match self.run_external_file_editor(&path) {
                    Some(ExternalEditorOutcome::Updated { .. }) => {
                        self.status = "Plan file saved".into();
                    }
                    Some(ExternalEditorOutcome::Unchanged) => {
                        self.status = "External editor left the plan file unchanged".into();
                    }
                    Some(outcome) => self.report_editor_outcome(outcome),
                    None => {}
                }
                Some(None)
            }
        }
    }

    fn edit_offered_plan(&mut self) -> Option<InputEvent> {
        let review = self.offered_plan_review()?;
        let (menu_id, markdown) = (review.menu_id, review.markdown.clone());
        match self.run_external_editor(&markdown)? {
            ExternalEditorOutcome::Updated { content } if content.trim().is_empty() => {
                self.status = "The edited plan is empty; the plan is unchanged".into();
                None
            }
            ExternalEditorOutcome::Updated { content } => {
                self.status = "Saving the edited plan…".into();
                Some(InputEvent::PlanEdited {
                    menu_id,
                    markdown: content,
                })
            }
            ExternalEditorOutcome::Unchanged => {
                self.status = "External editor left the plan unchanged".into();
                None
            }
            outcome => {
                self.report_editor_outcome(outcome);
                None
            }
        }
    }

    fn edit_plan_feedback(&mut self) {
        let Some(selected) = self.menu.as_ref().map(|menu| menu.selected) else {
            return;
        };
        let draft = self
            .menu_comment
            .as_ref()
            .and_then(|comment| comment.values.get(&selected))
            .cloned()
            .unwrap_or_default();
        match self.run_external_editor(&draft) {
            Some(ExternalEditorOutcome::Updated { content }) => {
                let comment = self.menu_comment.as_mut().unwrap();
                comment.cursor = content.len();
                comment.values.insert(selected, content);
                self.status = "Feedback updated by external editor".into();
            }
            Some(ExternalEditorOutcome::Unchanged) => {
                self.status = "External editor left the feedback unchanged".into();
            }
            Some(outcome) => self.report_editor_outcome(outcome),
            None => {}
        }
    }

    fn report_editor_outcome(&mut self, outcome: ExternalEditorOutcome) {
        self.status = match outcome {
            ExternalEditorOutcome::Cancelled { signal } => {
                format!("External editor cancelled · signal {signal:?}")
            }
            ExternalEditorOutcome::Unavailable => {
                "No external editor found; set VISUAL or EDITOR".into()
            }
            ExternalEditorOutcome::Failed(error) => format!("External editor failed: {error}"),
            ExternalEditorOutcome::Updated { .. } | ExternalEditorOutcome::Unchanged => return,
        };
    }

    /// The feedback field's key hint, naming the mode Shift+Tab approves with.
    pub(super) fn plan_feedback_hint(&self) -> Option<String> {
        let review = self.offered_plan_review()?;
        let option = self.menu.as_ref()?.options.get(review.feedback_approval)?;
        let mode = option.rsplit(" · ").next().unwrap_or(option);
        Some(format!(
            "{FEEDBACK_HINT_OPENING} · shift+tab approve in {mode} · esc close"
        ))
    }

    /// The key hint the plan approval dialog adds while its options have focus.
    pub(super) fn plan_review_hints(&self) -> Option<(String, u8)> {
        self.offered_plan_review()
            .map(|_| ("ctrl+g edit plan".into(), 10))
    }
}

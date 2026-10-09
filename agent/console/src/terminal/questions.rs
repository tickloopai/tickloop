//! AskUserQuestion answers in the terminal. The dialog steps through the questions like
//! Claude Code's `AskUserQuestionPermissionRequest`: one tab per question, an automatic
//! "Other" choice, multi-select toggles and a review step before submitting. Typed input
//! answers the questions one line at a time instead.

use uuid::Uuid;

use super::{
    console::{Menu, QuestionMenu},
    display::{DisplayProfile, Glyph},
    safe_line, safe_text,
};
use tickloop_agent_protocol::{Question, QuestionAnswer, QuestionRequest};

const OTHER_LABEL: &str = "Other";
const OTHER_PLACEHOLDER: &str = "Type something.";
const TYPED_OTHER_HINT: &str = "type your own answer";
const ANSWERED_CHIP: &str = "☒";
const UNANSWERED_CHIP: &str = "☐";
const REVIEW_TITLE: &str = "Review your answers";
const REVIEW_PROMPT: &str = "Ready to submit your answers?";
const SUBMIT_ANSWERS: &str = "Submit answers";
const ANSWER_LATER: &str = "Answer later";
const NEXT_QUESTION: &str = "Next";
const LAST_QUESTION: &str = "Submit";
const MULTI_SELECT_NOTE: &str = "(choose one or more)";
/// Option rows render as `❯ 1. label`; follow-up lines start under the label.
const OPTION_TEXT_INDENT: &str = "     ";
const CHECKBOX_INDENT: &str = "    ";
const PREVIEW_LINES: usize = 8;
const UNANSWERED_NOTICE: &str = "Answer this question before moving on";
const EMPTY_MULTI_SELECT_NOTICE: &str = "Choose at least one option or type an answer";
const INCOMPLETE_REVIEW_NOTICE: &str = "You have not answered all questions";
const EMPTY_TYPED_ANSWER: &str = "type an option number, an option label, or your own answer";

pub(super) enum FlowUpdate {
    /// Show the current step again.
    Show,
    Submit(Vec<QuestionAnswer>),
    /// Close the dialog; the question keeps waiting for a typed answer.
    Defer,
}

#[derive(Clone, Default)]
struct QuestionDraft {
    /// Chosen option indexes in the order they were chosen, as Claude Code reports them.
    options: Vec<usize>,
    /// Text typed for "Other"; kept while another option is chosen so it can be edited again.
    other_text: String,
    other_chosen: bool,
}

impl QuestionDraft {
    fn other(text: &str) -> Self {
        Self {
            options: Vec::new(),
            other_text: text.to_owned(),
            other_chosen: true,
        }
    }

    fn chosen_other(&self) -> Option<&str> {
        self.other_chosen
            .then(|| self.other_text.trim())
            .filter(|text| !text.is_empty())
    }

    fn is_answered(&self) -> bool {
        !self.options.is_empty() || self.chosen_other().is_some()
    }

    fn answer(&self, question: &Question) -> Option<QuestionAnswer> {
        self.is_answered().then(|| QuestionAnswer {
            options: self
                .options
                .iter()
                .map(|index| question.options[*index].label.clone())
                .collect(),
            other: self.chosen_other().map(str::to_owned),
        })
    }
}

pub(super) struct QuestionFlow {
    request: QuestionRequest,
    goal: String,
    drafts: Vec<QuestionDraft>,
    step: usize,
    focus: usize,
    notice: Option<&'static str>,
}

impl QuestionFlow {
    pub(super) fn new(request: QuestionRequest, goal: String) -> Self {
        let drafts = vec![QuestionDraft::default(); request.questions.len()];
        Self {
            request,
            goal,
            drafts,
            step: 0,
            focus: 0,
            notice: None,
        }
    }

    pub(super) fn request(&self) -> &QuestionRequest {
        &self.request
    }

    fn questions(&self) -> &[Question] {
        &self.request.questions
    }

    /// Claude Code hides the Submit tab only when one single-select question is answered
    /// with a single Enter.
    fn has_review(&self) -> bool {
        self.questions().len() > 1
            || self
                .questions()
                .iter()
                .any(|question| question.multi_select)
    }

    fn review_step(&self) -> usize {
        self.questions().len()
    }

    fn last_step(&self) -> usize {
        if self.has_review() {
            self.review_step()
        } else {
            self.review_step().saturating_sub(1)
        }
    }

    fn answers(&self) -> Option<Vec<QuestionAnswer>> {
        self.questions()
            .iter()
            .zip(&self.drafts)
            .map(|(question, draft)| draft.answer(question))
            .collect()
    }

    /// Applies a menu choice. `None` is Esc; the "Other" row carries its text as `comment`.
    pub(super) fn choose(&mut self, index: Option<usize>, comment: Option<String>) -> FlowUpdate {
        self.notice = None;
        let Some(index) = index else {
            return FlowUpdate::Defer;
        };
        if self.step == self.review_step() {
            if index != 0 {
                return FlowUpdate::Defer;
            }
            return match self.answers() {
                Some(answers) => FlowUpdate::Submit(answers),
                None => {
                    self.notice = Some(INCOMPLETE_REVIEW_NOTICE);
                    FlowUpdate::Show
                }
            };
        }
        self.focus = index;
        let question = &self.request.questions[self.step];
        let other_index = question.options.len();
        let multi_select = question.multi_select;
        let draft = &mut self.drafts[self.step];
        if index < other_index {
            if multi_select {
                match draft.options.iter().position(|chosen| *chosen == index) {
                    Some(position) => {
                        draft.options.remove(position);
                    }
                    None => draft.options.push(index),
                }
                return FlowUpdate::Show;
            }
            draft.options = vec![index];
            draft.other_chosen = false;
        } else if index == other_index {
            draft.other_text = comment.unwrap_or_default();
            draft.other_chosen = !draft.other_text.trim().is_empty();
            if multi_select || !draft.other_chosen {
                return FlowUpdate::Show;
            }
            draft.options.clear();
        } else if multi_select && index == other_index + 1 {
            if !draft.is_answered() {
                self.notice = Some(EMPTY_MULTI_SELECT_NOTICE);
                return FlowUpdate::Show;
            }
        } else {
            return FlowUpdate::Show;
        }
        if !self.has_review() {
            return self.answers().map_or(FlowUpdate::Show, FlowUpdate::Submit);
        }
        self.enter_step(self.step + 1);
        FlowUpdate::Show
    }

    /// Moves between question tabs. Moving forward needs an answer to the current question,
    /// because the Agent takes an answer to every question.
    pub(super) fn switch(&mut self, next: bool) {
        self.notice = None;
        if !next {
            if self.step > 0 {
                self.enter_step(self.step - 1);
            }
            return;
        }
        if self.step >= self.last_step() {
            return;
        }
        if !self.drafts[self.step].is_answered() {
            self.notice = Some(UNANSWERED_NOTICE);
            return;
        }
        self.enter_step(self.step + 1);
    }

    fn enter_step(&mut self, step: usize) {
        self.step = step;
        self.focus = match (self.questions().get(step), self.drafts.get(step)) {
            (Some(question), Some(draft)) if !question.multi_select => {
                if draft.chosen_other().is_some() {
                    question.options.len()
                } else {
                    draft.options.first().copied().unwrap_or(0)
                }
            }
            _ => 0,
        };
    }

    pub(super) fn menu(&self, display: DisplayProfile) -> (Menu, QuestionMenu) {
        let mut context = vec![self.tabs(display)];
        if !self.goal.is_empty() {
            context.push(safe_text(&format!("Goal: {}", self.goal)));
        }
        if let Some(notice) = self.notice {
            context.push(format!("{} {notice}", display.glyph(Glyph::Warning)));
        }
        if self.step == self.review_step() {
            for (question, draft) in self.questions().iter().zip(&self.drafts) {
                if let Some(answer) = draft.answer(question) {
                    context.push(format!("• {}", safe_line(&question.question)));
                    context.push(format!("  → {}", safe_line(&answer.text())));
                }
            }
            context.push(REVIEW_PROMPT.into());
            let menu = Menu {
                id: Uuid::new_v4(),
                title: REVIEW_TITLE.into(),
                context,
                options: vec![SUBMIT_ANSWERS.into(), ANSWER_LATER.into()],
                selected: self.focus.min(1),
                confirmation: false,
            };
            let keys = QuestionMenu {
                entry_option: None,
                entry_text: String::new(),
                multi_select: false,
                navigable: true,
            };
            return (menu, keys);
        }
        let question = &self.questions()[self.step];
        let draft = &self.drafts[self.step];
        let checkbox = |chosen: bool| match (question.multi_select, chosen) {
            (false, _) => String::new(),
            (true, true) => format!("[{}] ", display.glyph(Glyph::Success)),
            (true, false) => "[ ] ".into(),
        };
        let indent = if question.multi_select {
            format!("{OPTION_TEXT_INDENT}{CHECKBOX_INDENT}")
        } else {
            OPTION_TEXT_INDENT.to_owned()
        };
        let mut options = question
            .options
            .iter()
            .enumerate()
            .map(|(index, option)| {
                let mut row = format!(
                    "{}{}",
                    checkbox(draft.options.contains(&index)),
                    safe_line(&option.label)
                );
                append_indented(&mut row, &option.description, &indent, usize::MAX);
                if let Some(preview) = &option.preview {
                    append_indented(&mut row, preview, &indent, PREVIEW_LINES);
                }
                row
            })
            .collect::<Vec<_>>();
        let mut other_row = format!("{}{OTHER_LABEL}", checkbox(draft.chosen_other().is_some()));
        if draft.other_text.trim().is_empty() {
            append_indented(&mut other_row, OTHER_PLACEHOLDER, &indent, 1);
        }
        options.push(other_row);
        if question.multi_select {
            let is_last_question = self.step + 1 == self.questions().len();
            options.push(
                if is_last_question {
                    LAST_QUESTION
                } else {
                    NEXT_QUESTION
                }
                .into(),
            );
        }
        let menu = Menu {
            id: Uuid::new_v4(),
            title: safe_text(&question.question),
            context,
            selected: self.focus.min(options.len() - 1),
            options,
            confirmation: false,
        };
        let keys = QuestionMenu {
            entry_option: Some(question.options.len()),
            entry_text: draft.other_text.clone(),
            multi_select: question.multi_select,
            navigable: self.has_review(),
        };
        (menu, keys)
    }

    fn tabs(&self, display: DisplayProfile) -> String {
        let has_review = self.has_review();
        let mut chips = self
            .questions()
            .iter()
            .zip(&self.drafts)
            .enumerate()
            .map(|(index, (question, draft))| {
                let header = if question.header.trim().is_empty() {
                    format!("Q{}", index + 1)
                } else {
                    safe_line(&question.header)
                };
                let checkbox = if draft.is_answered() {
                    ANSWERED_CHIP
                } else {
                    UNANSWERED_CHIP
                };
                current_chip(
                    format!("{checkbox} {header}"),
                    has_review && index == self.step,
                )
            })
            .collect::<Vec<_>>();
        if !has_review {
            return chips.join("  ");
        }
        chips.push(current_chip(
            format!("{} {LAST_QUESTION}", display.glyph(Glyph::Success)),
            self.step == self.review_step(),
        ));
        format!("← {} →", chips.join("  "))
    }

    /// The first question still without an answer; typed input answers it next.
    fn next_unanswered(&self) -> Option<usize> {
        self.drafts.iter().position(|draft| !draft.is_answered())
    }

    /// Records typed text as the answer to the next unanswered question. Returns every answer
    /// once the last question has one.
    pub(super) fn answer_typed(
        &mut self,
        text: &str,
    ) -> Result<Option<Vec<QuestionAnswer>>, String> {
        if let Some(index) = self.next_unanswered() {
            self.drafts[index] = typed_draft(&self.request.questions[index], text)?;
        }
        Ok(self
            .next_unanswered()
            .is_none()
            .then(|| self.answers())
            .flatten())
    }

    /// Lines that ask the next unanswered question in plain text.
    pub(super) fn next_question_lines(&self) -> Vec<String> {
        self.next_unanswered()
            .map(|index| {
                question_lines(
                    &self.request.questions[index],
                    index,
                    self.questions().len(),
                )
            })
            .unwrap_or_default()
    }
}

fn current_chip(chip: String, is_current: bool) -> String {
    if is_current {
        format!("[{chip}]")
    } else {
        chip
    }
}

fn append_indented(row: &mut String, text: &str, indent: &str, max_lines: usize) {
    let text = safe_text(text);
    let mut lines = text.lines().filter(|line| !line.trim().is_empty());
    for line in lines.by_ref().take(max_lines) {
        row.push('\n');
        row.push_str(indent);
        row.push_str(line);
    }
    if lines.next().is_some() {
        row.push('\n');
        row.push_str(indent);
        row.push('…');
    }
}

/// Plain-text form of one question for typed answers: its position, header, text and the
/// numbered options, ending with the automatic "Other" choice.
pub(super) fn question_lines(question: &Question, index: usize, total: usize) -> Vec<String> {
    let mut heading = format!(
        "{}/{total} [{}] {}",
        index + 1,
        safe_line(&question.header),
        safe_line(&question.question)
    );
    if question.multi_select {
        heading.push(' ');
        heading.push_str(MULTI_SELECT_NOTE);
    }
    let mut lines = vec![heading];
    for (number, option) in (1..).zip(&question.options) {
        let mut line = format!("  {number}. {}", safe_line(&option.label));
        if !option.description.trim().is_empty() {
            line.push_str(&format!(" — {}", safe_line(&option.description)));
        }
        append_indented(
            &mut line,
            option.preview.as_deref().unwrap_or(""),
            OPTION_TEXT_INDENT,
            PREVIEW_LINES,
        );
        lines.extend(line.lines().map(str::to_owned));
    }
    lines.push(format!(
        "  {}. {OTHER_LABEL} — {TYPED_OTHER_HINT}",
        question.options.len() + 1
    ));
    lines
}

/// Reads a typed answer: an option label, an option number, comma-separated numbers or labels
/// for multi-select questions, and anything else as "Other" text.
fn typed_draft(question: &Question, text: &str) -> Result<QuestionDraft, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err(EMPTY_TYPED_ANSWER.into());
    }
    let option_index = |part: &str| {
        question
            .options
            .iter()
            .position(|option| option.label == part)
            .or_else(|| {
                question
                    .options
                    .iter()
                    .position(|option| option.label.eq_ignore_ascii_case(part))
            })
            .or_else(|| {
                part.parse::<usize>()
                    .ok()
                    .and_then(|number| number.checked_sub(1))
                    .filter(|index| *index < question.options.len())
            })
    };
    let parts = if question.multi_select && option_index(text).is_none() {
        text.split(',').map(str::trim).collect::<Vec<_>>()
    } else {
        vec![text]
    };
    let mut options = Vec::new();
    for part in parts {
        let Some(index) = option_index(part) else {
            return Ok(QuestionDraft::other(text));
        };
        if !options.contains(&index) {
            options.push(index);
        }
    }
    Ok(QuestionDraft {
        options,
        ..QuestionDraft::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tickloop_agent_protocol::QuestionOption;

    fn question(text: &str, header: &str, labels: &[&str], multi_select: bool) -> Question {
        Question {
            question: text.into(),
            header: header.into(),
            options: labels
                .iter()
                .map(|label| QuestionOption {
                    label: (*label).into(),
                    description: format!("About {label}"),
                    preview: None,
                })
                .collect(),
            multi_select,
        }
    }

    fn flow(questions: Vec<Question>) -> QuestionFlow {
        QuestionFlow::new(
            QuestionRequest {
                id: Uuid::new_v4(),
                chat_id: Uuid::new_v4(),
                loop_id: Uuid::new_v4(),
                run_id: Uuid::new_v4(),
                call_id: "clarify".into(),
                questions,
            },
            "Shorten the report".into(),
        )
    }

    fn report() -> Question {
        question("Which report?", "Report", &["Weekly", "Monthly"], false)
    }

    fn sections() -> Question {
        question(
            "Which sections?",
            "Sections",
            &["Summary", "Risks", "Charts"],
            true,
        )
    }

    fn submitted(update: FlowUpdate) -> Vec<QuestionAnswer> {
        match update {
            FlowUpdate::Submit(answers) => answers,
            FlowUpdate::Show => panic!("expected answers, the dialog stayed open"),
            FlowUpdate::Defer => panic!("expected answers, the dialog was deferred"),
        }
    }

    fn shown(update: FlowUpdate) {
        assert!(matches!(update, FlowUpdate::Show));
    }

    #[test]
    fn a_single_question_is_answered_with_one_choice() {
        let mut dialog = flow(vec![report()]);
        let (menu, keys) = dialog.menu(DisplayProfile::default());
        assert_eq!(menu.title, "Which report?");
        assert_eq!(menu.context, ["☐ Report", "Goal: Shorten the report"]);
        assert_eq!(
            menu.options,
            [
                "Weekly\n     About Weekly",
                "Monthly\n     About Monthly",
                "Other\n     Type something."
            ]
        );
        assert_eq!(keys.entry_option, Some(2));
        assert!(!keys.multi_select);
        assert!(!keys.navigable);

        assert_eq!(
            submitted(dialog.choose(Some(1), None)),
            [QuestionAnswer::option("Monthly")]
        );
    }

    #[test]
    fn other_needs_text_and_submits_it_as_the_answer() {
        let mut dialog = flow(vec![report()]);
        shown(dialog.choose(Some(2), None));
        shown(dialog.choose(Some(2), Some("  ".into())));
        assert_eq!(
            submitted(dialog.choose(Some(2), Some("Daily".into()))),
            [QuestionAnswer::other("Daily")]
        );
    }

    #[test]
    fn multi_select_toggles_in_choice_order_and_confirms_on_the_review() {
        let mut dialog = flow(vec![sections()]);
        let (menu, keys) = dialog.menu(DisplayProfile::default());
        assert_eq!(menu.options.len(), 5);
        assert_eq!(menu.options[4], "Submit");
        assert!(menu.options[0].starts_with("[ ] Summary\n         About Summary"));
        assert!(keys.multi_select);
        assert!(keys.navigable);
        assert_eq!(menu.context[0], "← [☐ Sections]  ✓ Submit →");

        shown(dialog.choose(Some(4), None));
        assert!(
            dialog.menu(DisplayProfile::default()).0.context[2].contains(EMPTY_MULTI_SELECT_NOTICE)
        );
        shown(dialog.choose(Some(2), None));
        shown(dialog.choose(Some(0), None));
        shown(dialog.choose(Some(1), None));
        shown(dialog.choose(Some(1), None));
        shown(dialog.choose(Some(3), Some("Appendix".into())));
        let (menu, keys) = dialog.menu(DisplayProfile::default());
        assert_eq!(menu.selected, 3);
        assert!(menu.options[2].starts_with("[✓] Charts"));
        assert!(menu.options[1].starts_with("[ ] Risks"));
        assert_eq!(menu.options[3], "[✓] Other");
        assert_eq!(keys.entry_text, "Appendix");

        shown(dialog.choose(Some(4), None));
        let (review, keys) = dialog.menu(DisplayProfile::default());
        assert_eq!(review.title, REVIEW_TITLE);
        assert_eq!(review.options, [SUBMIT_ANSWERS, ANSWER_LATER]);
        assert_eq!(keys.entry_option, None);
        assert!(
            review
                .context
                .contains(&"  → Charts, Summary, Appendix".to_owned())
        );
        assert_eq!(
            submitted(dialog.choose(Some(0), None)),
            [QuestionAnswer::options(["Charts", "Summary"]).with_other("Appendix")]
        );
    }

    #[test]
    fn several_questions_step_forward_and_back_before_the_review() {
        let mut dialog = flow(vec![report(), sections()]);
        assert_eq!(
            dialog.menu(DisplayProfile::default()).0.context[0],
            "← [☐ Report]  ☐ Sections  ✓ Submit →"
        );
        dialog.switch(true);
        assert_eq!(dialog.step, 0, "an unanswered question cannot be skipped");
        assert!(dialog.menu(DisplayProfile::default()).0.context[2].contains(UNANSWERED_NOTICE));

        shown(dialog.choose(Some(0), None));
        assert_eq!(dialog.step, 1);
        let (menu, _) = dialog.menu(DisplayProfile::default());
        assert_eq!(menu.title, "Which sections?");
        assert_eq!(menu.context[0], "← ☒ Report  [☐ Sections]  ✓ Submit →");
        assert_eq!(menu.options[4], LAST_QUESTION);

        dialog.switch(false);
        assert_eq!(dialog.step, 0);
        assert_eq!(dialog.menu(DisplayProfile::default()).0.selected, 0);
        shown(dialog.choose(Some(2), Some("Quarterly".into())));
        shown(dialog.choose(Some(1), None));
        dialog.switch(true);
        assert_eq!(dialog.step, 2);
        dialog.switch(true);
        assert_eq!(dialog.step, 2);
        dialog.switch(false);
        dialog.switch(false);
        assert_eq!(
            dialog.menu(DisplayProfile::default()).0.selected,
            2,
            "the Other answer stays focused when returning"
        );
        dialog.switch(true);
        dialog.switch(true);

        let review = dialog.menu(DisplayProfile::default()).0;
        assert_eq!(review.context[0], "← ☒ Report  ☒ Sections  [✓ Submit] →");
        assert_eq!(
            submitted(dialog.choose(Some(0), None)),
            [
                QuestionAnswer::other("Quarterly"),
                QuestionAnswer::option("Risks")
            ]
        );
    }

    #[test]
    fn esc_or_answer_later_defers_the_dialog() {
        let mut dialog = flow(vec![report(), sections()]);
        assert!(matches!(dialog.choose(None, None), FlowUpdate::Defer));
        shown(dialog.choose(Some(0), None));
        shown(dialog.choose(Some(0), None));
        shown(dialog.choose(Some(4), None));
        assert!(matches!(dialog.choose(Some(1), None), FlowUpdate::Defer));
    }

    #[test]
    fn previews_are_plain_text_under_their_option() {
        let mut previewed = report();
        previewed.options[1].preview = Some(
            (1..=10)
                .map(|line| format!("row {line}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let dialog = flow(vec![previewed]);
        let option = &dialog.menu(DisplayProfile::default()).0.options[1];
        assert!(option.starts_with("Monthly\n     About Monthly\n     row 1\n"));
        assert!(option.contains("     row 8\n     …"));
        assert!(!option.contains("row 9"));
    }

    #[test]
    fn typed_lines_answer_one_question_at_a_time() {
        let mut dialog = flow(vec![report(), sections()]);
        assert_eq!(
            dialog.next_question_lines()[0],
            "1/2 [Report] Which report?"
        );
        assert_eq!(
            dialog.answer_typed(" ").unwrap_err(),
            EMPTY_TYPED_ANSWER.to_owned()
        );
        assert_eq!(dialog.answer_typed("monthly").unwrap(), None);
        assert_eq!(
            dialog.next_question_lines(),
            [
                "2/2 [Sections] Which sections? (choose one or more)",
                "  1. Summary — About Summary",
                "  2. Risks — About Risks",
                "  3. Charts — About Charts",
                "  4. Other — type your own answer",
            ]
        );
        assert_eq!(
            dialog.answer_typed("3, Summary, 3").unwrap(),
            Some(vec![
                QuestionAnswer::option("Monthly"),
                QuestionAnswer::options(["Charts", "Summary"]),
            ])
        );
    }

    #[test]
    fn typed_text_that_names_no_option_is_an_other_answer() {
        for (question, text, answer) in [
            (report(), "2", QuestionAnswer::option("Monthly")),
            (report(), "1, 2", QuestionAnswer::other("1, 2")),
            (report(), "3", QuestionAnswer::other("3")),
            (sections(), "Risks", QuestionAnswer::option("Risks")),
            (sections(), "1, extra", QuestionAnswer::other("1, extra")),
            (report(), " Keep both ", QuestionAnswer::other("Keep both")),
        ] {
            let mut dialog = flow(vec![question]);
            assert_eq!(
                dialog.answer_typed(text).unwrap(),
                Some(vec![answer]),
                "{text}"
            );
        }
    }
}

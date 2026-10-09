//! The Chat's Tick Subscription view.
//!
//! It lists every Tick Subscription of the Chat's Loops with its cadence, how many Tick Updates
//! accumulated since the last delivery, what that delivery was, and whether the link to the Tick
//! Instance is live. The user changes a cadence or cancels a subscription here; creating one stays
//! with the Agent, which knows which Tick Instance answers the goal.

use uuid::Uuid;

use super::display::{DisplayProfile, Glyph};
use tickloop_agent_protocol::{
    LoopState, MAX_CADENCE_SECONDS, MIN_CADENCE_SECONDS, TickLinkState, TickSubscriptionChange,
    TickSubscriptionStatus,
};

const MAX_CADENCE_DIGITS: usize = 5;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SubscriptionViewport {
    pub height: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubscriptionAction {
    Previous,
    Next,
    EditCadence,
    TypeDigit(char),
    DeleteDigit,
    Confirm,
    RequestCancel,
    Escape,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubscriptionEffect {
    None,
    Close,
    Change {
        subscription: Uuid,
        change: TickSubscriptionChange,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubscriptionFocus {
    List,
    Cadence,
    CancelConfirmation,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubscriptionLine {
    pub text: String,
    pub selected: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubscriptionFrame {
    pub lines: Vec<SubscriptionLine>,
    pub footer: String,
}

/// The view's own state: the rows it last received, where the cursor is, and whether the user is
/// editing a cadence or confirming a cancellation.
pub struct SubscriptionSurface {
    chat_id: Uuid,
    rows: Vec<TickSubscriptionStatus>,
    selected: Option<Uuid>,
    focus: SubscriptionFocus,
    cadence_draft: String,
    top: usize,
    error: Option<String>,
}

impl SubscriptionSurface {
    pub fn select(&mut self, id: Uuid) {
        if self.rows.iter().any(|row| row.id == id) {
            self.selected = Some(id);
            self.top = 0;
        }
    }

    pub fn open(chat_id: Uuid, rows: Vec<TickSubscriptionStatus>) -> Self {
        let selected = rows.first().map(|row| row.id);
        Self {
            chat_id,
            rows,
            selected,
            focus: SubscriptionFocus::List,
            cadence_draft: String::new(),
            top: 0,
            error: None,
        }
    }

    pub fn chat_id(&self) -> Uuid {
        self.chat_id
    }

    pub fn focus(&self) -> SubscriptionFocus {
        self.focus
    }

    /// Takes a fresh snapshot without disturbing the cursor and answers whether anything changed,
    /// so a quiet poll costs no repaint. A row that disappeared hands the cursor to its neighbour
    /// and ends any editing of it.
    pub fn update(&mut self, rows: Vec<TickSubscriptionStatus>) -> bool {
        if self.rows == rows {
            return false;
        }
        let previous_index = self.selected_index();
        self.rows = rows;
        if self
            .selected
            .is_some_and(|selected| self.index_of(selected).is_some())
        {
            return true;
        }
        self.focus = SubscriptionFocus::List;
        self.cadence_draft.clear();
        self.selected = previous_index
            .and_then(|index| self.rows.get(index.min(self.rows.len().saturating_sub(1))))
            .or_else(|| self.rows.first())
            .map(|row| row.id);
        true
    }

    pub fn report_error(&mut self, message: String) {
        self.error = Some(message);
    }

    pub fn apply(&mut self, action: SubscriptionAction) -> SubscriptionEffect {
        self.error = None;
        match self.focus {
            SubscriptionFocus::List => self.apply_in_list(action),
            SubscriptionFocus::Cadence => self.apply_in_cadence(action),
            SubscriptionFocus::CancelConfirmation => self.apply_in_confirmation(action),
        }
    }

    fn apply_in_list(&mut self, action: SubscriptionAction) -> SubscriptionEffect {
        match action {
            SubscriptionAction::Previous => self.move_by(-1),
            SubscriptionAction::Next => self.move_by(1),
            SubscriptionAction::EditCadence => {
                if let Some(row) = self.selected_row() {
                    self.cadence_draft = row.every_seconds.to_string();
                    self.focus = SubscriptionFocus::Cadence;
                }
            }
            SubscriptionAction::RequestCancel => {
                if self.selected_row().is_some() {
                    self.focus = SubscriptionFocus::CancelConfirmation;
                }
            }
            SubscriptionAction::Escape => return SubscriptionEffect::Close,
            _ => {}
        }
        SubscriptionEffect::None
    }

    fn apply_in_cadence(&mut self, action: SubscriptionAction) -> SubscriptionEffect {
        match action {
            SubscriptionAction::TypeDigit(digit) => {
                if self.cadence_draft.len() < MAX_CADENCE_DIGITS {
                    self.cadence_draft.push(digit);
                }
            }
            SubscriptionAction::DeleteDigit => {
                self.cadence_draft.pop();
            }
            SubscriptionAction::Escape => self.leave_editing(),
            SubscriptionAction::Confirm => {
                let Some(subscription) = self.selected else {
                    self.leave_editing();
                    return SubscriptionEffect::None;
                };
                match self.cadence_draft.parse::<u64>() {
                    Ok(every_seconds)
                        if (MIN_CADENCE_SECONDS..=MAX_CADENCE_SECONDS).contains(&every_seconds) =>
                    {
                        self.leave_editing();
                        return SubscriptionEffect::Change {
                            subscription,
                            change: TickSubscriptionChange::Cadence(every_seconds),
                        };
                    }
                    _ => {
                        self.error = Some(format!(
                            "a cadence is {MIN_CADENCE_SECONDS} to {MAX_CADENCE_SECONDS} seconds"
                        ));
                    }
                }
            }
            _ => {}
        }
        SubscriptionEffect::None
    }

    fn apply_in_confirmation(&mut self, action: SubscriptionAction) -> SubscriptionEffect {
        match action {
            SubscriptionAction::Confirm => {
                self.focus = SubscriptionFocus::List;
                match self.selected {
                    Some(subscription) => SubscriptionEffect::Change {
                        subscription,
                        change: TickSubscriptionChange::Cancel,
                    },
                    None => SubscriptionEffect::None,
                }
            }
            SubscriptionAction::Escape => {
                self.focus = SubscriptionFocus::List;
                SubscriptionEffect::None
            }
            _ => SubscriptionEffect::None,
        }
    }

    fn leave_editing(&mut self) {
        self.focus = SubscriptionFocus::List;
        self.cadence_draft.clear();
    }

    fn move_by(&mut self, step: isize) {
        if self.rows.is_empty() {
            return;
        }
        let current = self.selected_index().unwrap_or(0) as isize;
        let last = self.rows.len() as isize - 1;
        let next = (current + step).clamp(0, last) as usize;
        self.selected = Some(self.rows[next].id);
    }

    fn index_of(&self, subscription: Uuid) -> Option<usize> {
        self.rows.iter().position(|row| row.id == subscription)
    }

    fn selected_index(&self) -> Option<usize> {
        self.selected.and_then(|selected| self.index_of(selected))
    }

    fn selected_row(&self) -> Option<&TickSubscriptionStatus> {
        self.selected_index().map(|index| &self.rows[index])
    }

    pub fn select_at_row(&mut self, row: usize, height: usize, display: DisplayProfile) -> bool {
        let frame = self.frame(SubscriptionViewport { height }, display);
        if frame.lines.get(row).is_none() {
            return false;
        }
        let Some(id) = self.rows.get(self.top + row).map(|row| row.id) else {
            return false;
        };
        if self.focus != SubscriptionFocus::List {
            return self.selected == Some(id);
        }
        self.select(id);
        true
    }

    pub fn content_rows(&self) -> usize {
        self.rows.len().max(1)
    }

    pub fn frame(
        &mut self,
        viewport: SubscriptionViewport,
        display: DisplayProfile,
    ) -> SubscriptionFrame {
        let selected_index = self.selected_index();
        self.top = scrolled_top(self.top, selected_index, self.rows.len(), viewport.height);
        let lines = if self.rows.is_empty() {
            vec![SubscriptionLine {
                text: "No Tick Subscription yet. Ask the Agent to watch a Panel.".into(),
                selected: false,
            }]
        } else {
            self.rows
                .iter()
                .enumerate()
                .skip(self.top)
                .take(viewport.height.max(1))
                .map(|(index, row)| SubscriptionLine {
                    text: row_text(row, display),
                    selected: Some(index) == selected_index,
                })
                .collect()
        };
        SubscriptionFrame {
            lines,
            footer: self.footer(),
        }
    }

    fn footer(&self) -> String {
        if let Some(error) = &self.error {
            return format!("{error} · Esc back");
        }
        match self.focus {
            SubscriptionFocus::List => "Enter cadence · d cancel · Esc close".into(),
            SubscriptionFocus::Cadence => format!(
                "Every {}s · digits edit · Enter save · Esc back",
                if self.cadence_draft.is_empty() {
                    "_"
                } else {
                    &self.cadence_draft
                }
            ),
            SubscriptionFocus::CancelConfirmation => match self.selected_row() {
                Some(row) => format!(
                    "Cancel the subscription to Tick Instance {}? Enter/y confirm · Esc keep it",
                    row.instance
                ),
                None => "Nothing to cancel · Esc back".into(),
            },
        }
    }
}

/// Where the list starts so the selected row stays visible.
fn scrolled_top(top: usize, selected: Option<usize>, rows: usize, height: usize) -> usize {
    let height = height.max(1);
    let top = top.min(rows.saturating_sub(height));
    match selected {
        Some(index) if index < top => index,
        Some(index) if index >= top + height => index + 1 - height,
        _ => top,
    }
}

fn row_text(row: &TickSubscriptionStatus, display: DisplayProfile) -> String {
    format!(
        "{} Tick Instance {} · every {}s · {} new · {} · {}",
        display.glyph(loop_state_glyph(&row.loop_state)),
        row.instance,
        row.every_seconds,
        row.accumulated,
        last_delivery(row),
        link_label(row.link),
    )
}

/// When the last delivery happened and what it was, so a quiet row says whether it ever fired.
pub fn last_delivery(row: &TickSubscriptionStatus) -> String {
    match (row.last_delivery_ms, row.last_delivery.as_deref()) {
        (Some(milliseconds), Some(summary)) => {
            format!("{} {summary}", clock_time(milliseconds))
        }
        _ => "no delivery yet".to_owned(),
    }
}

/// `HH:MM:SS` of a UTC timestamp, taken from the one timestamp format the Agent writes.
fn clock_time(milliseconds: u64) -> String {
    tickloop_agent_base::timestamp::format_timestamp(milliseconds)
        .get(11..19)
        .unwrap_or_default()
        .to_owned()
}

/// The owning Loop's state, so a row explains why it is quiet.
fn loop_state_glyph(state: &LoopState) -> Glyph {
    match state {
        LoopState::Running | LoopState::Queued => Glyph::ToolCall,
        LoopState::WaitingTrigger | LoopState::Idle => Glyph::TickDelivery,
        LoopState::WaitingPermission | LoopState::WaitingQuestion | LoopState::WaitingPlan => {
            Glyph::Warning
        }
        LoopState::Completed => Glyph::Success,
        LoopState::Failed | LoopState::Interrupted | LoopState::Stopped | LoopState::Expired => {
            Glyph::Failure
        }
    }
}

fn link_label(link: TickLinkState) -> &'static str {
    match link {
        TickLinkState::Initializing => "Initializing",
        TickLinkState::Live => "Live",
        TickLinkState::Disconnected => "Disconnected",
        TickLinkState::InstanceGone => "InstanceGone",
        TickLinkState::IdentityUncertain => "IdentityUncertain",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(instance: u64, every_seconds: u64) -> TickSubscriptionStatus {
        TickSubscriptionStatus {
            title: None,
            updated_at_ms: None,
            kind: None,
            id: Uuid::from_u128(u128::from(instance)),
            loop_id: Uuid::from_u128(900),
            loop_state: LoopState::WaitingTrigger,
            instance: tick_protocol::InstanceRef::native(format!("fixture-run-{instance}"))
                .unwrap(),
            every_seconds,
            accumulated: 0,
            last_delivery_ms: None,
            last_delivery: None,
            link: TickLinkState::Live,
        }
    }

    fn viewport() -> SubscriptionViewport {
        SubscriptionViewport { height: 10 }
    }

    #[test]
    fn a_row_shows_the_cadence_the_accumulated_count_and_the_link() {
        let mut waiting = row(3, 60);
        waiting.accumulated = 4;
        waiting.last_delivery_ms = Some(1_774_000_845_000);
        waiting.last_delivery = Some("3 #12 · dropped 2".into());
        let mut surface = SubscriptionSurface::open(Uuid::from_u128(1), vec![waiting, row(4, 90)]);

        let frame = surface.frame(viewport(), DisplayProfile::default());

        assert_eq!(
            frame.lines[0].text,
            "◉ Tick Instance herdr:fixture-run-3 · every 60s · 4 new · 10:00:45 3 #12 · dropped 2 · Live"
        );
        assert_eq!(
            frame.lines[1].text,
            "◉ Tick Instance herdr:fixture-run-4 · every 90s · 0 new · no delivery yet · Live"
        );
        assert!(frame.lines[0].selected);
    }

    #[test]
    fn a_gone_instance_keeps_its_row_and_says_so() {
        let mut gone = row(3, 60);
        gone.link = TickLinkState::InstanceGone;
        let mut surface = SubscriptionSurface::open(Uuid::from_u128(1), vec![gone]);

        let frame = surface.frame(viewport(), DisplayProfile::default());

        assert!(frame.lines[0].text.ends_with("InstanceGone"));
    }

    #[test]
    fn enter_edits_a_cadence_and_a_bad_one_keeps_the_editor_open() {
        let mut surface = SubscriptionSurface::open(Uuid::from_u128(1), vec![row(3, 60)]);

        surface.apply(SubscriptionAction::EditCadence);
        assert_eq!(surface.focus(), SubscriptionFocus::Cadence);
        for _ in 0..2 {
            surface.apply(SubscriptionAction::DeleteDigit);
        }
        surface.apply(SubscriptionAction::TypeDigit('1'));
        assert_eq!(
            surface.apply(SubscriptionAction::Confirm),
            SubscriptionEffect::None,
            "one second is below the floor"
        );
        assert_eq!(surface.focus(), SubscriptionFocus::Cadence);

        surface.apply(SubscriptionAction::TypeDigit('5'));
        assert_eq!(
            surface.apply(SubscriptionAction::Confirm),
            SubscriptionEffect::Change {
                subscription: Uuid::from_u128(3),
                change: TickSubscriptionChange::Cadence(15),
            }
        );
        assert_eq!(surface.focus(), SubscriptionFocus::List);
    }

    #[test]
    fn cancelling_asks_first_and_escape_keeps_the_subscription() {
        let mut surface =
            SubscriptionSurface::open(Uuid::from_u128(1), vec![row(3, 60), row(4, 60)]);
        surface.apply(SubscriptionAction::Next);

        surface.apply(SubscriptionAction::RequestCancel);
        assert_eq!(surface.focus(), SubscriptionFocus::CancelConfirmation);
        assert!(
            surface
                .frame(viewport(), DisplayProfile::default())
                .footer
                .contains("Tick Instance herdr:fixture-run-4")
        );

        assert_eq!(
            surface.apply(SubscriptionAction::Escape),
            SubscriptionEffect::None
        );
        assert_eq!(surface.focus(), SubscriptionFocus::List);

        surface.apply(SubscriptionAction::RequestCancel);
        assert_eq!(
            surface.apply(SubscriptionAction::Confirm),
            SubscriptionEffect::Change {
                subscription: Uuid::from_u128(4),
                change: TickSubscriptionChange::Cancel,
            }
        );
    }

    #[test]
    fn a_cancelled_row_hands_the_cursor_to_its_neighbour() {
        let mut surface =
            SubscriptionSurface::open(Uuid::from_u128(1), vec![row(3, 60), row(4, 60)]);
        surface.apply(SubscriptionAction::Next);
        surface.apply(SubscriptionAction::EditCadence);

        assert!(surface.update(vec![row(3, 60)]));

        assert_eq!(surface.focus(), SubscriptionFocus::List);
        let frame = surface.frame(viewport(), DisplayProfile::default());
        assert!(
            frame.lines[0].selected
                && frame.lines[0]
                    .text
                    .contains("Tick Instance herdr:fixture-run-3"),
            "{frame:?}"
        );
        assert!(
            !surface.update(vec![row(3, 60)]),
            "an unchanged snapshot costs no repaint"
        );
    }

    #[test]
    fn escape_in_the_list_closes_the_view() {
        let mut surface = SubscriptionSurface::open(Uuid::from_u128(1), Vec::new());

        assert_eq!(
            surface.apply(SubscriptionAction::Escape),
            SubscriptionEffect::Close
        );
        assert!(
            surface.frame(viewport(), DisplayProfile::default()).lines[0]
                .text
                .starts_with("No Tick Subscription yet")
        );
    }
}

//! The Tick Subscriptions on one Monitor row, shown inside the Monitor column.
//!
//! Each Loop that subscribes to the row's Tick Instance appears with its frequency, how many Tick
//! Updates accumulated since the last delivery, what that delivery was, and whether the link to
//! the instance is live. The user picks another periodic frequency or cancels a subscription
//! here; an immediate subscription keeps the frequency its Tick type decides. The Agent creates
//! subscriptions, and the user creates one with `f` on the row itself.

use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tick_protocol::InstanceRef;
use uuid::Uuid;

use super::{
    display::{DisplayProfile, Glyph},
    style::{Role, RoleLine},
    tick_actions::{
        PreparedTickAction, TickConfirmationView, TickDetailKey, TickDetailLine, TickDetailView,
    },
};
use crate::monitor_rows::{
    SUBSCRIPTION_FREQUENCY_KEY as FREQUENCY_KEY, SubscriptionOperation, UNSUBSCRIBE_KEY,
};
use tickloop_agent_protocol::{
    LoopId, LoopState, TickLinkState, TickSubscriptionChange, TickSubscriptionFrequency,
    TickSubscriptionStatus,
};

const SUBSCRIPTION_ROWS: usize = 3;
const HEADER_ROWS: usize = 2;
const DETAIL_INDENT: &str = "    ";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SubscriptionFocus {
    List,
    Frequency,
    CancelConfirmation,
}

/// A requested change the detail hands to the Agent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct SubscriptionRequest {
    pub(super) subscription: Uuid,
    pub(super) change: TickSubscriptionChange,
}

pub(super) struct SubscriptionDetail {
    id: Uuid,
    chat: Uuid,
    instance: InstanceRef,
    title: String,
    rows: Vec<TickSubscriptionStatus>,
    goals: HashMap<LoopId, String>,
    selected: Option<Uuid>,
    focus: SubscriptionFocus,
    /// The periodic frequency the cursor is on while the user picks one.
    frequency_choice: TickSubscriptionFrequency,
    top: usize,
    error: Option<String>,
    display: DisplayProfile,
}

impl SubscriptionDetail {
    pub(super) fn open(
        chat: Uuid,
        instance: InstanceRef,
        title: String,
        rows: Vec<TickSubscriptionStatus>,
        goals: HashMap<LoopId, String>,
        operation: SubscriptionOperation,
        display: DisplayProfile,
    ) -> Self {
        let mut detail = Self {
            id: Uuid::new_v4(),
            chat,
            instance,
            title,
            selected: rows.first().map(|row| row.id),
            rows,
            goals,
            focus: SubscriptionFocus::List,
            frequency_choice: TickSubscriptionFrequency::PERIODIC[0],
            top: 0,
            error: None,
            display,
        };
        if detail.rows.len() == 1 {
            match operation {
                SubscriptionOperation::View => {}
                SubscriptionOperation::EditFrequency => detail.edit_frequency(),
                SubscriptionOperation::Cancel => {
                    detail.focus = SubscriptionFocus::CancelConfirmation
                }
            }
        }
        detail
    }

    pub(super) fn instance(&self) -> &InstanceRef {
        &self.instance
    }

    #[cfg(test)]
    pub(super) fn focus(&self) -> SubscriptionFocus {
        self.focus
    }

    /// Takes the row's current subscriptions without disturbing the cursor. A subscription that
    /// disappeared hands the cursor to its neighbour and ends any editing of it.
    pub(super) fn update(
        &mut self,
        title: String,
        rows: Vec<TickSubscriptionStatus>,
        goals: HashMap<LoopId, String>,
    ) {
        self.title = title;
        self.goals = goals;
        if self.rows == rows {
            return;
        }
        let previous_index = self.selected_index();
        self.rows = rows;
        if self
            .selected
            .is_some_and(|selected| self.index_of(selected).is_some())
        {
            return;
        }
        self.leave_editing();
        self.selected = previous_index
            .and_then(|index| self.rows.get(index.min(self.rows.len().saturating_sub(1))))
            .or_else(|| self.rows.first())
            .map(|row| row.id);
    }

    pub(super) fn report_error(&mut self, message: String) {
        self.error = Some(message);
    }

    fn apply_key(&mut self, key: KeyEvent) -> Result<Option<SubscriptionRequest>, bool> {
        self.error = None;
        match self.focus {
            SubscriptionFocus::List => match (key.code, key.modifiers) {
                (KeyCode::Up | KeyCode::Char('k'), KeyModifiers::NONE)
                | (KeyCode::Char('p'), KeyModifiers::CONTROL) => self.move_by(-1),
                (KeyCode::Down | KeyCode::Char('j'), KeyModifiers::NONE)
                | (KeyCode::Char('n'), KeyModifiers::CONTROL) => self.move_by(1),
                (KeyCode::Enter, _) | (KeyCode::Char(FREQUENCY_KEY), KeyModifiers::NONE) => {
                    self.edit_frequency()
                }
                (KeyCode::Char(UNSUBSCRIBE_KEY), KeyModifiers::NONE) => {
                    if self.selected_row().is_some() {
                        self.focus = SubscriptionFocus::CancelConfirmation;
                    }
                }
                (KeyCode::Esc | KeyCode::Left, _) => return Err(true),
                _ => {}
            },
            SubscriptionFocus::Frequency => match (key.code, key.modifiers) {
                (KeyCode::Up | KeyCode::Char('k'), KeyModifiers::NONE)
                | (KeyCode::Char('p'), KeyModifiers::CONTROL) => self.choose_by(-1),
                (KeyCode::Down | KeyCode::Char('j'), KeyModifiers::NONE)
                | (KeyCode::Char('n'), KeyModifiers::CONTROL) => self.choose_by(1),
                (KeyCode::Esc, _) => self.leave_editing(),
                (KeyCode::Enter, _) => return Ok(self.confirm_frequency()),
                _ => {}
            },
            SubscriptionFocus::CancelConfirmation => match (key.code, key.modifiers) {
                (KeyCode::Enter, _) | (KeyCode::Char('y'), KeyModifiers::NONE) => {
                    self.focus = SubscriptionFocus::List;
                    return Ok(self.selected.map(|subscription| SubscriptionRequest {
                        subscription,
                        change: TickSubscriptionChange::Cancel,
                    }));
                }
                (KeyCode::Esc, _) | (KeyCode::Char('n'), KeyModifiers::NONE) => {
                    self.focus = SubscriptionFocus::List;
                }
                _ => {}
            },
        }
        Ok(None)
    }

    /// Starts picking a frequency for the selected subscription; an immediate one has none to pick.
    fn edit_frequency(&mut self) {
        if let Some(frequency) = self
            .selected_row()
            .map(|row| row.frequency)
            .filter(|frequency| frequency.period().is_some())
        {
            self.frequency_choice = frequency;
            self.focus = SubscriptionFocus::Frequency;
        }
    }

    fn choose_by(&mut self, step: isize) {
        let choices = TickSubscriptionFrequency::PERIODIC;
        let current = choices
            .iter()
            .position(|choice| *choice == self.frequency_choice)
            .unwrap_or(0) as isize;
        let next = (current + step).clamp(0, choices.len() as isize - 1) as usize;
        self.frequency_choice = choices[next];
    }

    /// Asks for the chosen frequency, or nothing when it is the one the subscription has.
    fn confirm_frequency(&mut self) -> Option<SubscriptionRequest> {
        let current = self.selected_row().map(|row| (row.id, row.frequency));
        self.leave_editing();
        let (subscription, frequency) = current?;
        (frequency != self.frequency_choice).then_some(SubscriptionRequest {
            subscription,
            change: TickSubscriptionChange::Frequency(self.frequency_choice),
        })
    }

    fn leave_editing(&mut self) {
        self.focus = SubscriptionFocus::List;
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

    fn goal(&self, row: &TickSubscriptionStatus) -> String {
        self.goals
            .get(&row.loop_id)
            .map(|goal| super::safe_line(goal))
            .filter(|goal| !goal.is_empty())
            .unwrap_or_else(|| format!("Loop {}", &row.loop_id.simple().to_string()[..8]))
    }

    fn subscription_lines(&self, row: &TickSubscriptionStatus, selected: bool) -> [RoleLine; 3] {
        let role = if selected {
            Role::Text
        } else {
            Role::MutedText
        };
        let mut frequency = format!(
            "{}⟳ {}",
            if selected { "› " } else { "  " },
            frequency_label(row.frequency)
        );
        if row.accumulated > 0 {
            frequency.push_str(&format!(" · {} new", row.accumulated));
        }
        [
            RoleLine::role(frequency, role),
            RoleLine::role(
                format!(
                    "{DETAIL_INDENT}{} {}",
                    self.display.glyph(loop_state_glyph(&row.loop_state)),
                    self.goal(row)
                ),
                Role::MutedText,
            ),
            RoleLine::role(
                format!(
                    "{DETAIL_INDENT}{} · {}",
                    last_delivery(row),
                    link_label(row.link)
                ),
                Role::MutedText,
            ),
        ]
    }

    fn subscription_list(&mut self, height: usize) -> Vec<RoleLine> {
        let mut lines = vec![
            RoleLine::role(super::safe_line(&self.title), Role::Text),
            RoleLine::role(
                format!("Tick Subscriptions {}", self.rows.len()),
                Role::MutedText,
            ),
        ];
        if self.rows.is_empty() {
            lines.push(RoleLine::role("No subscription left.", Role::MutedText));
            return lines;
        }
        let capacity = (height.saturating_sub(HEADER_ROWS) / SUBSCRIPTION_ROWS).max(1);
        let selected = self.selected_index().unwrap_or(0);
        self.top = self.top.min(self.rows.len().saturating_sub(capacity));
        if selected < self.top {
            self.top = selected;
        } else if selected >= self.top + capacity {
            self.top = selected + 1 - capacity;
        }
        for (index, row) in self.rows.iter().enumerate().skip(self.top).take(capacity) {
            lines.extend(self.subscription_lines(row, index == selected));
        }
        lines
    }

    /// The periodic frequencies the selected subscription can take, the cursor on the chosen one.
    fn frequency_lines(&self, row: &TickSubscriptionStatus) -> Vec<RoleLine> {
        let mut lines = vec![
            RoleLine::role(super::safe_line(&self.title), Role::Text),
            RoleLine::role(format!("Frequency · {}", self.goal(row)), Role::MutedText),
        ];
        lines.extend(TickSubscriptionFrequency::PERIODIC.map(|choice| {
            let is_chosen = choice == self.frequency_choice;
            RoleLine::role(
                format!(
                    "{}{}",
                    if is_chosen { "› " } else { "  " },
                    frequency_label(choice)
                ),
                if is_chosen {
                    Role::Text
                } else {
                    Role::MutedText
                },
            )
        }));
        lines
    }
}

impl TickDetailView for SubscriptionDetail {
    fn confirmation(&self) -> Option<TickConfirmationView> {
        let row = self
            .selected_row()
            .filter(|_| self.focus == SubscriptionFocus::CancelConfirmation)?;
        Some(TickConfirmationView {
            title: "Cancel Tick Subscription".into(),
            message: format!(
                "Stop delivering {} to {}?",
                super::safe_line(&self.title),
                self.goal(row)
            ),
            label: "Cancel subscription".into(),
        })
    }

    fn id(&self) -> Uuid {
        self.id
    }

    fn chat(&self) -> Uuid {
        self.chat
    }

    fn lines(&mut self, width: usize, height: usize, _now: u64) -> Vec<TickDetailLine> {
        let lines = match self
            .selected_row()
            .filter(|_| self.focus == SubscriptionFocus::Frequency)
        {
            Some(row) => self.frequency_lines(row),
            None => self.subscription_list(height),
        };
        lines
            .into_iter()
            .take(height)
            .map(|mut line| {
                line.truncate(width);
                TickDetailLine { line }
            })
            .collect()
    }

    fn hint(&self) -> String {
        if let Some(error) = &self.error {
            return format!("{error} · esc back");
        }
        match self.focus {
            SubscriptionFocus::List if self.rows.is_empty() => "esc back".into(),
            SubscriptionFocus::List
                if self
                    .selected_row()
                    .is_some_and(|row| row.frequency == TickSubscriptionFrequency::Immediate) =>
            {
                format!("{UNSUBSCRIBE_KEY} unsubscribe · esc back")
            }
            SubscriptionFocus::List => {
                format!("{FREQUENCY_KEY} frequency · {UNSUBSCRIBE_KEY} unsubscribe · esc back")
            }
            SubscriptionFocus::Frequency => "j/k select · enter save · esc back".into(),
            SubscriptionFocus::CancelConfirmation => "enter confirm · esc back".into(),
        }
    }

    fn key(&mut self, key: KeyEvent, _width: usize, _height: usize) -> TickDetailKey {
        match self.apply_key(key) {
            Err(_) => TickDetailKey::Back,
            Ok(None) => TickDetailKey::None,
            Ok(Some(request)) => TickDetailKey::Action(PreparedTickAction::new(
                self.instance.to_string(),
                match request.change {
                    TickSubscriptionChange::Frequency(_) => FREQUENCY_KEY,
                    TickSubscriptionChange::Cancel => UNSUBSCRIBE_KEY,
                },
                super::tick_actions::builtin::change_subscription(self.id, request),
            )),
        }
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

/// The frequency in words, with the period of a periodic one.
fn frequency_label(frequency: TickSubscriptionFrequency) -> String {
    match frequency.period() {
        Some(period) => format!("{} · every {} min", frequency.name(), period.as_secs() / 60),
        None => frequency.name().to_owned(),
    }
}

/// When the last delivery happened and what it was, so a quiet row says whether it ever fired.
fn last_delivery(row: &TickSubscriptionStatus) -> String {
    match (row.last_delivery_ms, row.last_delivery.as_deref()) {
        (Some(milliseconds), Some(summary)) => {
            format!("{} {}", clock_time(milliseconds), super::safe_line(summary))
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
        TickLinkState::Initializing => "initializing",
        TickLinkState::Live => "live",
        TickLinkState::Disconnected => "disconnected",
        TickLinkState::InstanceGone => "gone",
        TickLinkState::IdentityUncertain => "uncertain",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn instance() -> InstanceRef {
        InstanceRef::new("calendar", "fixture-run").unwrap()
    }

    fn row(id: u128, frequency: TickSubscriptionFrequency) -> TickSubscriptionStatus {
        TickSubscriptionStatus {
            title: None,
            updated_at_ms: None,
            kind: None,
            id: Uuid::from_u128(id),
            loop_id: Uuid::from_u128(900 + id),
            loop_state: LoopState::WaitingTrigger,
            instance: instance(),
            frequency,
            accumulated: 0,
            last_delivery_ms: None,
            last_delivery: None,
            link: TickLinkState::Live,
            next_check_at_ms: None,
            period_ms: None,
        }
    }

    fn detail(
        rows: Vec<TickSubscriptionStatus>,
        operation: SubscriptionOperation,
    ) -> SubscriptionDetail {
        let goals = rows
            .iter()
            .map(|row| (row.loop_id, format!("Watch run {}", row.id.as_u128())))
            .collect();
        SubscriptionDetail::open(
            Uuid::from_u128(1),
            instance(),
            "Fixture run".into(),
            rows,
            goals,
            operation,
            DisplayProfile::default(),
        )
    }

    fn text(detail: &mut SubscriptionDetail) -> Vec<String> {
        detail
            .lines(40, 20, 0)
            .into_iter()
            .map(|line| line.line.text())
            .collect()
    }

    fn press(detail: &mut SubscriptionDetail, code: KeyCode) -> TickDetailKey {
        detail.key(KeyEvent::new(code, KeyModifiers::NONE), 40, 20)
    }

    fn requested(key: TickDetailKey) -> Option<char> {
        match key {
            TickDetailKey::Action(action) => Some(action.key),
            _ => None,
        }
    }

    #[test]
    fn each_subscription_shows_its_frequency_loop_delivery_and_link() {
        let mut waiting = row(3, TickSubscriptionFrequency::High);
        waiting.accumulated = 4;
        waiting.last_delivery_ms = Some(1_774_000_845_000);
        waiting.last_delivery = Some("3 #12 · dropped 2".into());
        let mut detail = detail(
            vec![waiting, row(4, TickSubscriptionFrequency::Medium)],
            SubscriptionOperation::View,
        );

        let lines = text(&mut detail);

        assert_eq!(lines[0], "Fixture run");
        assert_eq!(lines[1], "Tick Subscriptions 2");
        assert_eq!(lines[2], "› ⟳ high · every 1 min · 4 new");
        assert_eq!(lines[3], "    ◉ Watch run 3");
        assert_eq!(lines[4], "    10:00:45 3 #12 · dropped 2 · live");
        assert_eq!(lines[5], "  ⟳ medium · every 10 min");
        assert_eq!(lines[7], "    no delivery yet · live");
        assert_eq!(detail.hint(), "e frequency · u unsubscribe · esc back");
    }

    #[test]
    fn a_frequency_is_picked_among_the_three_periodic_ones_only() {
        let mut detail = detail(
            vec![row(3, TickSubscriptionFrequency::Medium)],
            SubscriptionOperation::EditFrequency,
        );
        assert_eq!(detail.focus(), SubscriptionFocus::Frequency);
        assert_eq!(detail.hint(), "j/k select · enter save · esc back");
        assert_eq!(
            text(&mut detail),
            [
                "Fixture run",
                "Frequency · Watch run 3",
                "  high · every 1 min",
                "› medium · every 10 min",
                "  low · every 30 min",
            ],
            "the cursor starts on the current frequency and immediate is never offered"
        );

        for _ in 0..3 {
            press(&mut detail, KeyCode::Char('j'));
        }
        assert_eq!(text(&mut detail)[4], "› low · every 30 min");
        for _ in 0..3 {
            detail.key(
                KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
                40,
                20,
            );
        }
        assert_eq!(text(&mut detail)[2], "› high · every 1 min");

        assert_eq!(
            detail.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Ok(Some(SubscriptionRequest {
                subscription: Uuid::from_u128(3),
                change: TickSubscriptionChange::Frequency(TickSubscriptionFrequency::High),
            }))
        );
        assert_eq!(detail.focus(), SubscriptionFocus::List);
    }

    #[test]
    fn keeping_the_current_frequency_or_escaping_changes_nothing() {
        let mut detail = detail(
            vec![row(3, TickSubscriptionFrequency::Low)],
            SubscriptionOperation::EditFrequency,
        );
        assert_eq!(requested(press(&mut detail, KeyCode::Enter)), None);
        assert_eq!(detail.focus(), SubscriptionFocus::List);

        press(&mut detail, KeyCode::Char('e'));
        press(&mut detail, KeyCode::Char('k'));
        assert!(matches!(
            press(&mut detail, KeyCode::Esc),
            TickDetailKey::None
        ));
        assert_eq!(detail.focus(), SubscriptionFocus::List);
        assert_eq!(text(&mut detail)[2], "› ⟳ low · every 30 min");
    }

    #[test]
    fn an_immediate_subscription_shows_immediate_and_offers_no_frequency() {
        let mut detail = detail(
            vec![row(3, TickSubscriptionFrequency::Immediate)],
            SubscriptionOperation::EditFrequency,
        );
        assert_eq!(detail.focus(), SubscriptionFocus::List);
        assert_eq!(text(&mut detail)[2], "› ⟳ immediate");
        assert_eq!(detail.hint(), "u unsubscribe · esc back");

        for code in [KeyCode::Char('e'), KeyCode::Enter] {
            assert_eq!(requested(press(&mut detail, code)), None);
            assert_eq!(detail.focus(), SubscriptionFocus::List);
        }
    }

    #[test]
    fn unsubscribing_asks_first_and_escape_keeps_the_subscription() {
        let mut detail = detail(
            vec![
                row(3, TickSubscriptionFrequency::High),
                row(4, TickSubscriptionFrequency::High),
            ],
            SubscriptionOperation::Cancel,
        );
        assert_eq!(
            detail.focus(),
            SubscriptionFocus::List,
            "several subscriptions let the user choose first"
        );
        press(&mut detail, KeyCode::Char('j'));
        press(&mut detail, KeyCode::Char('u'));
        let confirmation = detail.confirmation().unwrap();
        assert_eq!(
            confirmation.message,
            "Stop delivering Fixture run to Watch run 4?"
        );

        assert!(matches!(
            press(&mut detail, KeyCode::Esc),
            TickDetailKey::None
        ));
        assert!(detail.confirmation().is_none());

        press(&mut detail, KeyCode::Char('u'));
        assert_eq!(
            detail.apply_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Ok(Some(SubscriptionRequest {
                subscription: Uuid::from_u128(4),
                change: TickSubscriptionChange::Cancel,
            }))
        );
    }

    #[test]
    fn a_single_subscription_opens_straight_into_the_cancel_confirmation() {
        let mut detail = detail(
            vec![row(3, TickSubscriptionFrequency::Immediate)],
            SubscriptionOperation::Cancel,
        );
        assert!(detail.confirmation().is_some());
        assert_eq!(requested(press(&mut detail, KeyCode::Enter)), Some('u'));
    }

    #[test]
    fn a_cancelled_subscription_hands_the_cursor_to_its_neighbour() {
        let mut detail = detail(
            vec![
                row(3, TickSubscriptionFrequency::High),
                row(4, TickSubscriptionFrequency::High),
            ],
            SubscriptionOperation::View,
        );
        press(&mut detail, KeyCode::Down);
        press(&mut detail, KeyCode::Enter);
        assert_eq!(detail.focus(), SubscriptionFocus::Frequency);

        detail.update(
            "Fixture run".into(),
            vec![row(3, TickSubscriptionFrequency::High)],
            HashMap::new(),
        );

        assert_eq!(detail.focus(), SubscriptionFocus::List);
        assert_eq!(text(&mut detail)[2], "› ⟳ high · every 1 min");
        detail.update("Fixture run".into(), Vec::new(), HashMap::new());
        assert_eq!(text(&mut detail)[2], "No subscription left.");
        assert_eq!(detail.hint(), "esc back");
        assert!(matches!(
            press(&mut detail, KeyCode::Esc),
            TickDetailKey::Back
        ));
    }
}

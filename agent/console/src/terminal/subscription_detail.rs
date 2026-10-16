//! The Tick Subscriptions on one Monitor row, shown inside the Monitor column.
//!
//! Each Loop's subscription shows its frequency, pending updates, last delivery and link state.
//! `s` cycles a periodic subscription through its frequencies and off; `u` cancels immediately.
//! Immediate subscriptions keep the frequency their Tick type decides.

use std::{collections::HashMap, sync::Arc};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tick_protocol::InstanceRef;
use uuid::Uuid;

use super::{
    display::{DisplayProfile, Glyph},
    style::{Role, RoleLine},
    tick_actions::{
        PreparedTickAction, TickDetailKey, TickDetailLine, TickDetailView,
        builtin::{CycleSubscription, change_subscription},
    },
};
use crate::monitor_rows::{SUBSCRIBE_KEY, SubscriptionOperation, UNSUBSCRIBE_KEY};
use tickloop_agent_protocol::{
    LoopId, LoopState, TickLinkState, TickSubscriptionChange, TickSubscriptionFrequency,
    TickSubscriptionStatus,
};

const SUBSCRIPTION_ROWS: usize = 3;
const HEADER_ROWS: usize = 2;
const DETAIL_INDENT: &str = "    ";
const IMMEDIATE_FREQUENCY_NOTICE: &str = "Its Tick type decides an immediate frequency";

/// A requested change the detail hands to the Agent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct SubscriptionRequest {
    pub(super) subscription: Uuid,
    pub(super) change: TickSubscriptionChange,
}

/// What a key asks of the detail.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DetailStep {
    Stay,
    Leave,
    CycleSubscription(Uuid),
    Cancel(Uuid),
}

pub(super) struct SubscriptionDetail {
    id: Uuid,
    chat: Uuid,
    instance: InstanceRef,
    title: String,
    rows: Vec<TickSubscriptionStatus>,
    goals: HashMap<LoopId, String>,
    selected: Option<Uuid>,
    top: usize,
    /// A refusal or explanation the hint shows until the next key.
    notice: Option<String>,
    /// Whether the row's `u` opened the detail, so it closes once a cancel goes through.
    opened_to_cancel: bool,
    /// The subscription whose cancel the detail last asked for, until the Agent refuses it.
    cancelling: Option<Uuid>,
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
        Self {
            id: Uuid::new_v4(),
            chat,
            instance,
            title,
            selected: rows.first().map(|row| row.id),
            rows,
            goals,
            top: 0,
            notice: None,
            opened_to_cancel: operation == SubscriptionOperation::Cancel,
            cancelling: None,
            display,
        }
    }

    pub(super) fn instance(&self) -> &InstanceRef {
        &self.instance
    }

    /// Takes the row's current subscriptions without disturbing the cursor. A subscription that
    /// disappeared hands the cursor to its neighbour.
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
        self.selected = previous_index
            .and_then(|index| self.rows.get(index.min(self.rows.len().saturating_sub(1))))
            .or_else(|| self.rows.first())
            .map(|row| row.id);
    }

    pub(super) fn report(&mut self, message: String) {
        self.notice = Some(message);
        self.cancelling = None;
    }

    /// Whether the detail has nothing left to show: no subscription remains, or the cancel the
    /// row's `u` opened it for went through.
    pub(super) fn is_done(&self) -> bool {
        self.rows.is_empty()
            || (self.opened_to_cancel
                && self
                    .cancelling
                    .is_some_and(|subscription| self.index_of(subscription).is_none()))
    }

    fn apply_key(&mut self, key: KeyEvent) -> DetailStep {
        self.notice = None;
        match (key.code, key.modifiers) {
            (KeyCode::Up | KeyCode::Char('k'), KeyModifiers::NONE)
            | (KeyCode::Char('p'), KeyModifiers::CONTROL) => self.move_by(-1),
            (KeyCode::Down | KeyCode::Char('j'), KeyModifiers::NONE)
            | (KeyCode::Char('n'), KeyModifiers::CONTROL) => self.move_by(1),
            (KeyCode::Char(SUBSCRIBE_KEY), KeyModifiers::NONE) => {
                match self
                    .selected_row()
                    .map(|row| (row.id, row.frequency.period()))
                {
                    Some((subscription, Some(_))) => {
                        return DetailStep::CycleSubscription(subscription);
                    }
                    Some((_, None)) => self.notice = Some(IMMEDIATE_FREQUENCY_NOTICE.into()),
                    None => {}
                }
            }
            (KeyCode::Char(UNSUBSCRIBE_KEY), KeyModifiers::NONE) => {
                if let Some(subscription) = self.selected {
                    self.cancelling = Some(subscription);
                    return DetailStep::Cancel(subscription);
                }
            }
            (KeyCode::Esc | KeyCode::Left, _) => return DetailStep::Leave,
            _ => {}
        }
        DetailStep::Stay
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
            "{}↻ {}",
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
}

impl TickDetailView for SubscriptionDetail {
    fn id(&self) -> Uuid {
        self.id
    }

    fn chat(&self) -> Uuid {
        self.chat
    }

    fn lines(&mut self, width: usize, height: usize, _now: u64) -> Vec<TickDetailLine> {
        self.subscription_list(height)
            .into_iter()
            .take(height)
            .map(|mut line| {
                line.truncate(width);
                TickDetailLine { line }
            })
            .collect()
    }

    fn hint(&self) -> String {
        if let Some(notice) = &self.notice {
            return format!("{notice} · esc back");
        }
        if self
            .selected_row()
            .is_some_and(|row| row.frequency.period().is_none())
        {
            format!("{UNSUBSCRIBE_KEY} unsubscribe · esc back")
        } else {
            format!("{SUBSCRIBE_KEY} subscriptions · {UNSUBSCRIBE_KEY} unsubscribe · esc back")
        }
    }

    fn key(&mut self, key: KeyEvent, _width: usize, _height: usize) -> TickDetailKey {
        match self.apply_key(key) {
            DetailStep::Stay => TickDetailKey::None,
            DetailStep::Leave => TickDetailKey::Back,
            DetailStep::CycleSubscription(subscription) => {
                TickDetailKey::Prepare(Arc::new(CycleSubscription {
                    instance: self.instance.clone(),
                    subscription,
                    panel: Some(self.id),
                }))
            }
            DetailStep::Cancel(subscription) => TickDetailKey::Action(PreparedTickAction::new(
                self.instance.to_string(),
                UNSUBSCRIBE_KEY,
                change_subscription(
                    Some(self.id),
                    SubscriptionRequest {
                        subscription,
                        change: TickSubscriptionChange::Cancel,
                    },
                ),
            )),
        }
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

/// The frequency in words, with the period of a periodic one.
pub(super) fn frequency_label(frequency: TickSubscriptionFrequency) -> String {
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
            has_pending_change: false,
            pending_since_ms: None,
            program_exit: None,
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
        assert_eq!(lines[2], "› ↻ high · every 1 min · 4 new");
        assert_eq!(lines[3], "    ◉ Watch run 3");
        assert_eq!(lines[4], "    10:00:45 3 #12 · dropped 2 · live");
        assert_eq!(lines[5], "  ↻ medium · every 10 min");
        assert_eq!(lines[7], "    no delivery yet · live");
        assert_eq!(detail.hint(), "s subscriptions · u unsubscribe · esc back");
    }

    fn step(detail: &mut SubscriptionDetail, code: KeyCode) -> DetailStep {
        detail.apply_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn s_moves_the_selected_periodic_subscription_on_at_once_without_a_picker() {
        let mut detail = detail(
            vec![
                row(3, TickSubscriptionFrequency::High),
                row(4, TickSubscriptionFrequency::Low),
            ],
            SubscriptionOperation::View,
        );
        assert_eq!(detail.hint(), "s subscriptions · u unsubscribe · esc back");
        assert_eq!(
            step(&mut detail, KeyCode::Char('s')),
            DetailStep::CycleSubscription(Uuid::from_u128(3))
        );
        assert!(matches!(
            press(&mut detail, KeyCode::Char('s')),
            TickDetailKey::Prepare(_)
        ));
        press(&mut detail, KeyCode::Char('j'));
        assert_eq!(
            step(&mut detail, KeyCode::Char('s')),
            DetailStep::CycleSubscription(Uuid::from_u128(4))
        );
        assert!(detail.confirmation().is_none(), "no confirmation follows");

        for code in [KeyCode::Char('e'), KeyCode::Enter] {
            assert_eq!(step(&mut detail, code), DetailStep::Stay, "{code:?}");
        }
    }

    #[test]
    fn an_immediate_subscription_shows_immediate_and_s_only_explains_its_frequency() {
        let mut detail = detail(
            vec![row(3, TickSubscriptionFrequency::Immediate)],
            SubscriptionOperation::View,
        );
        assert_eq!(text(&mut detail)[2], "› ↻ immediate");
        assert_eq!(detail.hint(), "u unsubscribe · esc back");

        assert!(matches!(
            press(&mut detail, KeyCode::Char('s')),
            TickDetailKey::None
        ));
        assert_eq!(
            detail.hint(),
            "Its Tick type decides an immediate frequency · esc back"
        );
        press(&mut detail, KeyCode::Char('j'));
        assert_eq!(detail.hint(), "u unsubscribe · esc back");
    }

    #[test]
    fn unsubscribing_is_immediate_and_escape_leaves_the_detail() {
        let mut detail = detail(
            vec![
                row(3, TickSubscriptionFrequency::High),
                row(4, TickSubscriptionFrequency::High),
            ],
            SubscriptionOperation::Cancel,
        );
        press(&mut detail, KeyCode::Char('j'));
        assert_eq!(
            detail.apply_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE)),
            DetailStep::Cancel(Uuid::from_u128(4))
        );
        assert!(detail.confirmation().is_none());
        assert!(matches!(
            press(&mut detail, KeyCode::Esc),
            TickDetailKey::Back
        ));
    }

    #[test]
    fn a_single_subscription_cancels_without_confirmation() {
        let mut detail = detail(
            vec![row(3, TickSubscriptionFrequency::Immediate)],
            SubscriptionOperation::Cancel,
        );
        assert!(detail.confirmation().is_none());
        assert_eq!(requested(press(&mut detail, KeyCode::Char('u'))), Some('u'));
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
        press(&mut detail, KeyCode::Char('u'));
        assert!(detail.confirmation().is_none());

        detail.update(
            "Fixture run".into(),
            vec![row(3, TickSubscriptionFrequency::High)],
            HashMap::new(),
        );

        assert_eq!(text(&mut detail)[2], "› ↻ high · every 1 min");
        assert!(
            !detail.is_done(),
            "a detail opened to view stays while one is left"
        );
        detail.update("Fixture run".into(), Vec::new(), HashMap::new());
        assert!(
            detail.is_done(),
            "with none left Monitor goes back to its list"
        );
    }

    #[test]
    fn a_detail_the_rows_u_opened_is_done_once_its_cancel_goes_through_but_not_when_refused() {
        let mut detail = detail(
            vec![
                row(3, TickSubscriptionFrequency::High),
                row(4, TickSubscriptionFrequency::Low),
            ],
            SubscriptionOperation::Cancel,
        );
        assert_eq!(requested(press(&mut detail, KeyCode::Char('u'))), Some('u'));
        assert!(!detail.is_done(), "the cancel has not gone through yet");

        detail.report("Cannot update the Tick Subscription: refused".into());
        detail.update(
            "Fixture run".into(),
            vec![
                row(3, TickSubscriptionFrequency::High),
                row(4, TickSubscriptionFrequency::Low),
            ],
            HashMap::new(),
        );
        assert!(!detail.is_done(), "a refused cancel stays on the detail");
        assert_eq!(
            detail.hint(),
            "Cannot update the Tick Subscription: refused · esc back"
        );

        press(&mut detail, KeyCode::Char('u'));
        detail.update(
            "Fixture run".into(),
            vec![row(4, TickSubscriptionFrequency::Low)],
            HashMap::new(),
        );
        assert!(detail.is_done(), "the cancelled subscription is gone");
    }
}

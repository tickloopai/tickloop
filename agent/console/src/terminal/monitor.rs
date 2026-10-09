use std::collections::{BTreeSet, HashMap};

use tickhost_connect::page::{MAX_PAGE_SIZE, PageCursor, PageError};

use super::{
    display::{DisplayProfile, Glyph},
    list_row::{self, ListRow},
    style::{Role, RoleLine},
    subscription_detail::SubscriptionDetail,
};

/// Monitor's default column, its `│ ` separator included.
pub(super) const MONITOR_COLUMNS: usize = 42;
/// The narrowest Monitor column beside the conversation: 30 text columns still hold a row's
/// indented details line, such as `    feedback elsewhere · ● 59m`, whole.
const MONITOR_MIN_COLUMNS: usize = 32;
/// The narrowest conversation left beside Monitor: what the narrowest Monitor leaves of an
/// 80-column terminal, the product's minimum, once the console's reserved last column is taken.
/// Below it Monitor covers the conversation instead.
pub(super) const CONVERSATION_MIN_COLUMNS: usize = 47;
const MILLISECONDS_PER_SECOND: f64 = 1000.0;
const BYTES_PER_MIB: f64 = 1024.0 * 1024.0;
const EMPTY_MONITOR_TEXT: &str = "Nothing to monitor";

/// Monitor's column beside a content area `width` columns wide: the user's preferred width or
/// the default, narrowed so the conversation keeps its minimum, or `None` when even the narrowest
/// Monitor leaves the conversation too little room and Monitor can only open as an overlay.
pub(super) fn side_columns(width: usize, preferred: Option<usize>) -> Option<usize> {
    let spare = width.checked_sub(CONVERSATION_MIN_COLUMNS)?;
    (spare >= MONITOR_MIN_COLUMNS).then(|| {
        preferred
            .unwrap_or(MONITOR_COLUMNS)
            .clamp(MONITOR_MIN_COLUMNS, spare)
    })
}

/// The Monitor width a drag of its separator to `column` asks for, within the same bounds as
/// [`side_columns`]; `None` when the terminal has no room for Monitor beside the conversation.
pub(super) fn dragged_columns(width: usize, column: usize) -> Option<usize> {
    side_columns(width, Some(width.saturating_sub(column)))
}

pub(super) use crate::monitor_rows::TickGroup;
use crate::monitor_rows::{
    GONE_DISPLAY_MS, MONITOR_ADD_KEY, MonitorAction, MonitorContext, MonitorRowsRegistry,
    RowUpdate, StatusTone, TickDescriptor, TickGlyph, TickOwner, TickResources,
    finished_shell_until, page_of,
};
use tick_protocol::InstanceRef;
use tickloop_agent_protocol::{
    ChatSnapshot, MonitorMembership, ShellTaskSnapshot, TickSubscriptionStatus,
};

#[derive(Clone, Debug)]
pub(super) struct MonitorEntry {
    pub(super) instance: InstanceRef,
    pub(super) group: TickGroup,
    pub(super) glyph: Glyph,
    pub(super) title: String,
    pub(super) kind: String,
    /// The state in words, for the detail; the row shows it only as its glyph's colour.
    pub(super) status: String,
    /// The facts the row shows at its right before its latest update.
    pub(super) notes: Vec<String>,
    pub(super) owner: TickOwner,
    pub(super) tone: StatusTone,
    pub(super) updated_at_ms: Option<u64>,
    /// The latest update that counts for unread, and whether this Chat still has to handle it.
    pub(super) update: Option<RowUpdate>,
    pub(super) resources: Option<TickResources>,
    pub(super) actions: Vec<MonitorAction>,
}

impl MonitorEntry {
    fn status_spans(&self, now_ms: u64) -> Vec<(String, Role)> {
        list_row::status_spans(
            self.tone,
            &self.notes,
            self.update.as_ref().map(|update| list_row::UpdateMark {
                age: relative_time(Some(update.at_ms), now_ms),
                unread: update.unread,
            }),
            now_ms,
        )
    }

    fn state_label(&self) -> &str {
        match (self.status.is_empty(), self.tone) {
            (true, StatusTone::Running { .. }) => "running",
            (true, _) => "—",
            (false, _) => &self.status,
        }
    }
}

#[derive(Clone)]
pub(super) struct TickConfirmation {
    pub(super) instance: String,
    pub(super) action: std::sync::Arc<dyn super::tick_actions::TickActionExecutor>,
    pub(super) prompt: String,
}
#[derive(Default)]
pub(super) struct Monitor {
    pub(super) detail: Option<Box<dyn super::tick_actions::TickDetailView>>,
    pub(super) subscriptions: Vec<TickSubscriptionStatus>,
    pub(super) metadata: HashMap<String, Option<tickloop_agent_artifacts::ArtifactMetadata>>,
    pub(super) metadata_failures: HashMap<String, String>,
    pub(super) member_findings: crate::monitor_rows::MemberFindings,
    pub(super) confirmation: Option<TickConfirmation>,
    /// The visible page; the complete set is `total` rows.
    entries: Vec<MonitorEntry>,
    total: usize,
    group_totals: std::collections::BTreeMap<TickGroup, usize>,
    page_start: Option<PageCursor>,
    earlier_pages: Vec<Option<PageCursor>>,
    next_page: Option<PageCursor>,
    selected: Option<InstanceRef>,
    rows: Option<MonitorRowsRegistry>,
    pub(super) chat_snapshot: Option<ChatSnapshot>,
    pub(super) shell_snapshot: Option<ShellTaskSnapshot>,
    /// When each running Shell started, as far as the View knows.
    pub(super) shell_starts: HashMap<uuid::Uuid, u64>,
    pub(super) focused: bool,
    pub(super) overlay: bool,
    /// Whether the column has shown rows in this Chat; it then stays when empty, so the
    /// conversation does not change width each time a background Shell starts or ends.
    shown: bool,
    /// When time alone next changes the list: a finished Shell or a gone Tick leaving.
    refresh_at: Option<u64>,
    /// Since when each instance has shown as gone, as this View saw it.
    gone_since: HashMap<InstanceRef, u64>,
}

pub(super) struct MonitorFrame {
    pub(super) lines: Vec<RoleLine>,
    pub(super) targets: Vec<(usize, InstanceRef)>,
}

impl Monitor {
    pub(super) fn refresh_action(
        &mut self,
    ) -> std::sync::Arc<dyn super::tick_actions::TickActionExecutor> {
        let intents = self
            .rows
            .get_or_insert_with(MonitorRowsRegistry::builtin)
            .refresh_intents();
        super::tick_actions::builtin::refresh(&intents)
    }
    /// The rows of the visible page.
    pub(super) fn entries(&self) -> &[MonitorEntry] {
        &self.entries
    }
    /// The size of the Chat's complete deduplicated Monitor set, shown as `Monitor N`.
    pub(super) fn total(&self) -> usize {
        self.total
    }
    pub(super) fn selected(&self) -> Option<&MonitorEntry> {
        self.selected.as_ref().and_then(|instance| {
            self.entries
                .iter()
                .find(|entry| &entry.instance == instance)
        })
    }
    /// Instances this Chat opened or explicitly added, as opposed to rows other sources show.
    pub(super) fn members(&self) -> BTreeSet<InstanceRef> {
        self.chat_snapshot
            .iter()
            .flat_map(|chat| &chat.monitor.members)
            .map(|member| member.instance.clone())
            .collect()
    }
    /// Instances removed from this Chat's Monitor, which an explicit add restores.
    pub(super) fn removed(&self) -> BTreeSet<InstanceRef> {
        self.chat_snapshot
            .as_ref()
            .map(|chat| chat.monitor.removed.clone())
            .unwrap_or_default()
    }
    /// Whether the column has shown rows in this Chat, so it stays on screen when empty.
    pub(super) fn is_shown(&self) -> bool {
        self.shown
    }
    /// Rebuilds the list when time alone changed it, and says whether it did.
    pub(super) fn refresh_due(&mut self, now_ms: u64) -> bool {
        if self.refresh_at.is_none_or(|at| at > now_ms) {
            return false;
        }
        self.rebuild_at(now_ms);
        true
    }
    pub(super) fn rebuild(&mut self) {
        self.rebuild_at(tickloop_agent_base::private_fs::unix_time_milliseconds());
    }
    fn rebuild_at(&mut self, now_ms: u64) {
        let context = MonitorContext {
            chat: self.chat_snapshot.clone(),
            shells: self.shell_snapshot.clone(),
            subscriptions: self.subscriptions.clone(),
            metadata: self.metadata.clone(),
            metadata_failures: self.metadata_failures.clone(),
            member_states: self.member_findings.states.clone(),
            member_resources: self.member_findings.resources.clone(),
            member_titles: self.member_findings.titles.clone(),
            member_updates: self.member_findings.updates.clone(),
            gone_since: self.gone_since.clone(),
            shell_starts: self.shell_starts.clone(),
            retained: self.detail.as_ref().and_then(|detail| detail.instance()),
            now_ms,
        };
        let listed = self
            .rows
            .get_or_insert_with(MonitorRowsRegistry::builtin)
            .list(&context);
        // A row that left as gone stays out; one that shows again as anything else starts over.
        for entry in &listed {
            if entry.tone == StatusTone::Gone {
                self.gone_since
                    .entry(entry.instance.clone())
                    .or_insert(now_ms);
            } else {
                self.gone_since.remove(&entry.instance);
            }
        }
        self.group_totals.clear();
        for entry in &listed {
            *self.group_totals.entry(entry.group).or_default() += 1;
        }
        let page = match page_of(listed.clone(), self.page_start.as_ref(), None) {
            Ok(page) => page,
            Err(PageError::CursorExpired) => {
                // A changed set expired the cursor; the listing starts again at its first page.
                self.page_start = None;
                self.earlier_pages.clear();
                page_of(listed, None, None)
                    .expect("the first page of a deduplicated set is always valid")
            }
            Err(error) => {
                unreachable!("Monitor pages a deduplicated set with the default size: {error}")
            }
        };
        self.total = page.total;
        self.shown |= self.total > 0;
        self.next_page = page.next;
        self.entries = page.items.into_iter().map(MonitorEntry::from).collect();
        self.refresh_at = self.next_refresh_ms(now_ms);
        if let Some(detail) = self
            .detail
            .as_mut()
            .and_then(|detail| detail.as_any_mut().downcast_mut::<MonitorDetail>())
            && let Some(entry) = self
                .entries
                .iter()
                .find(|entry| entry.instance == detail.entry.instance)
        {
            detail.entry = entry.clone();
        }
        let goals = self.loop_goals();
        if let Some(detail) = self
            .detail
            .as_mut()
            .and_then(|detail| detail.as_any_mut().downcast_mut::<SubscriptionDetail>())
        {
            let instance = detail.instance().clone();
            detail.update(
                title_of(&self.entries, &instance),
                crate::monitor_rows::subscriptions_on(&self.subscriptions, &instance),
                goals,
            );
        }
        self.reconcile_selection();
    }
    /// When time alone next changes the list: the next finished Shell or gone Tick to leave.
    fn next_refresh_ms(&self, now_ms: u64) -> Option<u64> {
        let handled = |task: &tickloop_agent_protocol::ShellTask| {
            self.chat_snapshot
                .as_ref()
                .and_then(|chat| chat.monitor.handled_at(&InstanceRef::shell(task.id)))
        };
        self.shell_snapshot
            .iter()
            .flat_map(|snapshot| &snapshot.tasks)
            .filter_map(|task| finished_shell_until(task, handled(task)))
            .chain(
                self.gone_since
                    .values()
                    .map(|since| since.saturating_add(GONE_DISPLAY_MS)),
            )
            .filter(|until| *until > now_ms)
            .min()
    }
    /// When time alone next changes the drawn list without changing its rows: a running row's
    /// glyph blinking or its elapsed time ticking. `None` while nothing runs or a detail covers
    /// the list.
    pub(super) fn next_frame_ms(&self, now_ms: u64, display: DisplayProfile) -> Option<u64> {
        (self.detail.is_none()
            && self
                .entries
                .iter()
                .any(|entry| matches!(entry.tone, StatusTone::Running { .. })))
        .then(|| display.next_running_change_ms(now_ms))
    }
    /// The row's title, or the instance itself when no row shows it.
    pub(super) fn title_of(&self, instance: &InstanceRef) -> String {
        title_of(&self.entries, instance)
    }
    /// What each of the Chat's Loops is for, by Loop.
    pub(super) fn loop_goals(&self) -> HashMap<uuid::Uuid, String> {
        self.chat_snapshot
            .iter()
            .flat_map(|chat| &chat.loops)
            .map(|goal| (goal.id, goal.goal.clone()))
            .collect()
    }
    #[cfg(test)]
    pub(super) fn replace(&mut self, group: TickGroup, rows: Vec<MonitorEntry>) {
        self.entries.retain(|entry| entry.group != group);
        self.entries.extend(rows);
        self.entries.sort_by(|a, b| {
            crate::monitor_rows::display_order(
                (a.group, a.updated_at_ms, &a.instance),
                (b.group, b.updated_at_ms, &b.instance),
            )
        });
        self.entries.dedup_by(|a, b| a.instance == b.instance);
        self.total = self.entries.len();
        self.shown |= self.total > 0;
        self.group_totals.clear();
        for entry in &self.entries {
            *self.group_totals.entry(entry.group).or_default() += 1;
        }
        self.reconcile_selection();
    }
    fn reconcile_selection(&mut self) {
        if self.entries.is_empty()
            && !self
                .detail
                .as_ref()
                .is_some_and(|detail| detail.keeps_empty_monitor())
        {
            self.detail = None;
            self.focused = false;
            self.overlay = false;
        }
        if self.selected().is_none() {
            self.selected = self.entries.first().map(|entry| entry.instance.clone());
        }
    }
    /// Closes the open detail; an empty Monitor then releases focus back to the input. A finished
    /// Shell the detail kept listed leaves on the next clock tick.
    pub(super) fn close_detail(&mut self) {
        if self
            .detail
            .take()
            .is_some_and(|detail| detail.instance().is_some())
        {
            self.refresh_at = Some(0);
        }
        self.reconcile_selection();
    }
    pub(super) fn select(&mut self, instance: &InstanceRef) {
        if self.entries.iter().any(|entry| &entry.instance == instance) {
            self.selected = Some(instance.clone());
        }
    }
    pub(super) fn focus(&mut self, overlay: bool, group: Option<TickGroup>) {
        self.focused = true;
        self.overlay = overlay;
        if let Some(group) = group {
            self.show_group(group);
        }
        if self.selected.is_none() {
            self.selected = self.entries.first().map(|entry| entry.instance.clone());
        }
    }
    /// Selects the group's first row, turning to the page that holds it.
    fn show_group(&mut self, group: TickGroup) {
        if !self.group_totals.contains_key(&group) {
            return;
        }
        if !self.entries.iter().any(|entry| entry.group == group) {
            self.page_start = None;
            self.earlier_pages.clear();
            self.rebuild();
            while !self.entries.iter().any(|entry| entry.group == group)
                && let Some(next) = self.next_page.take()
            {
                self.earlier_pages.push(self.page_start.replace(next));
                self.rebuild();
            }
        }
        if let Some(entry) = self.entries.iter().find(|entry| entry.group == group) {
            self.selected = Some(entry.instance.clone());
        }
    }
    /// Moves within the page; past its last or first row the next or earlier page opens.
    pub(super) fn move_selection(&mut self, delta: isize) {
        let index = self
            .entries
            .iter()
            .position(|entry| Some(&entry.instance) == self.selected.as_ref())
            .unwrap_or(0);
        let target = index as isize + delta;
        if target >= self.entries.len() as isize
            && let Some(next) = self.next_page.take()
        {
            self.earlier_pages.push(self.page_start.replace(next));
            self.rebuild();
            self.selected = self.entries.first().map(|entry| entry.instance.clone());
            return;
        }
        if target < 0
            && let Some(earlier) = self.earlier_pages.pop()
        {
            self.page_start = earlier;
            self.rebuild();
            self.selected = self.entries.last().map(|entry| entry.instance.clone());
            return;
        }
        if let Some(entry) = self
            .entries
            .get(target.clamp(0, self.entries.len().saturating_sub(1) as isize) as usize)
        {
            self.selected = Some(entry.instance.clone());
        }
    }
    /// Applies a membership the Agent just confirmed, so a removed row leaves at once.
    pub(super) fn set_membership(&mut self, membership: MonitorMembership) {
        if let Some(chat) = self.chat_snapshot.as_mut() {
            chat.monitor = membership;
        }
        self.rebuild();
    }
    /// The members a host probe can answer for: those no subscription already reports on.
    pub(super) fn probe_targets(&self, tick_type: &str) -> Vec<InstanceRef> {
        self.members()
            .into_iter()
            .filter(|instance| {
                instance.tick_type() == tick_type
                    && !self
                        .subscriptions
                        .iter()
                        .any(|row| &row.instance == instance)
            })
            .collect()
    }
    #[cfg(test)]
    pub(super) fn lines(
        &self,
        width: usize,
        height: usize,
        display: DisplayProfile,
        now_ms: u64,
    ) -> Vec<RoleLine> {
        self.frame(width, height, display, now_ms).lines
    }

    /// The keys of the focused list: the selected row's actions, adding, and Esc.
    pub(super) fn hint(&self, can_add: bool) -> String {
        let mut hints = self
            .selected()
            .map(|entry| {
                entry
                    .actions
                    .iter()
                    .map(|action| {
                        format!(
                            "{} {}",
                            if action.key == crate::monitor_rows::MONITOR_ENTER_KEY {
                                "enter".into()
                            } else {
                                action.key.to_string()
                            },
                            action.label
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if can_add {
            hints.push(format!("{MONITOR_ADD_KEY} add"));
        }
        hints.push("esc input".into());
        hints.join(" · ")
    }

    pub(super) fn frame(
        &self,
        width: usize,
        height: usize,
        display: DisplayProfile,
        now_ms: u64,
    ) -> MonitorFrame {
        let heading = if self.total > self.entries.len() {
            let first = self.earlier_pages.len() * MAX_PAGE_SIZE + 1;
            format!(
                "Monitor {first}–{} of {}",
                first + self.entries.len().saturating_sub(1),
                self.total
            )
        } else {
            "Monitor".into()
        };
        // The column's title stands out from its muted group headings.
        let mut lines = vec![RoleLine::role(
            heading,
            if self.focused {
                Role::Title
            } else {
                Role::Text
            },
        )];

        let mut rows = Vec::new();
        let mut row_ids = Vec::new();
        let mut selected_row = 0;
        let titles = self
            .entries
            .iter()
            .map(|entry| super::safe_line(&entry.title))
            .collect::<Vec<_>>();
        let list_rows = self
            .entries
            .iter()
            .zip(&titles)
            .map(|(entry, title)| ListRow {
                selected: self.focused && self.selected.as_ref() == Some(&entry.instance),
                glyph: display.glyph(entry.glyph),
                glyph_role: list_row::state_mark(entry.tone, display, now_ms),
                name: title,
                status: entry.status_spans(now_ms),
            })
            .collect::<Vec<_>>();
        let groups = self
            .entries
            .iter()
            .map(|entry| entry.group)
            .collect::<std::collections::BTreeSet<_>>();
        for group in groups {
            let entries: Vec<_> = self
                .entries
                .iter()
                .zip(&list_rows)
                .filter(|(entry, _)| entry.group == group)
                .collect();
            rows.push(RoleLine::role(
                format!(
                    "{} ({})",
                    group.title(),
                    self.group_totals
                        .get(&group)
                        .copied()
                        .unwrap_or(entries.len())
                ),
                Role::MutedText,
            ));
            row_ids.push(None);
            for (entry, row) in entries {
                if row.selected {
                    selected_row = rows.len();
                }
                for line in row.lines(width) {
                    rows.push(line);
                    row_ids.push(Some(entry.instance.clone()));
                }
            }
        }
        if self.entries.is_empty() {
            rows.push(RoleLine::role(EMPTY_MONITOR_TEXT, Role::MutedText));
            row_ids.push(None);
        }
        let capacity = height.saturating_sub(1);
        let offset = selected_row.saturating_sub(capacity.saturating_sub(2));
        let targets = row_ids
            .into_iter()
            .skip(offset)
            .take(capacity)
            .enumerate()
            .filter_map(|(row, id)| id.map(|id| (row + 1, id)))
            .collect();
        lines.extend(rows.into_iter().skip(offset).take(capacity));
        lines.truncate(height);
        for line in &mut lines {
            line.truncate(width);
        }
        MonitorFrame { lines, targets }
    }
}

/// The public facts of one Monitor row. Opening it never reads, subscribes or calls the model.
pub(super) struct MonitorDetail {
    id: uuid::Uuid,
    chat: uuid::Uuid,
    entry: MonitorEntry,
}

impl MonitorDetail {
    pub(super) fn new(chat: uuid::Uuid, entry: MonitorEntry) -> Self {
        Self {
            id: uuid::Uuid::new_v4(),
            chat,
            entry,
        }
    }
}

/// The public facts of one Monitor row: what the Chat recorded and the latest process sample.
pub(super) fn member_facts(entry: &MonitorEntry, now_ms: u64) -> Vec<String> {
    let mut facts = vec![
        super::safe_line(&entry.title),
        format!("Instance  {}", super::safe_line(entry.instance.as_str())),
        format!("Kind      {}", entry.kind),
        format!("State     {}", entry.state_label()),
        format!("Owner     {}", entry.owner.label()),
        format!("Updated   {}", relative_time(entry.updated_at_ms, now_ms)),
    ];
    match &entry.resources {
        Some(sample) => {
            let unavailable = || "unavailable".to_owned();
            facts.push(format!("PID       {}", sample.pid));
            facts.push(format!(
                "CPU time  {}",
                sample.cpu_time_ms.map_or_else(unavailable, |ms| format!(
                    "{:.1}s",
                    ms as f64 / MILLISECONDS_PER_SECOND
                ))
            ));
            facts.push(format!(
                "CPU       {}",
                sample
                    .cpu_percent
                    .map_or_else(unavailable, |percent| format!("{percent:.1}%"))
            ));
            facts.push(format!(
                "RSS       {}",
                sample.rss_bytes.map_or_else(unavailable, |bytes| format!(
                    "{:.1} MiB",
                    bytes as f64 / BYTES_PER_MIB
                ))
            ));
            facts.push(format!(
                "Sampled   {}",
                relative_time(Some(sample.sampled_at_ms), now_ms)
            ));
        }
        None => facts.push("Resources unavailable".into()),
    }
    facts
}

impl super::tick_actions::TickDetailView for MonitorDetail {
    fn id(&self) -> uuid::Uuid {
        self.id
    }
    fn chat(&self) -> uuid::Uuid {
        self.chat
    }
    fn lines(
        &mut self,
        width: usize,
        height: usize,
        now: u64,
    ) -> Vec<super::tick_actions::TickDetailLine> {
        member_facts(&self.entry, now)
            .into_iter()
            .take(height)
            .map(|text| {
                let mut line = super::tick_actions::TickDetailLine::new(text);
                line.line.truncate(width);
                line
            })
            .collect()
    }
    fn hint(&self) -> String {
        "esc back".into()
    }
    fn key(
        &mut self,
        key: crossterm::event::KeyEvent,
        _width: usize,
        _height: usize,
    ) -> super::tick_actions::TickDetailKey {
        match key.code {
            crossterm::event::KeyCode::Esc | crossterm::event::KeyCode::Left => {
                super::tick_actions::TickDetailKey::Back
            }
            _ => super::tick_actions::TickDetailKey::None,
        }
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

fn title_of(entries: &[MonitorEntry], instance: &InstanceRef) -> String {
    entries
        .iter()
        .find(|entry| &entry.instance == instance)
        .map_or_else(|| instance.to_string(), |entry| entry.title.clone())
}

/// Breaks `text` between words so each line fits `width` columns; a longer word is cut later.
pub(super) fn wrap_words(text: &str, width: usize) -> Vec<String> {
    use unicode_width::UnicodeWidthStr;
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split(' ') {
        if !line.is_empty() && line.width() + 1 + word.width() > width {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    lines.push(line);
    lines
}

pub(super) fn relative_time(updated: Option<u64>, now: u64) -> String {
    let Some(updated) = updated else {
        return "—".into();
    };
    let seconds = now.saturating_sub(updated) / 1000;
    match seconds {
        0 => "now".into(),
        1..60 => format!("{seconds}s"),
        60..3600 => format!("{}m", seconds / 60),
        3600..86400 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hints_break_between_words_to_fit_the_monitor() {
        assert_eq!(
            wrap_words("enter open · s snapshot · x remove · esc", 20),
            ["enter open · s", "snapshot · x remove", "· esc"]
        );
    }

    #[test]
    fn elapsed_update_time_uses_the_specified_units() {
        for (updated, expected) in [
            (None, "—"),
            (Some(90000000), "now"),
            (Some(89955000), "45s"),
            (Some(89820000), "3m"),
            (Some(82800000), "2h"),
            (Some(3600000), "1d"),
        ] {
            assert_eq!(relative_time(updated, 90000000), expected);
        }
    }
}

impl From<TickDescriptor> for MonitorEntry {
    fn from(row: TickDescriptor) -> Self {
        Self {
            instance: row.instance,
            group: row.group,
            glyph: match row.glyph {
                TickGlyph::Artifact => Glyph::ArtifactTick,
                TickGlyph::Shell => Glyph::ShellTick,
                TickGlyph::Protocol => Glyph::ProtocolTick,
                TickGlyph::Terminal => Glyph::TerminalTick,
            },
            title: row.title,
            kind: row.kind,
            status: row.status,
            notes: row.notes,
            owner: row.owner,
            tone: row.tone,
            updated_at_ms: row.updated_at_ms,
            update: row.update,
            resources: row.resources,
            actions: row.actions,
        }
    }
}

#[cfg(test)]
mod registered_rows_tests {
    use super::*;
    use crate::monitor_rows::{MonitorRows, TickIntent};
    use std::sync::Arc;

    struct Calendar;
    impl MonitorRows for Calendar {
        fn tick_type(&self) -> &'static str {
            "calendar"
        }
        fn list(&self, _: &MonitorContext) -> Vec<TickDescriptor> {
            vec![TickDescriptor {
                instance: tick_protocol::InstanceRef::new("calendar", "earnings").unwrap(),
                group: TickGroup {
                    order: 4,
                    name: "Calendars",
                },
                glyph: TickGlyph::Protocol,
                title: "Earnings calendar".into(),
                kind: "Calendar".into(),
                status: "waiting".into(),
                notes: vec!["3 events".into()],
                inherent_subscription: false,
                owner: crate::monitor_rows::TickOwner::Unknown,
                tone: crate::monitor_rows::StatusTone::Neutral,
                updated_at_ms: None,
                update: None,
                running: true,
                resources: None,
                actions: [('o', "inspect"), ('z', "expand")]
                    .into_iter()
                    .map(|(key, label)| MonitorAction {
                        key,
                        label,
                        intent: TickIntent::Notify("opened".into()),
                    })
                    .collect(),
            }]
        }
    }
    #[test]
    fn registered_rows_group_and_hints_render_without_list_type_changes() {
        let mut list = Monitor {
            rows: Some(
                MonitorRowsRegistry::new([Arc::new(Calendar) as Arc<dyn MonitorRows>]).unwrap(),
            ),
            ..Default::default()
        };
        list.rebuild();
        list.focus(false, None);
        let text = list
            .lines(80, 20, DisplayProfile::default(), 0)
            .iter()
            .map(RoleLine::text)
            .collect::<Vec<_>>()
            .join("\n");
        for expected in ["Calendars (1)", "Earnings calendar", "3 events"] {
            assert!(text.contains(expected), "{text}");
        }
        assert!(
            !text.contains("waiting"),
            "the state is the glyph's colour: {text}"
        );
        assert!(!text.contains("Subscribed"));
        assert!(!text.contains("enter"), "keys are on the hint row: {text}");
        assert_eq!(list.hint(false), "enter inspect · z expand · esc input");
    }
}

use std::{
    collections::{BTreeSet, HashMap},
    sync::Arc,
};

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
/// indented details line, such as `    feedback elsewhere · ●`, whole.
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
    GONE_DISPLAY_MS, MonitorAction, MonitorContext, MonitorRowsRegistry, RowUpdate, StatusTone,
    SubscriptionMark, TickDescriptor, TickGlyph, TickOwner, TickResources, page_of,
};
use tick_protocol::InstanceRef;
use tickloop_agent_protocol::{
    ChatStatus, MonitorMembership, ShellTaskSnapshot, TickSubscriptionStatus,
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
    /// The facts the row shows at its right before its marks.
    pub(super) notes: Vec<String>,
    pub(super) subscription: Option<SubscriptionMark>,
    pub(super) owner: TickOwner,
    pub(super) tone: StatusTone,
    pub(super) updated_at_ms: Option<u64>,
    /// The latest update that counts for unread, and whether this Chat still has to handle it.
    pub(super) update: Option<RowUpdate>,
    pub(super) resources: Option<TickResources>,
    pub(super) actions: Vec<MonitorAction>,
}

impl MonitorEntry {
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
    pub(super) option: Option<ConfirmationOption>,
}

impl TickConfirmation {
    /// The action confirming runs: without the option when the user turned it off.
    pub(super) fn chosen_action(
        &self,
    ) -> std::sync::Arc<dyn super::tick_actions::TickActionExecutor> {
        match &self.option {
            Some(option) if !option.is_chosen => option.action_without.clone(),
            _ => self.action.clone(),
        }
    }
}

/// A choice a confirmation offers, which the user can turn off before confirming.
#[derive(Clone)]
pub(crate) struct ConfirmationOption {
    pub(crate) label: String,
    pub(crate) is_chosen: bool,
    /// What confirming runs once the option is off.
    pub(crate) action_without: std::sync::Arc<dyn super::tick_actions::TickActionExecutor>,
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
    pub(super) chat_status: Option<Arc<ChatStatus>>,
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
    /// Since when this View has shown this Chat's Monitor; an Agent turn taking an update in
    /// earlier never flashes its row.
    flashes_since: Option<u64>,
    /// The update each row last listed as handled; taking the same update in again, such as a
    /// second read of it, does not flash the row.
    handled_updates: HashMap<InstanceRef, u64>,
    /// When an Agent turn took in each row's update while this View showed the Chat; the row
    /// flashes from then on.
    taken_in: HashMap<InstanceRef, u64>,
    /// The latest update of each row as this View last listed it, `None` for a row without one.
    /// A row's first listing is its baseline, so only an update that comes after it is new.
    listed_updates: HashMap<InstanceRef, Option<u64>>,
    /// Since when each row's glyph flashes for the new update this View last listed.
    update_flashes: HashMap<InstanceRef, u64>,
}

/// What one status refresh reads for Monitor.
pub(super) struct MonitorSources {
    pub(super) chat: Arc<ChatStatus>,
    pub(super) shells: ShellTaskSnapshot,
    /// `None` when they could not be read; the rows keep the previous ones.
    pub(super) subscriptions: Option<Vec<TickSubscriptionStatus>>,
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
    /// Instances this Chat opened, as opposed to rows other sources show.
    fn members(&self) -> BTreeSet<InstanceRef> {
        self.chat_status
            .iter()
            .flat_map(|chat| &chat.monitor.members)
            .map(|member| member.instance.clone())
            .collect()
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
    /// Takes the Chat's newest status and rebuilds the list from it.
    pub(super) fn set_chat_status(&mut self, status: Arc<ChatStatus>) {
        self.adopt_chat_status(status);
        self.rebuild();
    }
    /// Takes everything one status refresh read at once, so the list is rebuilt only once. The
    /// Chat status carries Monitor membership and removals, so the Shell and subscription rows it
    /// lists are never shown against an older one.
    pub(super) fn refresh_sources(&mut self, sources: MonitorSources) {
        self.adopt_chat_status(sources.chat);
        self.refresh_ticks(sources.shells, sources.subscriptions);
    }
    /// Takes the Chat's Shell tasks and subscriptions, which change without the Chat itself
    /// changing, and rebuilds the list once against the Chat status it already has.
    pub(super) fn refresh_ticks(
        &mut self,
        shells: ShellTaskSnapshot,
        subscriptions: Option<Vec<TickSubscriptionStatus>>,
    ) {
        self.shell_starts = self
            .chat_status
            .as_deref()
            .map(|chat| super::task_access::shell_starts(chat, &shells))
            .unwrap_or_default();
        self.shell_snapshot = Some(shells);
        if let Some(subscriptions) = subscriptions {
            self.subscriptions = subscriptions;
        }
        self.rebuild();
    }
    /// An artifact title that changed in the Chat since the previous status, such as after the
    /// Agent's own rename, is newer than the one the last metadata read cached, so it replaces
    /// that one.
    fn adopt_chat_status(&mut self, status: Arc<ChatStatus>) {
        let previous = self.chat_status.take();
        for tick in &status.artifact_ticks {
            let id = tick.instance_id();
            let is_renamed = previous.as_ref().is_some_and(|previous| {
                previous
                    .artifact_ticks
                    .iter()
                    .any(|known| known.instance_id() == id && known.title != tick.title)
            });
            if is_renamed && let Some(Some(metadata)) = self.metadata.get_mut(&id) {
                metadata.title = tick.title.clone();
            }
        }
        self.chat_status = Some(status);
    }
    pub(super) fn rebuild(&mut self) {
        self.rebuild_at(tickloop_agent_base::private_fs::unix_time_milliseconds());
    }
    fn rebuild_at(&mut self, now_ms: u64) {
        let context = MonitorContext {
            chat: self.chat_status.clone(),
            shells: self.shell_snapshot.clone(),
            subscriptions: self.subscriptions.clone(),
            metadata: self.metadata.clone(),
            metadata_failures: self.metadata_failures.clone(),
            member_states: self.member_findings.states.clone(),
            member_resources: self.member_findings.resources.clone(),
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
        self.note_flashes(&listed, now_ms);
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
    /// Notes what flashes: the glyph of a row whose new update just came, and the whole row whose
    /// update an Agent turn has just taken in. A row the user opened does not flash, nor one taken
    /// in before this View showed the Chat, nor an update the row already showed handled that a
    /// turn takes in again.
    fn note_flashes(&mut self, listed: &[TickDescriptor], now_ms: u64) {
        let flashes_since = *self.flashes_since.get_or_insert(now_ms);
        let mut handled_updates = HashMap::new();
        let mut listed_updates = HashMap::new();
        for entry in listed {
            let at_ms = entry.update.as_ref().map(|update| update.at_ms);
            if self
                .listed_updates
                .get(&entry.instance)
                .is_some_and(|listed| at_ms > *listed)
            {
                self.update_flashes.insert(entry.instance.clone(), now_ms);
            }
            listed_updates.insert(entry.instance.clone(), at_ms);
            let Some(update) = entry
                .update
                .as_ref()
                .filter(|update| update.unread.is_none())
            else {
                continue;
            };
            if let Some(taken_in) = update.taken_in_at_ms
                && taken_in >= flashes_since
                && self.handled_updates.get(&entry.instance) != Some(&update.at_ms)
            {
                self.taken_in.insert(entry.instance.clone(), taken_in);
            }
            handled_updates.insert(entry.instance.clone(), update.at_ms);
        }
        self.taken_in
            .retain(|instance, _| handled_updates.contains_key(instance));
        self.update_flashes
            .retain(|instance, _| listed_updates.contains_key(instance));
        self.handled_updates = handled_updates;
        self.listed_updates = listed_updates;
    }
    /// Ends the flashes under way and lets only later updates and take-ins flash, as when this
    /// Chat's Monitor comes back into view: what came or what an Agent turn took in while the View
    /// showed another Chat does not replay.
    pub(super) fn settle_flashes(&mut self, now_ms: u64) {
        self.taken_in.clear();
        self.update_flashes.clear();
        self.listed_updates.clear();
        self.flashes_since = Some(now_ms);
    }
    /// When time alone next changes the list: the next finished Shell or gone Tick to leave.
    fn next_refresh_ms(&self, now_ms: u64) -> Option<u64> {
        let handled = |task: &tickloop_agent_protocol::ShellTask| {
            self.chat_status
                .as_ref()
                .and_then(|chat| chat.monitor.handled_at(&InstanceRef::shell(task.id)))
        };
        self.shell_snapshot
            .iter()
            .flat_map(|snapshot| &snapshot.tasks)
            .filter_map(|task| task.finished_row_until(handled(task)))
            .chain(
                self.gone_since
                    .values()
                    .map(|since| since.saturating_add(GONE_DISPLAY_MS)),
            )
            .filter(|until| *until > now_ms)
            .min()
    }
    /// When time alone next changes the list drawn `width` columns wide without changing its
    /// rows: a running row's glyph blinking or its elapsed time ticking, a row or its glyph
    /// flashing, or a countdown filling one more column. `None` while nothing runs, flashes or
    /// counts down, or a detail covers the list.
    pub(super) fn next_frame_ms(
        &self,
        now_ms: u64,
        width: usize,
        display: DisplayProfile,
    ) -> Option<u64> {
        if self.detail.is_some() {
            return None;
        }
        let running = self
            .entries
            .iter()
            .any(|entry| matches!(entry.tone, StatusTone::Running { .. }))
            .then(|| display.next_running_change_ms(now_ms));
        let flashing = self
            .entries
            .iter()
            .flat_map(|entry| {
                [
                    self.taken_in.get(&entry.instance),
                    self.update_flashes.get(&entry.instance),
                ]
            })
            .flatten()
            .filter_map(|since| display.next_flash_change_ms(*since, now_ms));
        let counting_down = self
            .entries
            .iter()
            .filter_map(|entry| entry.subscription?.next_check)
            .filter_map(|check| check.next_column_at(width, now_ms));
        running
            .into_iter()
            .chain(flashing)
            .chain(counting_down)
            .min()
    }
    /// The row's title, or the instance itself when no row shows it.
    pub(super) fn title_of(&self, instance: &InstanceRef) -> String {
        title_of(&self.entries, instance)
    }
    /// What each of the Chat's Loops is for, by Loop.
    pub(super) fn loop_goals(&self) -> HashMap<uuid::Uuid, String> {
        self.chat_status
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
        if self.entries.is_empty() {
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
        if let Some(chat) = self.chat_status.as_mut() {
            Arc::make_mut(chat).monitor = membership;
        }
        self.rebuild();
    }
    /// The members a host probe can answer for: those no subscription already reports on.
    pub(super) fn probe_targets(&self, access: &str) -> Vec<InstanceRef> {
        self.members()
            .into_iter()
            .filter(|instance| {
                instance.access() == access
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

    /// The keys of the focused list: the selected row's actions and Esc.
    pub(super) fn hint(&self) -> String {
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
                glyph_role: list_row::glyph_mark(
                    entry.tone,
                    self.update_flashes.get(&entry.instance).copied(),
                    display,
                    now_ms,
                ),
                name: title,
                status: list_row::status_spans(
                    entry.tone,
                    &entry.notes,
                    list_row::RowMarks {
                        subscription: entry.subscription,
                        unread: entry.update.as_ref().and_then(|update| update.unread),
                    },
                    display,
                    now_ms,
                ),
                fill: list_row::flash_fill(
                    self.taken_in.get(&entry.instance).copied(),
                    display,
                    now_ms,
                ),
                countdown_columns: entry
                    .subscription
                    .and_then(|mark| mark.next_check)
                    .map(|check| check.elapsed_columns(width, now_ms)),
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
            subscription: row.subscription,
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
        fn access(&self) -> &'static str {
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
                subscription: None,
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
        assert_eq!(list.hint(), "enter inspect · z expand · esc input");
    }
}

#[cfg(test)]
mod signal_tests {
    use super::*;
    use crate::monitor_rows::{MonitorRows, NextCheck, RowUpdate};
    use std::sync::{Arc, Mutex};
    use tickloop_agent_config::DisplayPreferences;
    use tickloop_agent_protocol::{
        LoopState, TickLinkState, TickSubscriptionFrequency, TickSubscriptionStatus,
    };

    const UPDATED_AT: u64 = 10_000;
    const HANDLED_AT: u64 = 10_500;
    const WIDTH: usize = 40;

    fn quotes() -> InstanceRef {
        InstanceRef::new("calendar", "quotes").unwrap()
    }

    /// One live row whose latest update came at the time it holds, if any.
    struct Updated(Arc<Mutex<Option<u64>>>);
    impl MonitorRows for Updated {
        fn access(&self) -> &'static str {
            "calendar"
        }
        fn list(&self, _: &MonitorContext) -> Vec<TickDescriptor> {
            let at = *self.0.lock().unwrap();
            vec![TickDescriptor {
                instance: quotes(),
                group: TickGroup::Ticks,
                glyph: TickGlyph::Protocol,
                title: "Quotes".into(),
                kind: "Tick".into(),
                status: "live".into(),
                notes: Vec::new(),
                subscription: None,
                owner: TickOwner::Unknown,
                tone: StatusTone::Live,
                updated_at_ms: at,
                update: at.map(RowUpdate::new),
                running: true,
                resources: None,
                actions: Vec::new(),
            }]
        }
    }

    /// A Monitor of the one row, updated at [`UPDATED_AT`], and the hand that moves its update.
    fn updated_monitor(chat: Option<ChatStatus>) -> (Monitor, Arc<Mutex<Option<u64>>>) {
        let update = Arc::new(Mutex::new(Some(UPDATED_AT)));
        let monitor = Monitor {
            rows: Some(
                MonitorRowsRegistry::new([
                    Arc::new(Updated(update.clone())) as Arc<dyn MonitorRows>
                ])
                .unwrap(),
            ),
            chat_status: chat.map(Arc::new),
            ..Default::default()
        };
        (monitor, update)
    }

    fn chat() -> ChatStatus {
        ChatStatus::of(
            &serde_json::from_value(serde_json::json!({
                "id": uuid::Uuid::from_u128(1),
                "workdir": "/fixture",
                "messages": [],
                "loops": []
            }))
            .unwrap(),
        )
    }

    fn chat_handled(mark: fn(&mut MonitorMembership, InstanceRef, u64) -> bool) -> ChatStatus {
        let mut chat = chat();
        mark(&mut chat.monitor, quotes(), HANDLED_AT);
        chat
    }

    /// A Monitor that listed the row unread, then learnt how the Chat handled it.
    fn handled_while_listed(mark: fn(&mut MonitorMembership, InstanceRef, u64) -> bool) -> Monitor {
        let (mut monitor, _) = updated_monitor(Some(chat()));
        monitor.rebuild_at(UPDATED_AT + 100);
        monitor.chat_status = Some(Arc::new(chat_handled(mark)));
        monitor.rebuild_at(HANDLED_AT + 50);
        monitor
    }

    /// The row's line: the frame's heading and group title come first.
    fn row(monitor: &Monitor, display: DisplayProfile, now_ms: u64) -> RoleLine {
        monitor.frame(WIDTH, 10, display, now_ms).lines[2].clone()
    }

    fn fill(monitor: &Monitor, display: DisplayProfile, now_ms: u64) -> Option<Role> {
        row(monitor, display, now_ms).fill
    }

    /// The role of the row's leading glyph.
    fn glyph(monitor: &Monitor, display: DisplayProfile, now_ms: u64) -> Option<Role> {
        row(monitor, display, now_ms)
            .spans
            .into_iter()
            .find(|span| span.text == display.glyph(Glyph::ProtocolTick))
            .and_then(|span| span.role)
    }

    /// The role of the row's unread mark, `None` without one.
    fn unread_mark(monitor: &Monitor, display: DisplayProfile, now_ms: u64) -> Option<Role> {
        row(monitor, display, now_ms)
            .spans
            .into_iter()
            .find(|span| span.text == "●")
            .and_then(|span| span.role)
    }

    fn reduced_motion() -> DisplayProfile {
        DisplayProfile::new(DisplayPreferences {
            prefers_reduced_motion: true,
            ..Default::default()
        })
    }

    #[test]
    fn an_unread_update_shows_its_mark_without_a_fill_or_a_time() {
        let (mut monitor, _) = updated_monitor(Some(chat()));
        monitor.rebuild_at(UPDATED_AT + 100);
        let animated = DisplayProfile::default();
        for now in [UPDATED_AT + 100, UPDATED_AT + 500, UPDATED_AT + 60_000] {
            let line = row(&monitor, animated, now);
            assert_eq!(line.fill, None, "no unread fill: {line:?}");
            assert_eq!(line.lead_fill, None);
            assert!(line.text().trim_end().ends_with('●'), "{line:?}");
            assert_eq!(unread_mark(&monitor, animated, now), Some(Role::UnreadMark));
        }
        assert_eq!(
            monitor.next_frame_ms(UPDATED_AT + 100, WIDTH, animated),
            None,
            "an update the View found on its first listing does not flash"
        );
    }

    #[test]
    fn each_new_update_flashes_the_glyph_while_the_unread_mark_stays_still() {
        let (mut monitor, update) = updated_monitor(Some(chat()));
        monitor.rebuild_at(UPDATED_AT + 100);
        let animated = DisplayProfile::default();
        for next in [UPDATED_AT + 2_000, UPDATED_AT + 4_000] {
            *update.lock().unwrap() = Some(next);
            let seen = next + 200;
            monitor.rebuild_at(seen);
            assert_eq!(
                [0, 400, 800, 1_200].map(|elapsed| glyph(&monitor, animated, seen + elapsed)),
                [
                    Some(Role::RunningMark),
                    Some(Role::RunningMarkDim),
                    Some(Role::RunningMark),
                    Some(Role::SuccessMark)
                ],
                "the update at {next} flashes the glyph, then its state colour returns"
            );
            for elapsed in [0, 400] {
                assert_eq!(
                    unread_mark(&monitor, animated, seen + elapsed),
                    Some(Role::UnreadMark),
                    "the unread mark does not flash"
                );
            }
            assert_eq!(
                fill(&monitor, animated, seen),
                None,
                "the row does not flash"
            );
            assert_eq!(
                monitor.next_frame_ms(seen + 900, WIDTH, animated),
                Some(seen + 1_200)
            );
            assert_eq!(monitor.next_frame_ms(seen + 1_200, WIDTH, animated), None);
        }
        monitor.rebuild_at(UPDATED_AT + 6_000);
        assert_eq!(
            monitor.next_frame_ms(UPDATED_AT + 6_000, WIDTH, animated),
            None,
            "listing the same update again does not flash it again"
        );

        let (mut still, update) = updated_monitor(Some(chat()));
        still.rebuild_at(UPDATED_AT + 100);
        *update.lock().unwrap() = Some(UPDATED_AT + 2_000);
        still.rebuild_at(UPDATED_AT + 2_200);
        assert_eq!(
            glyph(&still, reduced_motion(), UPDATED_AT + 2_200),
            Some(Role::SuccessMark),
            "under Reduce motion the glyph never flashes"
        );
        assert_eq!(
            still.next_frame_ms(UPDATED_AT + 2_200, WIDTH, reduced_motion()),
            None
        );
    }

    #[test]
    fn a_turn_taking_the_update_in_flashes_the_row_and_removes_the_mark() {
        let monitor = handled_while_listed(MonitorMembership::mark_taken_in);
        let animated = DisplayProfile::default();
        let fills = [0, 399, 400, 799, 800, 1_199, 1_200, 5_000]
            .map(|elapsed| fill(&monitor, animated, HANDLED_AT + elapsed));
        assert_eq!(
            fills,
            [
                Some(Role::FlashStrong),
                Some(Role::FlashStrong),
                Some(Role::FlashSoft),
                Some(Role::FlashSoft),
                Some(Role::FlashStrong),
                Some(Role::FlashStrong),
                None,
                None
            ]
        );
        assert_eq!(unread_mark(&monitor, animated, HANDLED_AT + 50), None);
        assert_eq!(
            monitor.next_frame_ms(HANDLED_AT + 50, WIDTH, animated),
            Some(HANDLED_AT + 400)
        );
        assert_eq!(
            monitor.next_frame_ms(HANDLED_AT + 900, WIDTH, animated),
            Some(HANDLED_AT + 1_200),
            "the end of the flash is its last frame"
        );
        assert_eq!(
            monitor.next_frame_ms(HANDLED_AT + 1_200, WIDTH, animated),
            None,
            "a finished flash schedules nothing"
        );
    }

    #[test]
    fn under_reduce_motion_a_taken_in_row_loses_its_mark_without_a_flash() {
        let monitor = handled_while_listed(MonitorMembership::mark_taken_in);
        assert_eq!(fill(&monitor, reduced_motion(), HANDLED_AT + 50), None);
        assert_eq!(
            unread_mark(&monitor, reduced_motion(), HANDLED_AT + 50),
            None
        );
        assert_eq!(
            monitor.next_frame_ms(HANDLED_AT + 50, WIDTH, reduced_motion()),
            None
        );
    }

    #[test]
    fn the_user_opening_the_row_removes_the_mark_without_a_flash() {
        let monitor = handled_while_listed(MonitorMembership::mark_handled);
        let animated = DisplayProfile::default();
        assert_eq!(fill(&monitor, animated, HANDLED_AT + 50), None);
        assert_eq!(unread_mark(&monitor, animated, HANDLED_AT + 50), None);
        assert_eq!(
            monitor.next_frame_ms(HANDLED_AT + 50, WIDTH, animated),
            None
        );
    }

    #[test]
    fn a_take_in_from_before_the_view_showed_the_chat_never_flashes() {
        let animated = DisplayProfile::default();
        let (mut opened_after, _) = updated_monitor(None);
        opened_after.rebuild_at(HANDLED_AT + 50);
        opened_after.chat_status = Some(Arc::new(chat_handled(MonitorMembership::mark_taken_in)));
        opened_after.rebuild_at(HANDLED_AT + 80);
        assert_eq!(
            fill(&opened_after, animated, HANDLED_AT + 80),
            None,
            "a View that opens after the turn shows the row handled, whenever the Chat arrives"
        );

        let mut switched_back = handled_while_listed(MonitorMembership::mark_taken_in);
        switched_back.settle_flashes(HANDLED_AT + 60);
        assert_eq!(fill(&switched_back, animated, HANDLED_AT + 60), None);
        assert_eq!(
            switched_back.next_frame_ms(HANDLED_AT + 60, WIDTH, animated),
            None
        );
        switched_back.rebuild_at(HANDLED_AT + 70);
        assert_eq!(
            fill(&switched_back, animated, HANDLED_AT + 70),
            None,
            "coming back to the Chat does not replay what happened while away"
        );
    }

    #[test]
    fn an_update_that_came_while_the_view_showed_another_chat_does_not_flash() {
        let animated = DisplayProfile::default();
        let (mut monitor, update) = updated_monitor(Some(chat()));
        monitor.rebuild_at(UPDATED_AT + 100);
        *update.lock().unwrap() = Some(UPDATED_AT + 2_000);
        monitor.settle_flashes(UPDATED_AT + 3_000);
        monitor.rebuild_at(UPDATED_AT + 3_000);
        assert_eq!(
            glyph(&monitor, animated, UPDATED_AT + 3_000),
            Some(Role::SuccessMark)
        );
        *update.lock().unwrap() = Some(UPDATED_AT + 4_000);
        monitor.rebuild_at(UPDATED_AT + 4_100);
        assert_eq!(
            glyph(&monitor, animated, UPDATED_AT + 4_100),
            Some(Role::RunningMark),
            "an update after coming back flashes"
        );
    }

    #[test]
    fn a_take_in_flashes_even_before_the_view_listed_the_update_unread_but_not_twice() {
        let animated = DisplayProfile::default();
        let (mut monitor, _) = updated_monitor(Some(chat()));
        let listed = monitor.rows.take();
        monitor.rows = Some(MonitorRowsRegistry::new(Vec::<Arc<dyn MonitorRows>>::new()).unwrap());
        monitor.rebuild_at(UPDATED_AT - 1_000);
        monitor.rows = listed;
        monitor.chat_status = Some(Arc::new(chat_handled(MonitorMembership::mark_taken_in)));
        monitor.rebuild_at(HANDLED_AT + 50);
        assert_eq!(
            fill(&monitor, animated, HANDLED_AT + 50),
            Some(Role::FlashStrong),
            "the turn took the update in while the View showed the Chat"
        );

        let mut read_again = chat_handled(MonitorMembership::mark_taken_in);
        read_again
            .monitor
            .mark_taken_in(quotes(), HANDLED_AT + 5_000);
        monitor.chat_status = Some(Arc::new(read_again));
        monitor.rebuild_at(HANDLED_AT + 5_050);
        assert_eq!(
            fill(&monitor, animated, HANDLED_AT + 5_050),
            None,
            "reading an update the row already showed handled is no hand-over"
        );
    }

    #[test]
    fn a_selected_unread_row_keeps_the_selection_and_its_accent_mark() {
        let (mut monitor, _) = updated_monitor(Some(chat()));
        monitor.rebuild_at(UPDATED_AT + 100);
        monitor.focus(false, None);
        let line = row(&monitor, DisplayProfile::default(), UPDATED_AT + 100);
        assert_eq!(line.fill, Some(Role::Selection));
        assert!(line.text().starts_with("  ◉ Quotes"), "{line:?}");
        let mark = line.spans.iter().find(|span| span.text == "●").unwrap();
        assert_eq!(mark.role, Some(Role::UnreadMark));
    }

    #[test]
    fn the_row_detail_tells_when_the_row_last_updated() {
        let (mut monitor, _) = updated_monitor(Some(chat()));
        monitor.rebuild_at(UPDATED_AT + 100);
        let facts = member_facts(&monitor.entries()[0], UPDATED_AT + 180_000);
        assert!(facts.contains(&"Updated   3m".to_owned()), "{facts:?}");
    }

    fn subscription(
        frequency: TickSubscriptionFrequency,
        next_check_at_ms: Option<u64>,
    ) -> TickSubscriptionStatus {
        TickSubscriptionStatus {
            id: uuid::Uuid::from_u128(frequency as u128 + 2),
            loop_id: uuid::Uuid::from_u128(9),
            loop_state: LoopState::WaitingTrigger,
            instance: quotes(),
            frequency,
            accumulated: 0,
            last_delivery_ms: None,
            last_delivery: None,
            kind: None,
            title: Some("Quotes".into()),
            updated_at_ms: None,
            link: TickLinkState::Live,
            next_check_at_ms,
            period_ms: frequency.period().map(|period| period.as_millis() as u64),
        }
    }

    /// A Monitor whose one row the given subscriptions watch.
    fn subscribed(subscriptions: Vec<TickSubscriptionStatus>, now_ms: u64) -> Monitor {
        let mut monitor = Monitor {
            subscriptions,
            ..Default::default()
        };
        monitor.rebuild_at(now_ms);
        monitor
    }

    fn countdown(monitor: &Monitor, now_ms: u64) -> Option<(usize, Role)> {
        monitor
            .frame(WIDTH, 10, DisplayProfile::default(), now_ms)
            .lines[2]
            .lead_fill
            .map(|lead| (lead.columns, lead.role))
    }

    #[test]
    fn a_periodic_row_counts_down_to_its_next_check_and_starts_over_after_it() {
        let period = 60_000;
        let first_check = 100_000;
        let mut monitor = subscribed(
            vec![subscription(
                TickSubscriptionFrequency::High,
                Some(first_check),
            )],
            first_check - period,
        );
        for (now, columns) in [
            (first_check - period, 0),
            (first_check - 45_000, 10),
            (first_check - 30_000, 20),
            (first_check, 40),
            (first_check + 5_000, 40),
        ] {
            assert_eq!(
                countdown(&monitor, now),
                Some((columns, Role::CheckProgress)),
                "at {now}"
            );
        }
        let text = monitor
            .frame(WIDTH, 10, DisplayProfile::default(), first_check - 30_000)
            .lines[2]
            .text();
        assert!(
            !text.chars().any(|character| character.is_ascii_digit()),
            "no time left is written: {text}"
        );
        assert_eq!(
            monitor.next_frame_ms(first_check - 30_000, WIDTH, DisplayProfile::default()),
            Some(first_check - 30_000 + 1_500),
            "the next frame is when one more column fills"
        );
        assert_eq!(
            monitor.next_frame_ms(first_check, WIDTH, DisplayProfile::default()),
            None,
            "a full countdown waits for the check"
        );

        monitor.subscriptions = vec![subscription(
            TickSubscriptionFrequency::High,
            Some(first_check + period),
        )];
        monitor.rebuild_at(first_check + 100);
        assert_eq!(
            countdown(&monitor, first_check + 100),
            Some((0, Role::CheckProgress)),
            "the check starts the countdown over"
        );

        monitor.focus(false, None);
        assert_eq!(
            countdown(&monitor, first_check + 30_000),
            Some((20, Role::SelectionProgress)),
            "a selected row counts down in the selection's deeper shade"
        );
    }

    #[test]
    fn several_subscriptions_count_down_to_the_nearest_check_and_show_the_most_frequent() {
        let now = 1_800_000;
        let monitor = subscribed(
            vec![
                subscription(TickSubscriptionFrequency::Low, Some(now + 30_000)),
                subscription(TickSubscriptionFrequency::Medium, Some(now + 300_000)),
            ],
            now,
        );
        assert_eq!(
            monitor.entries()[0].subscription,
            Some(SubscriptionMark {
                frequency: TickSubscriptionFrequency::Medium,
                next_check: Some(NextCheck {
                    at_ms: now + 30_000,
                    period_ms: 1_800_000,
                }),
            })
        );
        let line = &monitor
            .frame(WIDTH, 10, DisplayProfile::default(), now)
            .lines[2];
        assert!(line.text().trim_end().ends_with('⟳'), "{line:?}");
        assert!(
            !line.text().contains('2'),
            "no count of subscriptions: {line:?}"
        );
        assert_eq!(
            line.spans
                .last()
                .map(|span| (span.text.as_str(), span.role)),
            Some(("⟳", Some(Role::SubscriptionMedium)))
        );
        assert_eq!(
            countdown(&monitor, now),
            Some((39, Role::CheckProgress)),
            "the low subscription checks first, with 30 s of its 30 min left"
        );
    }

    #[test]
    fn an_immediate_subscription_shows_no_mark_and_no_countdown() {
        let monitor = subscribed(
            vec![subscription(TickSubscriptionFrequency::Immediate, None)],
            40_000,
        );
        assert_eq!(monitor.entries()[0].subscription, None);
        let line = &monitor
            .frame(WIDTH, 10, DisplayProfile::default(), 40_000)
            .lines[2];
        assert_eq!(line.lead_fill, None);
        assert!(!line.text().contains(['⟳', '↯']), "{line:?}");
        assert_eq!(
            monitor.next_frame_ms(40_000, WIDTH, DisplayProfile::default()),
            None
        );
    }

    #[test]
    fn a_row_flash_covers_the_countdown_until_it_ends() {
        let animated = DisplayProfile::default();
        let mut monitor = subscribed(
            vec![subscription(TickSubscriptionFrequency::High, Some(70_000))],
            HANDLED_AT - 1_000,
        );
        let mut status = subscription(TickSubscriptionFrequency::High, Some(70_000));
        status.updated_at_ms = Some(UPDATED_AT);
        monitor.subscriptions = vec![status];
        monitor.chat_status = Some(Arc::new(chat()));
        monitor.rebuild_at(UPDATED_AT + 100);
        monitor.chat_status = Some(Arc::new(chat_handled(MonitorMembership::mark_taken_in)));
        monitor.rebuild_at(HANDLED_AT + 50);
        let line = row(&monitor, animated, HANDLED_AT + 50);
        assert_eq!(line.fill, Some(Role::FlashStrong));
        assert_eq!(line.lead_fill, None);
        let after = row(&monitor, animated, HANDLED_AT + 1_200);
        assert_eq!(after.fill, None);
        assert_eq!(
            after.lead_fill.map(|lead| lead.role),
            Some(Role::CheckProgress),
            "the countdown comes back once the flash ends"
        );
    }
}

#[cfg(test)]
mod rebuild_benchmarks {
    use super::*;

    fn chat_with_messages(count: usize) -> tickloop_agent_protocol::ChatSnapshot {
        let messages: Vec<_> = (0..count)
            .map(|index| {
                serde_json::json!({
                    "role": if index % 2 == 0 { "user" } else { "assistant" },
                    "content": [{
                        "type": "text",
                        "text": format!("message {index}: {}", "conversation text ".repeat(4)),
                    }],
                })
            })
            .collect();
        serde_json::from_value(serde_json::json!({
            "id": uuid::Uuid::from_u128(1),
            "workdir": "/fixture",
            "messages": messages,
            "loops": [],
        }))
        .unwrap()
    }

    fn shells() -> ShellTaskSnapshot {
        ShellTaskSnapshot {
            chat_id: uuid::Uuid::from_u128(1),
            live_revision: 1,
            tasks: Vec::new(),
        }
    }

    /// Mean cost of one Monitor rebuild, and of the Monitor work one status refresh does, for a
    /// Chat of `messages` messages.
    #[test]
    #[ignore = "timing benchmark: cargo test --release --config profile.release.debug-assertions=true -p tickloop-agent-console --lib bench_ -- --ignored --nocapture"]
    fn bench_monitor_rebuild_cost_by_conversation_size() {
        const ROUNDS: u32 = 50;
        for messages in [1_000, 10_000, 50_000] {
            let chat = Arc::new(ChatStatus::of(&chat_with_messages(messages)));
            let mut monitor = Monitor::default();
            monitor.set_chat_status(chat.clone());
            let started = std::time::Instant::now();
            for _ in 0..ROUNDS {
                monitor.rebuild();
            }
            let rebuild = started.elapsed().as_secs_f64() * 1e6 / f64::from(ROUNDS);
            let started = std::time::Instant::now();
            for _ in 0..ROUNDS {
                monitor.refresh_sources(MonitorSources {
                    chat: chat.clone(),
                    shells: shells(),
                    subscriptions: Some(Vec::new()),
                });
            }
            let refresh = started.elapsed().as_secs_f64() * 1e6 / f64::from(ROUNDS);
            println!(
                "BENCH monitor messages={messages} rebuild_us={rebuild:.1} status_refresh_us={refresh:.1}"
            );
        }
    }
}

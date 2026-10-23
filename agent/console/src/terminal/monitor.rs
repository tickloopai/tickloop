use std::{
    collections::{BTreeSet, HashMap},
    sync::Arc,
};

use tickhost_connect::page::{MAX_PAGE_SIZE, PageCursor, PageError};

use super::{
    display::{DisplayProfile, Glyph},
    list_row::{self, ListRow, RowFlash},
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
const MAX_PINNED_TASKS: usize = 3;
const TASK_ROWS: usize = 2;
const GOAL_ROWS: usize = 2;
const MINIMUM_TICK_SECTION_ROWS: usize = 3;
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
    MonitorAction, MonitorContext, MonitorRowsRegistry, NextCheck, RowUpdate, StatusTone,
    SubscriptionMark, TickDescriptor, TickGlyph, TickOwner, TickResources, page_of,
};
use tick_protocol::InstanceRef;
use tickloop_agent_protocol::{
    ChatStatus, MonitorMembership, ShellTaskSnapshot, TickSubscriptionFrequency,
    TickSubscriptionStatus,
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

    /// Whether the row holds an update the Chat has not handled: a row with periodic
    /// subscriptions while they hold one for a check, any other row until the Chat handles its
    /// latest update.
    fn is_unread(&self) -> bool {
        self.subscription.is_some_and(|mark| mark.is_waiting)
            || self
                .update
                .as_ref()
                .is_some_and(|update| update.unread.is_some())
    }

    /// The check that hands over what the row's periodic subscriptions hold, once the Agent
    /// reports it; the update bar counts toward it.
    fn waiting_check(&self) -> Option<NextCheck> {
        self.subscription.filter(|mark| mark.is_waiting)?.next_check
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum MonitorTarget {
    Tick(InstanceRef),
    ScheduledTask(String),
    MoreScheduled,
    Goal,
}

#[derive(Default)]
pub(super) struct Monitor {
    auto_reveal_pending: bool,
    auto_reveal_handled: bool,
    subscriptions_initialized: bool,
    /// Initial snapshots seed the baseline independently because sources can arrive separately.
    /// Later rebuilds compare the full list, so adding a subscription to a member is not new.
    listed_instances: BTreeSet<InstanceRef>,
    pub(super) auto_shown: bool,
    pub(super) entry_flash_at_ms: Option<u64>,
    task_fires: HashMap<String, Option<u64>>,
    task_flashes: HashMap<String, u64>,
    pub(super) detail: Option<Box<dyn super::tick_actions::TickDetailView>>,
    pub(super) subscriptions: Vec<TickSubscriptionStatus>,
    pub(super) metadata: HashMap<String, Option<tickloop_agent_artifacts::ArtifactMetadata>>,
    pub(super) metadata_failures: HashMap<String, String>,
    pub(super) member_findings: crate::monitor_rows::MemberFindings,
    /// The visible page; the complete set is `total` rows.
    entries: Vec<MonitorEntry>,
    total: usize,
    group_totals: std::collections::BTreeMap<TickGroup, usize>,
    page_start: Option<PageCursor>,
    earlier_pages: Vec<Option<PageCursor>>,
    next_page: Option<PageCursor>,
    selected: Option<MonitorTarget>,
    tick_anchor: Option<InstanceRef>,
    expanded_tasks: bool,
    goal_continued_at_ms: Option<u64>,
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
    /// When time alone next changes the list: a finished Shell leaving.
    refresh_at: Option<u64>,
    /// Since when this View has shown this Chat's Monitor; an Agent turn taking an update in or
    /// using an instance earlier never flashes its row.
    flashes_since: Option<u64>,
    /// Delivery timestamps already seen, retained when active flashes are settled.
    seen_take_ins: HashMap<InstanceRef, u64>,
    /// When an Agent turn took in each row's update while this View showed the Chat; the row
    /// flashes warm from then on.
    taken_in: HashMap<InstanceRef, u64>,
    /// When the Agent last read or operated each row's instance with a tool while this View
    /// showed the Chat; the row flashes cold from then on, anew for each later use.
    used: HashMap<InstanceRef, u64>,
    /// The latest update of each row as this View last listed it, `None` for a row without one.
    /// A row's first listing is its baseline, so only an update that comes after it is new.
    listed_updates: HashMap<InstanceRef, Option<u64>>,
    /// Since when each row's glyph flashes for the new update this View last listed.
    update_flashes: HashMap<InstanceRef, u64>,
    /// The frequencies the user asked for that the Agent does not report yet, by subscription.
    /// Monitor shows them at once, and the next `f` cycles on from them rather than from a report
    /// that predates the request.
    requested_subscriptions: HashMap<uuid::Uuid, Option<TickSubscriptionFrequency>>,
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
    pub(super) targets: Vec<(usize, MonitorTarget)>,
}

impl Monitor {
    pub(super) fn take_auto_reveal(&mut self) -> bool {
        if !std::mem::take(&mut self.auto_reveal_pending) || self.auto_reveal_handled {
            return false;
        }
        self.auto_reveal_handled = true;
        true
    }

    pub(super) fn set_subscriptions(&mut self, subscriptions: Vec<TickSubscriptionStatus>) {
        if !self.subscriptions_initialized {
            self.listed_instances
                .extend(subscriptions.iter().map(|row| row.instance.clone()));
        }
        self.subscriptions_initialized = true;
        self.subscriptions = subscriptions;
    }

    pub(super) fn goal(&self) -> Option<super::goal_view::GoalView<'_>> {
        let mut view =
            super::goal_view::GoalView::from_snapshot(self.chat_status.as_ref()?.goal.as_ref()?);
        view.continued_at_ms = self.goal_continued_at_ms;
        if let Some(status) = &self.chat_status
            && matches!(
                status.goal.as_ref()?.status,
                tickloop_agent_protocol::GoalStatus::Active
            )
            && matches!(
                status.chat_loop.state,
                tickloop_agent_protocol::LoopState::Running
                    | tickloop_agent_protocol::LoopState::WaitingPermission
                    | tickloop_agent_protocol::LoopState::WaitingQuestion
                    | tickloop_agent_protocol::LoopState::WaitingPlan
            )
        {
            let since = status
                .goal
                .as_ref()?
                .updated_at_ms
                .max(status.chat_loop.run_started_at.unwrap_or(u64::MAX));
            view.elapsed_ms = view.elapsed_ms.saturating_add(
                tickloop_agent_base::private_fs::unix_time_milliseconds().saturating_sub(since),
            );
        }
        Some(view)
    }

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
    /// Ticks, scheduled tasks and an optional Goal, shown as `Monitor N`.
    pub(super) fn total(&self) -> usize {
        self.total + usize::from(self.goal().is_some())
    }
    pub(super) fn has_content(&self) -> bool {
        self.total() > 0
    }
    pub(super) fn selected(&self) -> Option<&MonitorEntry> {
        let MonitorTarget::Tick(instance) = self.selected.as_ref()? else {
            return None;
        };
        self.entries
            .iter()
            .find(|entry| &entry.instance == instance)
    }
    fn tasks(&self) -> &[tickloop_agent_protocol::ScheduledTask] {
        self.chat_status
            .as_ref()
            .map_or(&[], |chat| chat.scheduled_tasks.as_slice())
    }
    pub(super) fn selected_task(&self) -> Option<&tickloop_agent_protocol::ScheduledTask> {
        let MonitorTarget::ScheduledTask(id) = self.selected.as_ref()? else {
            return None;
        };
        self.tasks().iter().find(|task| &task.id == id)
    }
    pub(super) fn is_goal_selected(&self) -> bool {
        self.selected == Some(MonitorTarget::Goal) && self.goal().is_some()
    }
    pub(super) fn select_target(&mut self, target: &MonitorTarget) {
        if let Some(entry) = self.selected() {
            self.tick_anchor = Some(entry.instance.clone());
        }
        match target {
            MonitorTarget::Tick(instance) => self.select(instance),
            MonitorTarget::ScheduledTask(id) => {
                let limit = if self.expanded_tasks {
                    self.tasks().len()
                } else {
                    self.preview_count()
                };
                if self.tasks().iter().take(limit).any(|task| &task.id == id) {
                    self.selected = Some(target.clone());
                }
            }
            MonitorTarget::MoreScheduled => {
                if self.tasks().len() > MAX_PINNED_TASKS {
                    self.selected = Some(MonitorTarget::MoreScheduled);
                }
            }
            MonitorTarget::Goal => {
                if self.goal().is_some() {
                    self.selected = Some(MonitorTarget::Goal);
                }
            }
        }
    }
    fn preview_count(&self) -> usize {
        self.tasks().len().min(MAX_PINNED_TASKS)
    }
    pub(super) fn is_more_selected(&self) -> bool {
        self.selected == Some(MonitorTarget::MoreScheduled) && self.tasks().len() > MAX_PINNED_TASKS
    }
    pub(super) fn task_list_is_open(&self) -> bool {
        self.expanded_tasks
    }
    pub(super) fn open_task_list(&mut self) {
        if let Some(task) = self.tasks().get(MAX_PINNED_TASKS) {
            self.selected = Some(MonitorTarget::ScheduledTask(task.id.clone()));
            self.expanded_tasks = true;
        }
    }
    pub(super) fn close_task_list(&mut self) {
        self.expanded_tasks = false;
        if self.tasks().len() > MAX_PINNED_TASKS {
            self.selected = Some(MonitorTarget::MoreScheduled);
        }
        self.reconcile_selection();
    }
    fn waiting_task(&self, id: &str) -> bool {
        self.chat_status.as_ref().is_some_and(|chat| chat.pending_inputs.iter().any(|input| {
            matches!(&input.trigger_source, Some(tickloop_agent_protocol::TriggerSource::ScheduledTask { task_id, .. }) if task_id == id)
        }))
    }
    fn tasks_paused(&self) -> bool {
        self.chat_status
            .as_ref()
            .is_some_and(|chat| !chat.chat_loop.receiving.is_receiving())
    }
    pub(super) fn remove_task(&mut self, id: &str) {
        if let Some(chat) = &mut self.chat_status {
            Arc::make_mut(chat)
                .scheduled_tasks
                .retain(|task| task.id != id);
        }
        self.close_detail();
        self.rebuild();
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
        self.shown || self.goal().is_some()
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
        if self.shell_snapshot.is_none() {
            self.listed_instances
                .extend(shells.tasks.iter().map(|task| InstanceRef::shell(task.id)));
        }
        self.shell_starts = self
            .chat_status
            .as_deref()
            .map(|chat| super::task_access::shell_starts(chat, &shells))
            .unwrap_or_default();
        self.shell_snapshot = Some(shells);
        if let Some(subscriptions) = subscriptions {
            self.set_subscriptions(subscriptions);
        }
        self.rebuild();
    }
    /// An artifact title that changed in the Chat since the previous status, such as after the
    /// Agent's own rename, is newer than the one the last metadata read cached, so it replaces
    /// that one.
    fn adopt_chat_status(&mut self, status: Arc<ChatStatus>) {
        let previous = self.chat_status.take();
        if let Some(previous) = &previous {
            self.auto_reveal_pending |= status.goal.as_ref().is_some_and(|goal| {
                previous
                    .goal
                    .as_ref()
                    .is_none_or(|known| known.id != goal.id)
            }) || status.scheduled_tasks.iter().any(|task| {
                !previous
                    .scheduled_tasks
                    .iter()
                    .any(|known| known.id == task.id)
            });
            if previous.goal.as_ref().map(|goal| goal.id)
                != status.goal.as_ref().map(|goal| goal.id)
            {
                self.goal_continued_at_ms = None;
            }
            let continued = status
                .goal
                .as_ref()
                .and_then(|goal| goal.last_continued_at_ms);
            if continued.is_some()
                && continued
                    != previous
                        .goal
                        .as_ref()
                        .and_then(|goal| goal.last_continued_at_ms)
            {
                self.goal_continued_at_ms =
                    Some(tickloop_agent_base::private_fs::unix_time_milliseconds());
            }
        }
        if previous.is_none() {
            self.listed_instances.extend(
                status
                    .monitor
                    .members
                    .iter()
                    .map(|member| member.instance.clone()),
            );
            self.listed_instances.extend(
                status
                    .artifact_ticks
                    .iter()
                    .filter_map(|tick| tick.instance_id().parse().ok()),
            );
        }
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
    /// The Chat's subscriptions as Monitor shows them: at the frequency the user last asked for
    /// until the Agent reports it.
    pub(super) fn shown_subscriptions(&self) -> Vec<TickSubscriptionStatus> {
        self.subscriptions
            .iter()
            .cloned()
            .filter_map(|mut row| {
                if let Some(frequency) = self.requested_subscriptions.get(&row.id) {
                    row.frequency = (*frequency)?;
                }
                Some(row)
            })
            .collect()
    }
    /// The subscription on `instance` whose frequency the row's `f` cycles: the last periodic
    /// one, since the Agent lists subscriptions in the order their Loops began.
    pub(super) fn subscription_target(&self, instance: &InstanceRef) -> Option<uuid::Uuid> {
        crate::monitor_rows::subscriptions_on(&self.shown_subscriptions(), instance)
            .iter()
            .filter(|row| row.frequency.period().is_some())
            .min_by_key(|row| row.frequency.period())
            .map(|row| row.id)
    }
    pub(super) fn request_next_subscription(
        &mut self,
        subscription: uuid::Uuid,
        all_on_instance: bool,
    ) -> Option<tickloop_agent_protocol::TickSubscriptionChange> {
        let rows = self.shown_subscriptions();
        let row = rows.iter().find(|row| row.id == subscription)?;
        let next = match row.frequency {
            TickSubscriptionFrequency::Low => None,
            frequency => Some(frequency.next_periodic()?),
        };
        for target in rows.iter().filter(|target| {
            target.id == subscription
                || (all_on_instance
                    && target.instance == row.instance
                    && target.frequency.period().is_some())
        }) {
            self.requested_subscriptions.insert(target.id, next);
        }
        self.rebuild();
        Some(next.map_or(
            tickloop_agent_protocol::TickSubscriptionChange::Cancel,
            tickloop_agent_protocol::TickSubscriptionChange::Frequency,
        ))
    }

    /// Shows what the Agent reports for `subscription` again, after it refused the change.
    pub(super) fn forget_requested_subscription(&mut self, subscription: uuid::Uuid) {
        if self.requested_subscriptions.remove(&subscription).is_some() {
            self.rebuild();
        }
    }
    fn rebuild_at(&mut self, now_ms: u64) {
        let subscriptions = &self.subscriptions;
        self.requested_subscriptions.retain(|id, requested| {
            subscriptions.iter().any(|row| {
                row.id == *id && requested.is_none_or(|frequency| row.frequency != frequency)
            })
        });
        let shown_subscriptions = self.shown_subscriptions();
        let mut member_updates = self.member_findings.updates.clone();
        for row in &self.subscriptions {
            if (row.has_pending_change || row.accumulated > 0)
                && let Some(updated) = row.updated_at_ms
            {
                let mark = member_updates.entry(row.instance.clone()).or_default();
                *mark = (*mark).max(updated);
            }
        }
        let context = MonitorContext {
            terminal_members: self.member_findings.terminals.clone(),
            chat: self.chat_status.clone(),
            shells: self.shell_snapshot.clone(),
            subscriptions: shown_subscriptions.clone(),
            metadata: self.metadata.clone(),
            metadata_failures: self.metadata_failures.clone(),
            member_states: self.member_findings.states.clone(),
            member_resources: self.member_findings.resources.clone(),
            member_updates,
            shell_starts: self.shell_starts.clone(),
            retained: self.detail.as_ref().and_then(|detail| detail.instance()),
            now_ms,
        };
        let listed = self
            .rows
            .get_or_insert_with(MonitorRowsRegistry::builtin)
            .list(&context);
        self.auto_reveal_pending |= listed
            .iter()
            .any(|entry| !self.listed_instances.contains(&entry.instance));
        self.listed_instances = listed.iter().map(|entry| entry.instance.clone()).collect();
        self.note_flashes(&listed, now_ms);
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
        let tasks = self.tasks().to_vec();
        for task in &tasks {
            let previous = self
                .task_fires
                .insert(task.id.clone(), task.last_fired_at_ms);
            if previous.is_some_and(|previous| task.last_fired_at_ms > previous) {
                self.task_flashes.insert(task.id.clone(), now_ms);
            }
        }
        self.task_fires
            .retain(|id, _| tasks.iter().any(|task| &task.id == id));
        self.task_flashes
            .retain(|id, _| tasks.iter().any(|task| &task.id == id));
        self.total = page.total + tasks.len();
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
        if let Some(detail) = self
            .detail
            .as_mut()
            .and_then(|detail| detail.as_any_mut().downcast_mut::<SubscriptionDetail>())
        {
            let instance = detail.instance().clone();
            detail.update(
                title_of(&self.entries, &instance),
                crate::monitor_rows::subscriptions_on(&shown_subscriptions, &instance),
            );
            if detail.is_done() {
                self.close_detail();
            }
        }
        if let Some(detail) = self.detail.as_mut().and_then(|detail| {
            detail
                .as_any_mut()
                .downcast_mut::<super::scheduled_tasks::TaskDetail>()
        }) {
            if let Some(task) = tasks.iter().find(|task| task.id == detail.task.id) {
                detail.task = task.clone();
            } else {
                self.close_detail();
            }
        }
        self.reconcile_selection();
    }
    /// Notes what flashes: the glyph of a row whose new update just came, the whole row whose
    /// update an Agent turn has just taken in, and the whole row whose instance the Agent has just
    /// used. A row the user opened does not flash, nor one taken in or used before this View
    /// showed the Chat, nor the same recorded delivery listed again. Each new delivery can flash even when a read already handled its content.
    fn note_flashes(&mut self, listed: &[TickDescriptor], now_ms: u64) {
        let flashes_since = *self.flashes_since.get_or_insert(now_ms);
        let membership = self.chat_status.as_deref().map(|chat| &chat.monitor);
        let mut listed_updates = HashMap::new();
        for entry in listed {
            if let Some(used) = membership
                .and_then(|membership| membership.used_at(&entry.instance))
                .filter(|used| *used >= flashes_since)
            {
                self.used
                    .entry(entry.instance.clone())
                    .and_modify(|since| *since = (*since).max(used))
                    .or_insert(used);
            }
            if let Some(taken_in) = membership
                .and_then(|membership| membership.taken_in_at(&entry.instance))
                .or_else(|| {
                    entry
                        .update
                        .as_ref()
                        .and_then(|update| update.taken_in_at_ms)
                })
            {
                let previous = self.seen_take_ins.insert(entry.instance.clone(), taken_in);
                if previous != Some(taken_in) && taken_in >= flashes_since {
                    self.taken_in.insert(entry.instance.clone(), taken_in);
                }
            }
            let at_ms = entry.update.as_ref().map(|update| update.at_ms);
            if self
                .listed_updates
                .get(&entry.instance)
                .is_some_and(|listed| at_ms > *listed)
            {
                self.update_flashes.insert(entry.instance.clone(), now_ms);
            }
            listed_updates.insert(entry.instance.clone(), at_ms);
        }
        self.taken_in
            .retain(|instance, _| listed_updates.contains_key(instance));
        self.used
            .retain(|instance, _| listed_updates.contains_key(instance));
        self.update_flashes
            .retain(|instance, _| listed_updates.contains_key(instance));
        self.seen_take_ins
            .retain(|instance, _| listed_updates.contains_key(instance));
        self.listed_updates = listed_updates;
    }
    /// Ends the flashes under way and lets only later updates, take-ins and uses flash, as when
    /// this Chat's Monitor comes back into view: what came, or what an Agent turn took in or used,
    /// while the View showed another Chat does not replay.
    pub(super) fn settle_flashes(&mut self, now_ms: u64) {
        self.taken_in.clear();
        self.used.clear();
        self.update_flashes.clear();
        self.listed_updates.clear();
        self.flashes_since = Some(now_ms);
        self.goal_continued_at_ms = None;
    }
    /// When time alone next changes the list: the next finished Shell to leave.
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
            .filter(|until| *until > now_ms)
            .min()
    }
    /// When time alone next changes the list drawn `width` columns wide without changing its
    /// rows: a running row's glyph blinking or its elapsed time ticking, a row or an unread row's
    /// glyph flashing, or an update bar filling one more column. `None` while nothing runs,
    /// flashes or fills, or a detail covers the list.
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
                    self.used.get(&entry.instance),
                    self.update_flashes
                        .get(&entry.instance)
                        .filter(|_| entry.is_unread()),
                ]
            })
            .flatten()
            .filter_map(|since| display.next_flash_change_ms(*since, now_ms));
        let counting_down = self.entries.iter().filter_map(|entry| {
            entry.waiting_check()?.next_column_at(
                width.saturating_sub(list_row::name_column(display.glyph(entry.glyph))),
                now_ms,
            )
        });
        let task_animation = self
            .chat_status
            .as_ref()
            .filter(|chat| !chat.scheduled_tasks.is_empty())
            .and_then(|_| {
                let flashing = (!self.tasks_paused())
                    .then(|| {
                        self.task_flashes
                            .values()
                            .filter_map(|since| display.next_flash_change_ms(*since, now_ms))
                            .min()
                    })
                    .flatten();
                let countdown = self
                    .tasks()
                    .iter()
                    .filter_map(|task| {
                        let next = task.next_fire_at_ms?;
                        let wait_ms =
                            next.checked_sub(task.last_fired_at_ms.unwrap_or(task.created_at_ms))?;
                        if wait_ms == 0 || self.waiting_task(&task.id) {
                            return None;
                        }
                        NextCheck {
                            at_ms: next,
                            wait_ms: wait_ms.min(super::scheduled_task_rows::PROGRESS_WINDOW_MS),
                        }
                        .next_column_at(
                            width.saturating_sub(list_row::compact_name_column(
                                display.glyph(Glyph::AgentWaiting),
                            )),
                            now_ms,
                        )
                    })
                    .min();
                flashing.into_iter().chain(countdown).min()
            });
        running
            .into_iter()
            .chain(task_animation)
            .chain(flashing)
            .chain(counting_down)
            .chain(
                self.goal_continued_at_ms
                    .and_then(|since| display.next_flash_change_ms(since, now_ms)),
            )
            .min()
    }
    /// What `entry`'s whole row flashes for, and since when: the later of an Agent turn taking its
    /// update in and the Agent using its instance, so a flash that starts covers one under way.
    pub(super) fn flash_use(&mut self, instance: InstanceRef, since_ms: u64) {
        self.used.insert(instance, since_ms);
    }

    fn row_flash(&self, entry: &MonitorEntry) -> Option<(RowFlash, u64)> {
        let taken_in = self
            .taken_in
            .get(&entry.instance)
            .map(|since| (RowFlash::TakeIn, *since));
        let used = self
            .used
            .get(&entry.instance)
            .map(|since| (RowFlash::Use, *since));
        taken_in
            .into_iter()
            .chain(used)
            .max_by_key(|(_, since)| *since)
    }
    /// The colour of `entry`'s glyph: the update colour, flashing for each new update, while the
    /// row holds an update the Chat has not handled, and its state colour otherwise.
    fn glyph_role(&self, entry: &MonitorEntry, display: DisplayProfile, now_ms: u64) -> Role {
        if entry.is_unread() {
            let flash_since = self.update_flashes.get(&entry.instance).copied();
            list_row::update_mark(flash_since, display, now_ms)
        } else {
            list_row::state_mark(entry.tone, display, now_ms)
        }
    }
    /// The row's title, or the instance itself when no row shows it.
    pub(super) fn title_of(&self, instance: &InstanceRef) -> String {
        title_of(&self.entries, instance)
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
        self.total = self.entries.len() + self.tasks().len();
        self.shown |= self.total > 0;
        self.group_totals.clear();
        for entry in &self.entries {
            *self.group_totals.entry(entry.group).or_default() += 1;
        }
        self.reconcile_selection();
    }
    fn reconcile_selection(&mut self) {
        if self.expanded_tasks && self.tasks().is_empty() {
            self.expanded_tasks = false;
        }
        if self.entries.is_empty() && self.tasks().is_empty() {
            self.detail = None;
        }
        if self.entries.is_empty() && self.tasks().is_empty() && self.goal().is_none() {
            self.focused = false;
            self.overlay = false;
        }
        if self.selected().is_none()
            && self.selected_task().is_none()
            && !self.is_more_selected()
            && !self.is_goal_selected()
        {
            self.selected = if self.expanded_tasks {
                None
            } else {
                self.entries
                    .first()
                    .map(|entry| MonitorTarget::Tick(entry.instance.clone()))
            };
            if self.selected.is_none() {
                self.selected = self
                    .tasks()
                    .first()
                    .map(|task| MonitorTarget::ScheduledTask(task.id.clone()));
            }
            if self.selected.is_none() && self.goal().is_some() {
                self.selected = Some(MonitorTarget::Goal);
            }
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
            self.selected = Some(MonitorTarget::Tick(instance.clone()));
            self.tick_anchor = Some(instance.clone());
        }
    }
    pub(super) fn focus(&mut self, overlay: bool, group: Option<TickGroup>) {
        self.focused = true;
        self.overlay = overlay;
        if let Some(group) = group {
            self.show_group(group);
        }
        self.reconcile_selection();
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
            self.selected = Some(MonitorTarget::Tick(entry.instance.clone()));
        }
    }
    /// Moves within the page; past its last or first row the next or earlier page opens.
    pub(super) fn move_selection(&mut self, delta: isize) {
        if self.expanded_tasks {
            if self.tasks().is_empty() {
                self.close_task_list();
                return;
            }
            let index = self
                .selected_task()
                .and_then(|selected| self.tasks().iter().position(|task| task.id == selected.id))
                .unwrap_or(0);
            let target = (index as isize)
                .saturating_add(delta)
                .clamp(0, self.tasks().len() as isize - 1) as usize;
            self.selected = Some(MonitorTarget::ScheduledTask(
                self.tasks()[target].id.clone(),
            ));
            return;
        }
        let from_tick = matches!(self.selected, Some(MonitorTarget::Tick(_)));
        if let Some(entry) = self.selected() {
            self.tick_anchor = Some(entry.instance.clone());
        }
        let mut targets = self
            .entries
            .iter()
            .map(|entry| MonitorTarget::Tick(entry.instance.clone()))
            .collect::<Vec<_>>();
        targets.extend(
            self.tasks()
                .iter()
                .take(self.preview_count())
                .map(|task| MonitorTarget::ScheduledTask(task.id.clone())),
        );
        if self.tasks().len() > MAX_PINNED_TASKS {
            targets.push(MonitorTarget::MoreScheduled);
        }
        if self.goal().is_some() {
            targets.push(MonitorTarget::Goal);
        }
        let index = self
            .selected
            .as_ref()
            .and_then(|selected| targets.iter().position(|target| target == selected))
            .unwrap_or(0);
        let target = (index as isize).saturating_add(delta);
        if from_tick
            && target >= self.entries.len() as isize
            && let Some(next) = self.next_page.take()
        {
            self.earlier_pages.push(self.page_start.replace(next));
            self.rebuild();
            self.selected = self
                .entries
                .first()
                .map(|entry| MonitorTarget::Tick(entry.instance.clone()));
            return;
        }
        if target < 0
            && let Some(earlier) = self.earlier_pages.pop()
        {
            self.page_start = earlier;
            self.rebuild();
            self.selected = self
                .entries
                .last()
                .map(|entry| MonitorTarget::Tick(entry.instance.clone()));
            return;
        }
        self.selected = targets
            .get(target.clamp(0, targets.len().saturating_sub(1) as isize) as usize)
            .cloned();
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

    fn selected_status(&self) -> String {
        let Some(entry) = self.selected() else {
            return String::new();
        };
        let mut parts = vec![match entry.tone {
            StatusTone::Running { .. } => "Running".to_owned(),
            StatusTone::Live => "Live".to_owned(),
            StatusTone::Degraded => "Degraded".to_owned(),
            StatusTone::Gone => "Gone".to_owned(),
            StatusTone::Succeeded => "Completed".to_owned(),
            StatusTone::Failed => "Failed".to_owned(),
            StatusTone::Stopped => "Stopped".to_owned(),
            StatusTone::Neutral => entry.status.clone(),
        }];
        if let Some(mark) = entry.subscription {
            let frequency = match mark.frequency {
                TickSubscriptionFrequency::High => "Every 1 min",
                TickSubscriptionFrequency::Medium => "Every 10 min",
                TickSubscriptionFrequency::Low => "Every 30 min",
                TickSubscriptionFrequency::Immediate => "Immediate",
            };
            parts.push(frequency.to_owned());
        }
        if entry.is_unread() {
            parts.push("Unread".to_owned());
        }
        parts.retain(|part| !part.is_empty());
        parts.join(" · ")
    }

    /// The keys of the focused list: the selected row's actions and Esc.
    pub(super) fn hint(&self) -> String {
        if self.is_more_selected() {
            return "enter all scheduled · j/k select · esc input".into();
        }
        if self.is_goal_selected() {
            return "enter Goal details · j/k select · esc return to input".into();
        }
        if self.selected_task().is_some() {
            return if self.expanded_tasks {
                "enter details · Ctrl+X remove · j/k select · esc back"
            } else {
                "enter details · Ctrl+X remove · esc input"
            }
            .into();
        }
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
                            } else if action.key == crate::monitor_rows::REMOVE_KEY {
                                "Ctrl+X".into()
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

    fn entry_lines(
        &self,
        entry: &MonitorEntry,
        width: usize,
        display: DisplayProfile,
        now_ms: u64,
    ) -> Vec<RoleLine> {
        let title = super::safe_line(&entry.title);
        ListRow {
            selected: self.focused && matches!(&self.selected, Some(MonitorTarget::Tick(instance)) if instance == &entry.instance),
            glyph: display.glyph(entry.glyph),
            glyph_role: self.glyph_role(entry, display, now_ms),
            name: &title,
            details: list_row::row_details(entry.tone, &entry.notes, now_ms),
            fill: list_row::flash_fill(self.row_flash(entry), display, now_ms),
            subscription: list_row::subscription_mark(entry.subscription, display),
            bar_columns: entry.waiting_check().map(|check| {
                check.elapsed_columns(
                    width.saturating_sub(list_row::name_column(display.glyph(entry.glyph))),
                    now_ms,
                )
            }),
        }.lines(width)
    }

    /// Bottom padding yields before section gaps, pinned rows or the selected Tick's full row.
    pub(super) fn side_content_height(
        &self,
        width: usize,
        height: usize,
        display: DisplayProfile,
        now_ms: u64,
    ) -> usize {
        let minimum = if self.detail.is_some() {
            1
        } else if self.expanded_tasks {
            TASK_ROWS + 2
        } else {
            let has_ticks = !self.entries.is_empty();
            let has_tasks = !self.tasks().is_empty();
            let has_goal = self.goal().is_some();
            let tick_rows = self
                .selected()
                .or_else(|| {
                    self.tick_anchor.as_ref().and_then(|instance| {
                        self.entries
                            .iter()
                            .find(|entry| &entry.instance == instance)
                    })
                })
                .or_else(|| self.entries.first())
                .map_or(0, |entry| {
                    1 + self.entry_lines(entry, width, display, now_ms).len()
                });
            let footer_rows = usize::from(!self.selected_status().is_empty());
            let tick_rows = if has_ticks {
                (tick_rows + footer_rows).max(MINIMUM_TICK_SECTION_ROWS)
            } else {
                footer_rows
            };
            1 + tick_rows
                + usize::from(has_tasks)
                + self.preview_count() * TASK_ROWS
                + usize::from(self.tasks().len() > MAX_PINNED_TASKS)
                + usize::from(has_goal) * GOAL_ROWS
                + usize::from(has_ticks && (has_tasks || has_goal))
                + usize::from(has_tasks && has_goal)
                + usize::from(!has_ticks && !has_tasks && !has_goal)
        };
        height.saturating_sub(usize::from(height > minimum))
    }

    fn task_lines(
        &self,
        task: &tickloop_agent_protocol::ScheduledTask,
        width: usize,
        display: DisplayProfile,
        now_ms: u64,
    ) -> Vec<RoleLine> {
        let selected = self.focused
            && matches!(&self.selected, Some(MonitorTarget::ScheduledTask(id)) if id == &task.id);
        let view = super::scheduled_tasks::view(
            task,
            self.tasks_paused(),
            self.task_flashes.get(&task.id).copied(),
            self.waiting_task(&task.id),
        );
        view.row_lines(
            width,
            selected,
            super::scheduled_task_rows::ScheduledTaskRender {
                display,
                ascii: false,
                now: super::scheduled_tasks::local_time(now_ms, &task.time_zone)
                    .unwrap_or(time::OffsetDateTime::UNIX_EPOCH),
                now_ms,
            },
        )
    }

    fn task_list_frame(
        &self,
        width: usize,
        height: usize,
        display: DisplayProfile,
        now_ms: u64,
    ) -> MonitorFrame {
        let mut lines = vec![RoleLine::role(
            format!("Scheduled ({})", self.tasks().len()),
            Role::Title,
        )];
        let mut targets = Vec::new();
        let capacity = height.saturating_sub(2);
        let visible_tasks = (capacity / TASK_ROWS).max(1);
        let selected = self
            .selected_task()
            .and_then(|selected| self.tasks().iter().position(|task| task.id == selected.id))
            .unwrap_or(0);
        let offset = selected.saturating_sub(visible_tasks.saturating_sub(1));
        for task in self.tasks().iter().skip(offset).take(visible_tasks) {
            for line in self.task_lines(task, width, display, now_ms) {
                if lines.len() > capacity {
                    break;
                }
                targets.push((lines.len(), MonitorTarget::ScheduledTask(task.id.clone())));
                lines.push(line);
            }
        }
        if height > 1 {
            lines.resize(height - 1, RoleLine::default());
            lines.push(RoleLine::role(self.selected_status(), Role::MutedText));
        }
        lines.truncate(height);
        for line in &mut lines {
            if unicode_width::UnicodeWidthStr::width(line.text().as_str()) > width {
                line.truncate(width);
            }
        }
        MonitorFrame { lines, targets }
    }

    pub(super) fn frame(
        &self,
        width: usize,
        height: usize,
        display: DisplayProfile,
        now_ms: u64,
    ) -> MonitorFrame {
        if self.expanded_tasks {
            return self.task_list_frame(width, height, display, now_ms);
        }
        let ticks_total = self.total.saturating_sub(self.tasks().len());
        let heading = if ticks_total > self.entries.len() {
            let first = self.earlier_pages.len() * MAX_PAGE_SIZE + 1;
            format!(
                "Monitor {first}–{} of {ticks_total}",
                first + self.entries.len().saturating_sub(1)
            )
        } else {
            "Monitor".into()
        };
        let mut goal_lines = self
            .goal()
            .map(|goal| {
                goal.monitor_lines(
                    width,
                    self.focused && self.is_goal_selected(),
                    display,
                    false,
                    now_ms,
                )
            })
            .unwrap_or_default();
        goal_lines.truncate(height.min(GOAL_ROWS));
        let has_ticks = !self.entries.is_empty();
        let has_tasks = !self.tasks().is_empty();
        let has_goal = !goal_lines.is_empty();
        let gap_count =
            usize::from(has_ticks && (has_tasks || has_goal)) + usize::from(has_tasks && has_goal);
        let full_dock_rows = goal_lines.len()
            + self.preview_count() * TASK_ROWS
            + usize::from(has_tasks)
            + usize::from(self.tasks().len() > MAX_PINNED_TASKS);
        let spacing_budget = height
            .saturating_sub(
                full_dock_rows
                    + 1
                    + if has_ticks {
                        MINIMUM_TICK_SECTION_ROWS
                    } else {
                        0
                    },
            )
            .min(gap_count);
        let goal_gap = has_tasks && has_goal && spacing_budget > 0;
        let tick_gap =
            has_ticks && (has_tasks || has_goal) && spacing_budget > usize::from(goal_gap);
        let content_height = height - usize::from(goal_gap) - usize::from(tick_gap);
        let minimum_tasks = if self.tasks().is_empty() {
            0
        } else {
            TASK_ROWS.min(content_height.saturating_sub(goal_lines.len()))
        };
        let mut extra = content_height.saturating_sub(goal_lines.len() + minimum_tasks);
        let scheduled_heading = !self.tasks().is_empty() && extra > 0;
        extra = extra.saturating_sub(usize::from(scheduled_heading));
        let more = self.tasks().len() > MAX_PINNED_TASKS && extra > 0;
        extra = extra.saturating_sub(usize::from(more));
        let show_heading = extra > 0;
        extra = extra.saturating_sub(usize::from(show_heading));
        let show_footer = extra > 0 && !self.selected_status().is_empty();
        let mut lines = if show_heading {
            vec![RoleLine::role(
                heading,
                if self.focused {
                    Role::Title
                } else {
                    Role::Text
                },
            )]
        } else {
            Vec::new()
        };
        let fixed_budget = content_height
            .saturating_sub(lines.len() + goal_lines.len() + usize::from(show_footer));
        let task_budget =
            fixed_budget.saturating_sub(usize::from(scheduled_heading) + usize::from(more));
        let visible_tasks = if minimum_tasks > 0 {
            (task_budget / TASK_ROWS).max(1).min(self.preview_count())
        } else {
            0
        };
        let scheduled_rows =
            (usize::from(scheduled_heading) + visible_tasks * TASK_ROWS + usize::from(more))
                .min(fixed_budget);
        let tick_capacity = fixed_budget.saturating_sub(scheduled_rows);
        let mut rows = Vec::new();
        let mut row_ids = Vec::new();
        let mut selected_row = 0;
        let scroll_target = match self.selected.as_ref() {
            Some(MonitorTarget::Tick(instance)) => Some(instance),
            _ => self.tick_anchor.as_ref(),
        };
        let groups = self
            .entries
            .iter()
            .map(|entry| entry.group)
            .collect::<std::collections::BTreeSet<_>>();
        for group in groups {
            let entries: Vec<_> = self
                .entries
                .iter()
                .filter(|entry| entry.group == group)
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
            for entry in entries {
                if scroll_target == Some(&entry.instance) {
                    selected_row = rows.len();
                }
                for line in self.entry_lines(entry, width, display, now_ms) {
                    rows.push(line);
                    row_ids.push(Some(MonitorTarget::Tick(entry.instance.clone())));
                }
            }
        }

        if self.entries.is_empty() && self.tasks().is_empty() && self.goal().is_none() {
            rows.push(RoleLine::role(EMPTY_MONITOR_TEXT, Role::MutedText));
            row_ids.push(None);
        }
        let offset = selected_row.saturating_sub(tick_capacity.saturating_sub(2));
        let mut targets = row_ids
            .into_iter()
            .skip(offset)
            .take(tick_capacity)
            .enumerate()
            .filter_map(|(row, id)| id.map(|id| (row + lines.len(), id)))
            .collect::<Vec<_>>();
        lines.extend(rows.into_iter().skip(offset).take(tick_capacity));
        lines.resize(
            usize::from(show_heading) + tick_capacity,
            RoleLine::default(),
        );
        if show_footer {
            lines.push(RoleLine::role(self.selected_status(), Role::MutedText));
        }
        if tick_gap {
            lines.push(RoleLine::default());
        }
        let scheduled_end = lines.len() + scheduled_rows;
        if scheduled_heading {
            lines.push(RoleLine::role(
                format!("Scheduled ({})", self.tasks().len()),
                Role::MutedText,
            ));
        }
        let selected_task = self.selected_task().and_then(|selected| {
            self.tasks()
                .iter()
                .take(self.preview_count())
                .position(|task| task.id == selected.id)
        });
        let start = selected_task
            .unwrap_or_else(|| {
                if self.is_more_selected() || self.is_goal_selected() {
                    self.preview_count().saturating_sub(1)
                } else {
                    0
                }
            })
            .saturating_sub(visible_tasks.saturating_sub(1))
            .min(self.preview_count().saturating_sub(visible_tasks));
        for task in self.tasks().iter().skip(start).take(visible_tasks) {
            for line in self.task_lines(task, width, display, now_ms) {
                if lines.len() >= scheduled_end.saturating_sub(usize::from(more)) {
                    break;
                }
                targets.push((lines.len(), MonitorTarget::ScheduledTask(task.id.clone())));
                lines.push(line);
            }
        }
        if more && lines.len() < scheduled_end {
            let mut line = RoleLine::role(
                format!("+{} more", self.tasks().len() - self.preview_count()),
                Role::MutedText,
            );
            if self.focused && self.is_more_selected() {
                line = line.filled(Role::Selection);
            }
            targets.push((lines.len(), MonitorTarget::MoreScheduled));
            lines.push(line);
        }
        lines.truncate(scheduled_end);
        if goal_gap {
            lines.push(RoleLine::default());
        }
        lines.extend(goal_lines);
        lines.truncate(height);
        for line in &mut lines {
            if unicode_width::UnicodeWidthStr::width(line.text().as_str()) > width {
                line.truncate(width);
            }
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
    if let Some(count) = entry
        .update
        .as_ref()
        .and_then(|update| update.unread)
        .and_then(|unread| unread.count)
    {
        facts.push(format!("Waiting   {count} updates"));
    }
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
            text.lines().last() == Some("waiting"),
            "selected state belongs in the footer: {text}"
        );
        assert!(!text.contains("Subscribed"));
        assert!(!text.contains("enter"), "keys are on the hint row: {text}");
        assert_eq!(list.hint(), "enter inspect · z expand · esc input");
    }
}

#[cfg(test)]
mod signal_tests {
    use super::*;
    use crate::monitor_rows::{MonitorRows, RowUpdate};
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
                "folder": "/fixture",
                "messages": [],
                "chat_loop": {"id":"00000000-0000-0000-0000-000000000000","state":"Idle"}
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

    /// The text and role of the row's first column.
    fn lead(monitor: &Monitor, display: DisplayProfile, now_ms: u64) -> (String, Option<Role>) {
        let line = row(monitor, display, now_ms);
        let first = &line.spans[0];
        (first.text.chars().take(1).collect(), first.role)
    }

    fn reduced_motion() -> DisplayProfile {
        DisplayProfile::new(DisplayPreferences {
            prefers_reduced_motion: true,
        })
    }

    #[test]
    fn an_unread_update_colours_the_glyph_without_a_fill_a_time_or_a_dot() {
        let (mut monitor, _) = updated_monitor(Some(chat()));
        monitor.rebuild_at(UPDATED_AT + 100);
        let animated = DisplayProfile::default();
        for now in [UPDATED_AT + 100, UPDATED_AT + 500, UPDATED_AT + 60_000] {
            let line = row(&monitor, animated, now);
            assert_eq!(line.fill, None, "no unread fill: {line:?}");
            assert_eq!(
                line.band, None,
                "no subscription holds the update, so no bar"
            );
            assert!(!line.text().contains('●'), "{line:?}");
            assert_eq!(glyph(&monitor, animated, now), Some(Role::UpdateMark));
            assert_eq!(
                lead(&monitor, animated, now),
                (" ".to_owned(), Some(Role::MutedText)),
                "the first column stays bare"
            );
        }
        assert_eq!(
            monitor.next_frame_ms(UPDATED_AT + 100, WIDTH, animated),
            None,
            "an update the View found on its first listing does not flash"
        );
    }

    #[test]
    fn each_new_update_flashes_the_glyph_in_the_update_colour() {
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
                    Some(Role::UpdateMark),
                    Some(Role::UpdateMarkDim),
                    Some(Role::UpdateMark),
                    Some(Role::UpdateMark)
                ],
                "the update at {next} flashes the glyph, which then stays in the update colour"
            );
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
            Some(Role::UpdateMark),
            "under Reduce motion the glyph never flashes"
        );
        assert_eq!(
            still.next_frame_ms(UPDATED_AT + 2_200, WIDTH, reduced_motion()),
            None
        );
    }

    #[test]
    fn a_turn_taking_the_update_in_flashes_the_row_and_restores_the_state_colour() {
        let monitor = handled_while_listed(MonitorMembership::mark_taken_in);
        let animated = DisplayProfile::default();
        let fills = [0, 399, 400, 799, 800, 1_199, 1_200, 5_000]
            .map(|elapsed| fill(&monitor, animated, HANDLED_AT + elapsed));
        assert_eq!(
            fills,
            [
                Some(Role::TakeInFlashStrong),
                Some(Role::TakeInFlashStrong),
                Some(Role::TakeInFlashSoft),
                Some(Role::TakeInFlashSoft),
                Some(Role::TakeInFlashStrong),
                Some(Role::TakeInFlashStrong),
                None,
                None
            ]
        );
        assert_eq!(
            glyph(&monitor, animated, HANDLED_AT + 50),
            Some(Role::SuccessMark)
        );
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
    fn under_reduce_motion_a_taken_in_row_restores_the_state_colour_without_a_flash() {
        let monitor = handled_while_listed(MonitorMembership::mark_taken_in);
        assert_eq!(fill(&monitor, reduced_motion(), HANDLED_AT + 50), None);
        assert_eq!(
            glyph(&monitor, reduced_motion(), HANDLED_AT + 50),
            Some(Role::SuccessMark)
        );
        assert_eq!(
            monitor.next_frame_ms(HANDLED_AT + 50, WIDTH, reduced_motion()),
            None
        );
    }

    #[test]
    fn the_user_opening_the_row_restores_the_state_colour_without_a_flash() {
        let monitor = handled_while_listed(MonitorMembership::mark_handled);
        let animated = DisplayProfile::default();
        assert_eq!(fill(&monitor, animated, HANDLED_AT + 50), None);
        assert_eq!(
            glyph(&monitor, animated, HANDLED_AT + 50),
            Some(Role::SuccessMark)
        );
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
            glyph(&monitor, animated, UPDATED_AT + 3_400),
            Some(Role::UpdateMark)
        );
        *update.lock().unwrap() = Some(UPDATED_AT + 4_000);
        monitor.rebuild_at(UPDATED_AT + 4_100);
        assert_eq!(
            glyph(&monitor, animated, UPDATED_AT + 4_500),
            Some(Role::UpdateMarkDim),
            "an update after coming back flashes"
        );
    }

    #[test]
    fn distinct_take_ins_flash_even_when_the_update_was_already_handled() {
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
            Some(Role::TakeInFlashStrong),
            "the turn took the update in while the View showed the Chat"
        );

        let mut taken_in_again = chat_handled(MonitorMembership::mark_taken_in);
        taken_in_again
            .monitor
            .mark_taken_in(quotes(), HANDLED_AT + 5_000);
        monitor.chat_status = Some(Arc::new(taken_in_again));
        monitor.rebuild_at(HANDLED_AT + 5_050);
        assert_eq!(
            fill(&monitor, animated, HANDLED_AT + 5_050),
            Some(Role::TakeInFlashStrong),
            "an explicitly delivered snapshot flashes even if its content was already handled"
        );
    }

    #[test]
    fn the_agent_using_the_instance_flashes_the_row_cold_and_leaves_the_update_unread() {
        let monitor = handled_while_listed(MonitorMembership::mark_used);
        let animated = DisplayProfile::default();
        let fills = [0, 399, 400, 799, 800, 1_199, 1_200, 5_000]
            .map(|elapsed| fill(&monitor, animated, HANDLED_AT + elapsed));
        assert_eq!(
            fills,
            [
                Some(Role::UseFlashStrong),
                Some(Role::UseFlashStrong),
                Some(Role::UseFlashSoft),
                Some(Role::UseFlashSoft),
                Some(Role::UseFlashStrong),
                Some(Role::UseFlashStrong),
                None,
                None
            ]
        );
        assert_eq!(
            glyph(&monitor, animated, HANDLED_AT + 50),
            Some(Role::UpdateMark),
            "an operation handles nothing, so the update stays unread"
        );
        assert_eq!(
            monitor.next_frame_ms(HANDLED_AT + 50, WIDTH, animated),
            Some(HANDLED_AT + 400)
        );
        assert_eq!(
            monitor.next_frame_ms(HANDLED_AT + 900, WIDTH, animated),
            Some(HANDLED_AT + 1_200)
        );
        assert_eq!(
            monitor.next_frame_ms(HANDLED_AT + 1_200, WIDTH, animated),
            None
        );
        assert_eq!(
            fill(&monitor, reduced_motion(), HANDLED_AT + 50),
            None,
            "under Reduce motion a use does not flash"
        );
        assert_eq!(
            monitor.next_frame_ms(HANDLED_AT + 50, WIDTH, reduced_motion()),
            None
        );
    }

    #[test]
    fn each_new_use_flashes_the_row_again_and_the_same_use_listed_again_does_not() {
        let animated = DisplayProfile::default();
        let mut monitor = handled_while_listed(MonitorMembership::mark_used);
        monitor.rebuild_at(HANDLED_AT + 2_000);
        assert_eq!(fill(&monitor, animated, HANDLED_AT + 2_000), None);
        assert_eq!(
            monitor.next_frame_ms(HANDLED_AT + 2_000, WIDTH, animated),
            None,
            "listing the same use again does not flash the row again"
        );
        for used_again in [HANDLED_AT + 3_000, HANDLED_AT + 3_500] {
            let mut chat = chat_handled(MonitorMembership::mark_used);
            chat.monitor.mark_used(quotes(), used_again);
            monitor.chat_status = Some(Arc::new(chat));
            monitor.rebuild_at(used_again + 50);
            assert_eq!(
                [0, 400].map(|elapsed| fill(&monitor, animated, used_again + elapsed)),
                [Some(Role::UseFlashStrong), Some(Role::UseFlashSoft)],
                "the use at {used_again} flashes the row from its start, also mid-flash"
            );
        }
    }

    #[test]
    fn a_use_from_before_the_view_showed_the_chat_never_flashes() {
        let animated = DisplayProfile::default();
        let (mut opened_after, _) = updated_monitor(None);
        opened_after.rebuild_at(HANDLED_AT + 50);
        opened_after.chat_status = Some(Arc::new(chat_handled(MonitorMembership::mark_used)));
        opened_after.rebuild_at(HANDLED_AT + 80);
        assert_eq!(fill(&opened_after, animated, HANDLED_AT + 80), None);

        let mut switched_back = handled_while_listed(MonitorMembership::mark_used);
        switched_back.settle_flashes(HANDLED_AT + 60);
        switched_back.rebuild_at(HANDLED_AT + 70);
        assert_eq!(
            fill(&switched_back, animated, HANDLED_AT + 70),
            None,
            "coming back to the Chat does not replay a use made while away"
        );
        assert_eq!(
            switched_back.next_frame_ms(HANDLED_AT + 70, WIDTH, animated),
            None
        );
    }

    #[test]
    fn the_later_of_a_take_in_and_a_use_covers_the_earlier_flash() {
        let animated = DisplayProfile::default();
        let second = HANDLED_AT + 500;
        let flashed =
            |first: fn(&mut MonitorMembership, InstanceRef, u64) -> bool,
             then: fn(&mut MonitorMembership, InstanceRef, u64) -> bool| {
                let mut monitor = handled_while_listed(first);
                let mut chat = chat_handled(first);
                then(&mut chat.monitor, quotes(), second);
                monitor.chat_status = Some(Arc::new(chat));
                monitor.rebuild_at(second + 10);
                monitor
            };
        // At 500 and 900 the first flash still runs, in its soft and then its strong phase.
        let used_later = flashed(
            MonitorMembership::mark_taken_in,
            MonitorMembership::mark_used,
        );
        assert_eq!(
            [500, 900, 1_300, 1_700].map(|elapsed| fill(
                &used_later,
                animated,
                HANDLED_AT + elapsed
            )),
            [
                Some(Role::UseFlashStrong),
                Some(Role::UseFlashSoft),
                Some(Role::UseFlashStrong),
                None
            ]
        );
        assert_eq!(
            used_later.next_frame_ms(HANDLED_AT + 1_300, WIDTH, animated),
            Some(HANDLED_AT + 1_700),
            "frames run until the later flash ends"
        );
        let taken_in_later = flashed(
            MonitorMembership::mark_used,
            MonitorMembership::mark_taken_in,
        );
        assert_eq!(
            [500, 900].map(|elapsed| fill(&taken_in_later, animated, HANDLED_AT + elapsed)),
            [Some(Role::TakeInFlashStrong), Some(Role::TakeInFlashSoft)]
        );
    }

    #[test]
    fn a_use_flash_starts_after_the_mark_and_a_selected_row_keeps_its_selection() {
        let animated = DisplayProfile::default();
        let monitor = handled_while_listed(MonitorMembership::mark_used);
        let line = row(&monitor, animated, HANDLED_AT + 50);
        assert_eq!(
            (line.fill, line.fill_start),
            (Some(Role::UseFlashStrong), 4),
            "a row without `↻` flashes from its first column"
        );

        let mut subscribed = subscribed(
            vec![waiting(TickSubscriptionFrequency::High, Some(70_000))],
            UPDATED_AT + 100,
        );
        subscribed.chat_status = Some(Arc::new(chat_handled(MonitorMembership::mark_used)));
        subscribed.rebuild_at(HANDLED_AT + 50);
        let line = row(&subscribed, animated, HANDLED_AT + 50);
        assert_eq!(
            (line.fill, line.fill_start, line.band),
            (Some(Role::UseFlashStrong), 4, None),
            "the flash starts after the `↻` and covers the bar"
        );

        subscribed.focus(false, None);
        let line = row(&subscribed, animated, HANDLED_AT + 50);
        assert_eq!(
            (line.fill, line.fill_start),
            (Some(Role::Selection), 1),
            "the selection wins over the flash"
        );
    }

    #[test]
    fn a_selected_unread_row_fills_from_its_first_column_and_keeps_the_update_colour() {
        let (mut monitor, _) = updated_monitor(Some(chat()));
        monitor.rebuild_at(UPDATED_AT + 100);
        monitor.focus(false, None);
        let line = row(&monitor, DisplayProfile::default(), UPDATED_AT + 100);
        assert_eq!(
            (line.fill, line.fill_start),
            (Some(Role::Selection), 0),
            "nothing keeps the first column off the selection"
        );
        assert!(line.text().starts_with("  ◉ Quotes"), "{line:?}");
        assert_eq!(
            glyph(&monitor, DisplayProfile::default(), UPDATED_AT + 100),
            Some(Role::UpdateMark)
        );
    }

    #[test]
    fn the_row_detail_tells_when_the_row_last_updated() {
        let (mut monitor, _) = updated_monitor(Some(chat()));
        monitor.rebuild_at(UPDATED_AT + 100);
        let facts = member_facts(&monitor.entries()[0], UPDATED_AT + 180_000);
        assert!(facts.contains(&"Updated   3m".to_owned()), "{facts:?}");
        assert!(
            !facts.iter().any(|fact| fact.starts_with("Waiting")),
            "an update nobody counted shows no count: {facts:?}"
        );
    }

    #[test]
    fn the_row_detail_counts_the_waiting_updates_the_row_no_longer_shows() {
        let (mut monitor, _) = updated_monitor(Some(chat()));
        monitor.rebuild_at(UPDATED_AT + 100);
        let mut entry = monitor.entries()[0].clone();
        entry.update.as_mut().unwrap().unread =
            Some(crate::monitor_rows::Unread { count: Some(26) });
        assert!(
            member_facts(&entry, UPDATED_AT + 200).contains(&"Waiting   26 updates".to_owned())
        );
        monitor.entries[0] = entry;
        let line = row(&monitor, DisplayProfile::default(), UPDATED_AT + 200);
        assert!(!line.text().contains("26"), "{line:?}");
    }

    fn subscription(
        frequency: TickSubscriptionFrequency,
        next_check_at_ms: Option<u64>,
    ) -> TickSubscriptionStatus {
        TickSubscriptionStatus {
            id: uuid::Uuid::from_u128(frequency as u128 + 2),
            loop_state: LoopState::Idle,
            instance: quotes(),
            frequency,
            accumulated: 0,
            has_pending_change: false,
            pending_since_ms: None,
            program_exit: None,
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

    /// The columns and role of the row's update bar, `None` without one.
    fn bar(monitor: &Monitor, now_ms: u64) -> Option<(std::ops::Range<usize>, Role)> {
        monitor
            .frame(WIDTH, 10, DisplayProfile::default(), now_ms)
            .lines[2]
            .band
            .clone()
            .map(|band| (band.columns, band.role))
    }

    fn waiting(
        frequency: TickSubscriptionFrequency,
        next_check_at_ms: Option<u64>,
    ) -> TickSubscriptionStatus {
        TickSubscriptionStatus {
            accumulated: 1,
            ..subscription(frequency, next_check_at_ms)
        }
    }

    #[test]
    fn a_late_first_change_starts_its_bar_at_the_arrival_and_fills_until_delivery() {
        let mut monitor = subscribed(
            vec![subscription(TickSubscriptionFrequency::High, Some(60_000))],
            0,
        );
        assert_eq!(bar(&monitor, 48_000), None);
        let mut status = serde_json::to_value(&monitor.subscriptions[0]).unwrap();
        status["has_pending_change"] = true.into();
        status["pending_since_ms"] = 49_000.into();
        monitor.subscriptions[0] = serde_json::from_value(status).unwrap();
        monitor.rebuild_at(49_000);
        let actual = bar(&monitor, 49_000);
        assert_eq!(actual, Some((4..5, Role::UpdateProgress)));

        assert_eq!(bar(&monitor, 54_500), Some((4..22, Role::UpdateProgress)));
        assert_eq!(bar(&monitor, 60_000), Some((4..40, Role::UpdateProgress)));

        let mut early = subscribed(
            vec![subscription(TickSubscriptionFrequency::High, Some(60_000))],
            0,
        );
        early.subscriptions[0].has_pending_change = true;
        early.rebuild_at(1_000);
        assert_eq!(bar(&early, 1_000), Some((4..5, Role::UpdateProgress)));
    }

    #[test]
    fn changing_a_periodic_frequency_keeps_the_unread_mark_and_bar() {
        let mut status = waiting(TickSubscriptionFrequency::High, Some(60_000));
        status.has_pending_change = true;
        status.updated_at_ms = Some(49_000);
        status.pending_since_ms = Some(49_000);
        let id = status.id;
        let mut monitor = subscribed(vec![status], 49_000);
        assert!(monitor.entries[0].is_unread());
        for (frequency, deadline) in [
            (TickSubscriptionFrequency::Medium, 650_000),
            (TickSubscriptionFrequency::Low, 1_850_000),
        ] {
            monitor.request_next_subscription(id, true).unwrap();
            assert!(
                monitor.entries[0].is_unread(),
                "optimistic {frequency:?} cleared unread"
            );
            assert!(bar(&monitor, 50_000).is_some());
            monitor.subscriptions[0].frequency = frequency;
            monitor.subscriptions[0].next_check_at_ms = Some(deadline);
            monitor.subscriptions[0].period_ms =
                Some(frequency.period().unwrap().as_millis() as u64);
            monitor.rebuild_at(50_000);
            assert!(
                monitor.entries[0].is_unread(),
                "confirmed {frequency:?} cleared unread"
            );
            assert!(bar(&monitor, 50_000).is_some());
        }
    }

    #[test]
    fn hosted_subscriptions_keep_their_terminal_or_protocol_type() {
        for (kind, glyph) in [
            (
                tickloop_agent_protocol::ObservationKind::Terminal { truncated: false },
                Glyph::TerminalTick,
            ),
            (
                tickloop_agent_protocol::ObservationKind::Protocol,
                Glyph::ProtocolTick,
            ),
        ] {
            let mut status = subscription(TickSubscriptionFrequency::High, Some(60_000));
            status.instance = InstanceRef::new("hosted", "typed-instance").unwrap();
            status.kind = Some(kind);
            let monitor = subscribed(vec![status], 0);
            assert_eq!(monitor.entries[0].glyph, glyph);
        }
    }

    #[test]
    fn a_subscribed_exited_terminal_keeps_its_type_and_stops_showing_running() {
        let mut status = subscription(TickSubscriptionFrequency::High, Some(60_000));
        status.instance = InstanceRef::new("hosted", "exited-terminal").unwrap();
        status.kind = Some(tickloop_agent_protocol::ObservationKind::Terminal { truncated: false });
        status.program_exit = Some(tickloop_agent_protocol::HostedProgramExit {
            code: Some(3),
            signal: None,
        });
        let monitor = subscribed(vec![status], 0);
        assert_eq!(monitor.entries[0].glyph, Glyph::TerminalTick);
        assert_eq!(monitor.entries[0].status, "exited 3");
        assert_eq!(monitor.entries[0].tone, StatusTone::Neutral);
    }

    #[test]
    fn an_unsubscribed_terminal_keeps_its_type_without_a_live_probe() {
        let instance = InstanceRef::new("hosted", "terminal-instance").unwrap();
        let mut chat = chat();
        chat.monitor
            .record_opened(instance.clone(), Some("sh watcher.sh".into()))
            .is_terminal = true;
        let mut monitor = Monitor {
            chat_status: Some(Arc::new(chat)),
            ..Default::default()
        };
        monitor.rebuild_at(0);
        let entry = monitor
            .entries
            .iter()
            .find(|entry| entry.instance == instance)
            .unwrap();
        assert_eq!(entry.glyph, Glyph::TerminalTick);
        assert_eq!(entry.kind, "Terminal");
    }

    #[test]
    fn a_quiet_periodic_row_keeps_its_state_colour_and_shows_no_bar() {
        let monitor = subscribed(
            vec![subscription(TickSubscriptionFrequency::High, Some(100_000))],
            70_000,
        );
        assert_eq!(bar(&monitor, 70_000), None);
        assert_eq!(
            glyph(&monitor, DisplayProfile::default(), 70_000),
            Some(Role::SuccessMark)
        );
        assert_eq!(
            lead(&monitor, DisplayProfile::default(), 70_000),
            ("↻".to_owned(), Some(Role::SubscriptionHigh)),
            "the `↻` leads the row in its frequency's colour"
        );
        assert_eq!(
            monitor.next_frame_ms(70_000, WIDTH, DisplayProfile::default()),
            None,
            "nothing waits, so nothing fills"
        );
    }

    #[test]
    fn a_waiting_update_fills_a_bar_from_the_first_column_until_the_check_hands_it_over() {
        let period = 60_000;
        let first_check = 100_000;
        let mut monitor = subscribed(
            vec![waiting(TickSubscriptionFrequency::High, Some(first_check))],
            first_check - period,
        );
        for (now, end) in [
            (first_check - period, 5),
            (first_check - 45_000, 13),
            (first_check - 30_000, 22),
            (first_check, 40),
            (first_check + 5_000, 40),
        ] {
            assert_eq!(
                bar(&monitor, now),
                Some((4..end, Role::UpdateProgress)),
                "at {now}"
            );
            assert_eq!(
                glyph(&monitor, DisplayProfile::default(), now),
                Some(Role::UpdateMark)
            );
            assert_eq!(
                lead(&monitor, DisplayProfile::default(), now),
                ("↻".to_owned(), Some(Role::SubscriptionHigh)),
                "the `↻` keeps its frequency's colour on the bar"
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
            Some(first_check - 30_000 + 1_667),
            "the next frame is when one more column fills"
        );
        assert_eq!(
            monitor.next_frame_ms(first_check, WIDTH, DisplayProfile::default()),
            None,
            "a full bar waits for the check"
        );

        monitor.focus(false, None);
        assert_eq!(
            bar(&monitor, first_check - 30_000),
            Some((4..22, Role::UpdateProgress)),
            "a selected row keeps the bar's colour over the selection"
        );

        monitor.focused = false;
        monitor.subscriptions = vec![subscription(
            TickSubscriptionFrequency::High,
            Some(first_check + period),
        )];
        monitor.rebuild_at(first_check + 100);
        assert_eq!(bar(&monitor, first_check + 100), None);
        assert_eq!(
            glyph(&monitor, DisplayProfile::default(), first_check + 100),
            Some(Role::SuccessMark),
            "the check handed the update over, or found nothing to hand over"
        );
        assert_eq!(
            lead(&monitor, DisplayProfile::default(), first_check + 100),
            ("↻".to_owned(), Some(Role::SubscriptionHigh))
        );
        assert_eq!(
            row(&monitor, DisplayProfile::default(), first_check + 100).fill,
            None,
            "only an Agent turn taking the update in flashes the row"
        );
    }

    #[test]
    fn a_new_waiting_update_flashes_the_glyph_while_its_bar_shows() {
        let animated = DisplayProfile::default();
        let mut monitor = subscribed(
            vec![waiting(TickSubscriptionFrequency::High, Some(100_000))],
            40_000,
        );
        monitor.chat_status = Some(Arc::new(chat()));
        let mut updated = waiting(TickSubscriptionFrequency::High, Some(100_000));
        updated.updated_at_ms = Some(50_000);
        monitor.subscriptions = vec![updated];
        monitor.rebuild_at(50_000);
        assert_eq!(glyph(&monitor, animated, 50_400), Some(Role::UpdateMarkDim));
        assert_eq!(
            bar(&monitor, 50_400),
            Some((4..10, Role::UpdateProgress)),
            "the bar does not wait for the flash"
        );
        assert_eq!(monitor.next_frame_ms(50_900, WIDTH, animated), Some(51_200));
        assert_eq!(glyph(&monitor, animated, 51_200), Some(Role::UpdateMark));
        assert_eq!(bar(&monitor, 51_200), Some((4..10, Role::UpdateProgress)));
    }

    /// A Tick that updates every second, quicker than a flash lasts, keeps its bar on screen and
    /// growing with the period while each update flashes its glyph anew.
    #[test]
    fn an_update_every_second_keeps_its_bar_growing_while_the_glyph_flashes() {
        let animated = DisplayProfile::default();
        let (period, check) = (60_000, 100_000);
        let mut monitor = subscribed(
            vec![waiting(TickSubscriptionFrequency::High, Some(check))],
            49_000,
        );
        monitor.chat_status = Some(Arc::new(chat()));
        let mut reached = 0;
        for second in (50_000..=58_000).step_by(1_000) {
            let mut updated = waiting(TickSubscriptionFrequency::High, Some(check));
            updated.updated_at_ms = Some(second);
            monitor.subscriptions = vec![updated];
            monitor.rebuild_at(second);
            for (elapsed_in_second, phase) in [
                (0, Role::UpdateMark),
                (400, Role::UpdateMarkDim),
                (800, Role::UpdateMark),
            ] {
                let now = second + elapsed_in_second;
                let filled = (now - (check - period)) as usize * (WIDTH - 4) / period as usize;
                assert_eq!(
                    bar(&monitor, now),
                    Some((4..4 + filled.max(1), Role::UpdateProgress)),
                    "at {now} the bar shows the part of the period elapsed"
                );
                assert!(filled >= reached, "the bar never shrinks");
                reached = filled;
                assert_eq!(glyph(&monitor, animated, now), Some(phase), "at {now}");
                assert_eq!(
                    lead(&monitor, animated, now),
                    ("↻".to_owned(), Some(Role::SubscriptionHigh))
                );
            }
        }
        assert_eq!(reached, 11, "the bar grew with the seconds");
    }

    /// A Terminal Tick's screen that changed since its subscription last got it flashes the glyph
    /// in the update colour and shows the bar to the check that reads it. Clearing the pending
    /// subscription sample clears the bar, but only handling content clears the known unread mark.
    #[test]
    fn a_changed_terminal_screen_shows_the_update_colour_and_bar_until_its_check() {
        let animated = DisplayProfile::default();
        let mut monitor = subscribed(
            vec![subscription(TickSubscriptionFrequency::High, Some(100_000))],
            40_000,
        );
        monitor.chat_status = Some(Arc::new(chat()));
        let mut changed = subscription(TickSubscriptionFrequency::High, Some(100_000));
        changed.has_pending_change = true;
        changed.updated_at_ms = Some(50_000);
        monitor.subscriptions = vec![changed];
        monitor.rebuild_at(50_000);
        assert_eq!(
            glyph(&monitor, animated, 50_400),
            Some(Role::UpdateMarkDim),
            "the change flashes the glyph"
        );
        assert_eq!(bar(&monitor, 50_400), Some((4..10, Role::UpdateProgress)));
        assert_eq!(glyph(&monitor, animated, 51_200), Some(Role::UpdateMark));
        assert_eq!(bar(&monitor, 51_200), Some((4..10, Role::UpdateProgress)));

        let mut settled = subscription(TickSubscriptionFrequency::High, Some(160_000));
        settled.updated_at_ms = Some(50_000);
        monitor.subscriptions = vec![settled];
        monitor.rebuild_at(100_100);
        assert_eq!(bar(&monitor, 100_100), None);
        assert_eq!(glyph(&monitor, animated, 100_100), Some(Role::UpdateMark));
        assert_eq!(row(&monitor, animated, 100_100).fill, None);
        let mut handled = chat();
        handled.monitor.mark_handled(quotes(), 100_100);
        monitor.chat_status = Some(Arc::new(handled));
        monitor.rebuild_at(100_200);
        assert_eq!(glyph(&monitor, animated, 100_200), Some(Role::SuccessMark));
    }

    #[test]
    fn several_subscriptions_fill_to_the_nearest_check_holding_updates_and_show_the_most_frequent()
    {
        let now = 1_800_000;
        let monitor = subscribed(
            vec![
                waiting(TickSubscriptionFrequency::Low, Some(now + 30_000)),
                subscription(TickSubscriptionFrequency::High, Some(now + 10_000)),
                waiting(TickSubscriptionFrequency::Medium, Some(now + 300_000)),
            ],
            now,
        );
        assert_eq!(
            monitor.entries()[0].subscription,
            Some(SubscriptionMark {
                frequency: TickSubscriptionFrequency::High,
                is_waiting: true,
                next_check: Some(NextCheck {
                    at_ms: now + 30_000,
                    wait_ms: 1_800_000,
                }),
            }),
            "the quiet high subscription's sooner check hands nothing over"
        );
        let line = &monitor
            .frame(WIDTH, 10, DisplayProfile::default(), now)
            .lines[2];
        assert_eq!(line.text().matches('↻').count(), 1, "{line:?}");
        assert!(
            !line.text().contains(['2', '3']),
            "no count of subscriptions: {line:?}"
        );
        assert_eq!(
            (line.spans[0].text.as_str(), line.spans[0].role),
            ("↻", Some(Role::SubscriptionHigh)),
            "on the bar the `↻` keeps the most frequent subscription's colour"
        );
        assert_eq!(
            bar(&monitor, now),
            Some((4..39, Role::UpdateProgress)),
            "the low subscription checks first, with 30 s of its 30 min left"
        );
    }

    #[test]
    fn an_immediate_subscription_shows_no_mark_and_no_bar() {
        let monitor = subscribed(
            vec![TickSubscriptionStatus {
                accumulated: 1,
                ..subscription(TickSubscriptionFrequency::Immediate, None)
            }],
            40_000,
        );
        assert_eq!(monitor.entries()[0].subscription, None);
        let line = &monitor
            .frame(WIDTH, 10, DisplayProfile::default(), 40_000)
            .lines[2];
        assert_eq!(line.band, None);
        assert!(!line.text().contains(['↻', '↯']), "{line:?}");
        assert_eq!(
            monitor.next_frame_ms(40_000, WIDTH, DisplayProfile::default()),
            None
        );
    }

    #[test]
    fn a_row_flash_covers_the_bar_until_it_ends() {
        let animated = DisplayProfile::default();
        let mut monitor = subscribed(
            vec![waiting(TickSubscriptionFrequency::High, Some(70_000))],
            HANDLED_AT - 1_000,
        );
        let mut status = waiting(TickSubscriptionFrequency::High, Some(70_000));
        status.updated_at_ms = Some(UPDATED_AT);
        monitor.subscriptions = vec![status];
        monitor.chat_status = Some(Arc::new(chat()));
        monitor.rebuild_at(UPDATED_AT + 100);
        monitor.chat_status = Some(Arc::new(chat_handled(MonitorMembership::mark_taken_in)));
        monitor.rebuild_at(HANDLED_AT + 50);
        let line = row(&monitor, animated, HANDLED_AT + 50);
        assert_eq!(line.fill, Some(Role::TakeInFlashStrong));
        assert_eq!(line.band, None);
        let after = row(&monitor, animated, HANDLED_AT + 1_200);
        assert_eq!(after.fill, None);
        assert_eq!(
            after.band.map(|band| band.role),
            Some(Role::UpdateProgress),
            "a newer update still waiting shows its bar once the flash ends"
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
            "folder": "/fixture",
            "messages": messages,
            "chat_loop": {"id":"00000000-0000-0000-0000-000000000000","state":"Idle"},
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

#[cfg(test)]
mod scheduled_tests {
    use super::*;
    use tickloop_agent_protocol::{ChatSnapshot, PauseReason, Receiving, ScheduledTask};

    fn task() -> ScheduledTask {
        serde_json::from_value(serde_json::json!({
            "id":"0123abcd", "kind":{"type":"recurring", "cron":"* * * * *"},
            "prompt":"check weather", "creator":"user", "time_zone":"Asia/Shanghai",
            "created_at_ms":0, "next_fire_at_ms":60000, "scheduled_for_ms":60000,
            "jitter_ms":0, "skipped":0
        }))
        .unwrap()
    }
    fn chat(task: ScheduledTask) -> ChatStatus {
        let snapshot: ChatSnapshot = serde_json::from_value(serde_json::json!({
            "id":uuid::Uuid::new_v4(), "messages":[], "scheduled_tasks":[task],
            "chat_loop":{"id":uuid::Uuid::nil(),"state":"Idle"}
        }))
        .unwrap();
        ChatStatus::of(&snapshot)
    }
    #[test]
    fn scheduled_rows_render_full_overdue_and_gray_paused_with_narrow_widths() {
        let mut monitor = Monitor {
            chat_status: Some(Arc::new(chat(task()))),
            ..Default::default()
        };
        monitor.rebuild_at(30000);
        assert_eq!(monitor.total(), 1);
        assert!(monitor.is_shown());
        assert!(
            monitor.entries().is_empty(),
            "scheduled tasks are not Ticks"
        );
        monitor.focus(false, None);
        assert_eq!(monitor.selected_task().unwrap().id, "0123abcd");
        let display = DisplayProfile::default();
        for width in [1, 5, 16, 42] {
            let frame = monitor.frame(width, 10, display, 60000);
            assert!(
                frame
                    .lines
                    .iter()
                    .all(
                        |line| unicode_width::UnicodeWidthStr::width(line.text().as_str()) <= width
                    )
            );
        }
        let frame = monitor.frame(42, 10, display, 60000);
        let band = frame
            .lines
            .iter()
            .find_map(|line| line.band.as_ref())
            .unwrap();
        assert_eq!(band.columns.end, 42);
        assert_eq!(band.role, Role::UpdateProgress);
        let mut status = chat(task());
        status.chat_loop.receiving = Receiving::Paused {
            reason: PauseReason::Stopped,
        };
        monitor.chat_status = Some(Arc::new(status));
        monitor.rebuild_at(60000);
        let frame = monitor.frame(42, 10, display, 60000);
        assert_eq!(
            frame
                .lines
                .iter()
                .find_map(|line| line.band.as_ref())
                .unwrap()
                .role,
            Role::UserMessageBg
        );
    }
    #[test]
    fn new_task_delivery_flashes_once_and_navigation_includes_tasks() {
        let mut monitor = Monitor {
            chat_status: Some(Arc::new(chat(task()))),
            ..Default::default()
        };
        monitor.rebuild_at(0);
        let mut fired = task();
        fired.last_fired_at_ms = Some(60000);
        fired.next_fire_at_ms = Some(120000);
        monitor.chat_status = Some(Arc::new(chat(fired)));
        monitor.rebuild_at(60000);
        let display = DisplayProfile::default();
        assert!(
            monitor
                .frame(42, 10, display, 60000)
                .lines
                .iter()
                .any(|line| line.fill == Some(Role::TakeInFlashStrong))
        );
        monitor.rebuild_at(62000);
        assert!(
            monitor
                .frame(42, 10, display, 62000)
                .lines
                .iter()
                .all(|line| line.fill != Some(Role::TakeInFlashStrong))
        );
        monitor.focus(false, None);
        monitor.move_selection(-1);
        assert!(monitor.selected_task().is_some());
        monitor.move_selection(1);
        assert!(monitor.selected_task().is_some());
    }
}

#[cfg(test)]
mod queued_scheduled_tests {
    use super::*;
    #[test]
    fn a_delivered_task_waiting_for_an_idle_loop_keeps_its_full_bar() {
        let chat: tickloop_agent_protocol::ChatSnapshot =
            serde_json::from_value(serde_json::json!({
                "id":uuid::Uuid::nil(), "messages":[],
                "chat_loop":{"id":uuid::Uuid::nil(),"state":"Running"},
                "scheduled_tasks":[{
                    "id":"0123abcd", "kind":{"type":"recurring", "cron":"* * * * *"},
                    "prompt":"weather", "creator":"user", "time_zone":"UTC", "created_at_ms":0,
                    "last_fired_at_ms":60000, "next_fire_at_ms":120000, "jitter_ms":0,"skipped":0
                }]
            }))
            .unwrap();
        let mut status = ChatStatus::of(&chat);
        status.pending_inputs = serde_json::from_value(serde_json::json!([{
            "receipt":{"chat_id":uuid::Uuid::nil(),"loop_id":uuid::Uuid::nil(),"run_id":uuid::Uuid::new_v4()},
            "input":"weather", "trigger_source":{"kind":"scheduled_task","task_id":"0123abcd","scheduled_for_ms":60000}, "armed":true
        }])).unwrap();
        let mut monitor = Monitor {
            chat_status: Some(Arc::new(status)),
            ..Default::default()
        };
        monitor.rebuild_at(61000);
        let frame = monitor.frame(42, 10, DisplayProfile::default(), 61000);
        let band = frame
            .lines
            .iter()
            .find_map(|line| line.band.as_ref())
            .unwrap();
        assert_eq!(band.columns.end, 42);
        assert!(frame.lines.iter().any(|line| line.text().contains("00:01")));
    }
}

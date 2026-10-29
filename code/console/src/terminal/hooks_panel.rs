//! `/hooks`: the hooks configured for a Folder, grouped by where they come from, each with its
//! event, matcher, `if` condition, command and trust. Space or `t` trusts the selected plugin or
//! repository hook, `x` twice disables it or restores it, and Enter shows its details with what
//! its trust covers, which the arrows and Ctrl+N/Ctrl+P scroll when they do not fit.
//! The review a New Agent offers once per Folder is the same panel opened on its menu; reviewing
//! one by one turns it into the list. The panel keeps what it shows and what the user chose; the
//! Console runs the work and hands each result back through [`HooksPanel::apply`].

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tickloop_harness::protocol::hooks::{
    ConfiguredHook, HookBackground, HookList, HookSource, HookTrust, HookTrustAction,
    HookTrustChange,
};
use uuid::Uuid;

use super::{
    console::wrap_text,
    conversation_picker::{ConversationPickerFrame, compact_line, fit_hint},
    display::{DisplayProfile, Glyph},
    list_row::ListRow,
    safe_line,
    style::{Role, RoleLine},
    ticks_panel::{Notice, step},
};

const TITLE: &str = "Hooks";
const REVIEW_TITLE: &str = "Hooks need review";
/// The title and the key hints.
const CHROME_ROWS: usize = 2;
const INDENT: &str = "  ";
const CONFIRMATION_WINDOW: Duration = Duration::from_secs(3);
const REVIEW_OPTIONS: [ReviewChoice; 3] = [
    ReviewChoice::ReviewEach,
    ReviewChoice::TrustAll,
    ReviewChoice::Continue,
];
const PLUGIN_COVERAGE: &str = "Trust covers this declaration and the plugin's whole content: updating, reinstalling or changing any of its files asks again.";
const REPOSITORY_COVERAGE: &str = "Trust covers this declaration and the Folder's .claude/hooks/ directory. Files the command runs from anywhere else are not covered: changing them does not ask again.";
const USER_COVERAGE: &str = "Your own settings.json hook: it runs without trust. Disabling it turns it off in every folder.";

/// Work the panel asks the Console to do; its result comes back as a [`HooksResult`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum HooksRequest {
    Load,
    /// Changes trust records while they are still at `expected_revision`, the revision the list
    /// the user saw was read at.
    Write {
        changes: Vec<HookTrustChange>,
        expected_revision: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct HooksWork {
    pub(super) panel: Uuid,
    pub(super) request: HooksRequest,
}

#[derive(Debug)]
pub(super) enum HooksResult {
    /// The hooks as the read numbered `read` found them; reads are numbered in the order they
    /// start, so one that started later is the more recent.
    Listed {
        read: u64,
        listing: Result<HookList, String>,
    },
    /// A write's outcome, with the hooks as they stand after it.
    Written(Result<HookList, String>),
}

pub(super) enum HooksPanelEffect {
    None,
    Close,
    Work(HooksRequest),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReviewChoice {
    ReviewEach,
    TrustAll,
    Continue,
}

impl ReviewChoice {
    fn label(self) -> &'static str {
        match self {
            Self::ReviewEach => "Review them one by one",
            Self::TrustAll => "Trust all and continue",
            Self::Continue => "Continue without trusting (they will not run)",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    /// The menu a New Agent offers when its Folder has hooks to review.
    Review {
        selected: usize,
    },
    List,
    Details,
}

enum Listing {
    Loading,
    Loaded(HookList),
    Failed(String),
}

/// The write under way, as the panel tells it once it ends.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Pending {
    Trust(String),
    Disable(String),
    Restore(String),
    TrustAll(usize),
}

impl Pending {
    fn busy_text(&self) -> String {
        match self {
            Self::Trust(name) => format!("Trusting {name}…"),
            Self::Disable(name) => format!("Disabling {name}…"),
            Self::Restore(name) => format!("Restoring {name}…"),
            Self::TrustAll(count) => format!("Trusting {}…", hook_count(*count)),
        }
    }

    fn done(&self) -> String {
        match self {
            Self::Trust(name) => format!("Trusted {name}"),
            Self::Disable(name) => format!("Disabled {name}"),
            Self::Restore(name) => format!("Restored {name}"),
            Self::TrustAll(count) => format!("Trusted {}", hook_count(*count)),
        }
    }

    fn failed(&self, error: &str) -> String {
        match self {
            Self::Trust(name) => format!("Cannot trust {name}: {error}"),
            Self::Disable(name) => format!("Cannot disable {name}: {error}"),
            Self::Restore(name) => format!("Cannot restore {name}: {error}"),
            Self::TrustAll(_) => format!("Cannot trust the hooks: {error}"),
        }
    }
}

pub(super) struct HooksPanel {
    id: Uuid,
    pub(super) chat: Option<Uuid>,
    view: View,
    listing: Listing,
    /// The number of the read `listing` holds; a read that started before it is stale.
    read: u64,
    selected: usize,
    top: usize,
    /// The first row of the details shown.
    details_top: usize,
    /// The hook a first `x` named, by source and key, and when.
    confirmation: Option<(HookSource, String, Instant)>,
    pending: Option<Pending>,
    notice: Option<Notice>,
    /// The body rows each list row or option was drawn on, for pointer selection.
    row_targets: Vec<(std::ops::Range<usize>, usize)>,
}

impl HooksPanel {
    /// `/hooks` for `chat`, or for the New Agent; its hooks are read next.
    pub(super) fn new(chat: Option<Uuid>) -> Self {
        Self {
            id: tickloop_ids::generate(),
            chat,
            view: View::List,
            listing: Listing::Loading,
            read: 0,
            selected: 0,
            top: 0,
            details_top: 0,
            confirmation: None,
            pending: None,
            notice: None,
            row_targets: Vec::new(),
        }
    }

    /// The review a New Agent offers for the hooks of `listing` that are new or changed.
    pub(super) fn review(listing: HookList) -> Self {
        let mut panel = Self::new(None);
        panel.view = View::Review { selected: 0 };
        panel.listing = Listing::Loaded(sorted(listing));
        panel
    }

    pub(super) fn id(&self) -> Uuid {
        self.id
    }

    pub(super) fn title(&self) -> &'static str {
        TITLE
    }

    /// Takes `result`; `false` once the panel has nothing more to show, as after trusting every
    /// hook from the review.
    pub(super) fn apply(&mut self, result: HooksResult) -> bool {
        match result {
            HooksResult::Listed { read, .. } if read < self.read => {}
            HooksResult::Listed { read, listing } => {
                self.read = read;
                self.replace_listing(listing.map(sorted));
            }
            HooksResult::Written(result) => {
                let pending = self.pending.take();
                match result {
                    Ok(listing) => {
                        if matches!(self.view, View::Review { .. }) {
                            return false;
                        }
                        if let Some(pending) = pending {
                            self.notice = Some(Notice::success(pending.done()));
                        }
                        self.replace_listing(Ok(sorted(listing)));
                    }
                    Err(error) => {
                        let text = match &pending {
                            Some(pending) => pending.failed(&error),
                            None => error,
                        };
                        self.notice = Some(Notice::failure(text));
                    }
                }
            }
        }
        true
    }

    /// Shows `listing` on the hook that was selected, wherever it now is.
    fn replace_listing(&mut self, listing: Result<HookList, String>) {
        let shown = self
            .selected_hook()
            .map(|hook| (hook.source.clone(), hook.key.clone()));
        self.listing = listing.map_or_else(Listing::Failed, Listing::Loaded);
        self.selected = shown
            .and_then(|(source, key)| {
                self.hooks()
                    .iter()
                    .position(|hook| hook.source == source && hook.key == key)
            })
            .unwrap_or(self.selected)
            .min(self.hooks().len().saturating_sub(1));
        if self.view == View::Details && self.selected_hook().is_none() {
            self.view = View::List;
        }
    }

    pub(super) fn report(&mut self, error: String) {
        self.notice = Some(Notice::failure(error));
    }

    pub(super) fn cancel_confirmation(&mut self) {
        self.confirmation = None;
    }

    pub(super) fn expire_confirmation(&mut self) -> bool {
        if self
            .confirmation
            .as_ref()
            .is_some_and(|(_, _, at)| at.elapsed() >= CONFIRMATION_WINDOW)
        {
            self.confirmation = None;
            true
        } else {
            false
        }
    }

    fn hooks(&self) -> &[ConfiguredHook] {
        match &self.listing {
            Listing::Loaded(listing) => &listing.hooks,
            _ => &[],
        }
    }

    fn selected_hook(&self) -> Option<&ConfiguredHook> {
        self.hooks().get(self.selected)
    }

    fn revision(&self) -> Option<String> {
        match &self.listing {
            Listing::Loaded(listing) => Some(listing.revision.clone()),
            _ => None,
        }
    }

    pub(super) fn key(&mut self, key: KeyEvent) -> HooksPanelEffect {
        let disable_key = key.code == KeyCode::Char('x') && key.modifiers == KeyModifiers::NONE;
        if !disable_key {
            self.confirmation = None;
        }
        let movement = match (key.code, key.modifiers) {
            (KeyCode::Up | KeyCode::Char('k'), KeyModifiers::NONE)
            | (KeyCode::Char('p'), KeyModifiers::CONTROL) => Some(false),
            (KeyCode::Down | KeyCode::Char('j'), KeyModifiers::NONE)
            | (KeyCode::Char('n'), KeyModifiers::CONTROL) => Some(true),
            _ => None,
        };
        if let View::Review { selected } = self.view {
            if let Some(down) = movement {
                self.view = View::Review {
                    selected: step(selected, REVIEW_OPTIONS.len(), down),
                };
                return HooksPanelEffect::None;
            }
            return match key.code {
                KeyCode::Enter => self.choose(REVIEW_OPTIONS[selected]),
                KeyCode::Esc if self.pending.is_none() => HooksPanelEffect::Close,
                _ => HooksPanelEffect::None,
            };
        }
        if let Some(down) = movement {
            if self.view == View::Details {
                self.details_top = if down {
                    self.details_top.saturating_add(1)
                } else {
                    self.details_top.saturating_sub(1)
                };
            } else {
                self.selected = step(self.selected, self.hooks().len(), down);
            }
            return HooksPanelEffect::None;
        }
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) if self.view == View::Details => {
                self.view = View::List;
                HooksPanelEffect::None
            }
            (KeyCode::Esc, _) => HooksPanelEffect::Close,
            (KeyCode::Enter, _) if self.view == View::List && self.selected_hook().is_some() => {
                self.view = View::Details;
                self.details_top = 0;
                HooksPanelEffect::None
            }
            (KeyCode::Char(' ' | 't'), KeyModifiers::NONE) => self.trust_selected(),
            (KeyCode::Char('x'), KeyModifiers::NONE) => self.disable_or_restore_selected(),
            (KeyCode::Char('r'), KeyModifiers::NONE) => HooksPanelEffect::Work(HooksRequest::Load),
            _ => HooksPanelEffect::None,
        }
    }

    fn choose(&mut self, choice: ReviewChoice) -> HooksPanelEffect {
        if self.pending.is_some() {
            return HooksPanelEffect::None;
        }
        match choice {
            ReviewChoice::ReviewEach => {
                self.view = View::List;
                self.selected = self
                    .hooks()
                    .iter()
                    .position(needs_review)
                    .unwrap_or_default();
                HooksPanelEffect::None
            }
            ReviewChoice::Continue => HooksPanelEffect::Close,
            ReviewChoice::TrustAll => {
                let changes = self
                    .hooks()
                    .iter()
                    .filter(|hook| needs_review(hook))
                    .filter_map(trust_change)
                    .collect::<Vec<_>>();
                let count = changes.len();
                self.write(changes, Pending::TrustAll(count))
            }
        }
    }

    fn trust_selected(&mut self) -> HooksPanelEffect {
        let Some(hook) = self.selected_hook() else {
            return HooksPanelEffect::None;
        };
        let name = hook_name(hook);
        let refusal = match (&hook.source, hook.trust) {
            (HookSource::User, _) => Some("Your own hooks run without trust".to_owned()),
            (_, HookTrust::Trusted) => Some(format!("{name} is already trusted")),
            (_, HookTrust::Disabled) => Some(format!("{name} is disabled; restore it with x")),
            _ => None,
        };
        let change = trust_change(hook);
        if let Some(refusal) = refusal {
            self.notice = Some(Notice::failure(refusal));
            return HooksPanelEffect::None;
        }
        match change {
            Some(change) => self.write(vec![change], Pending::Trust(name)),
            None => HooksPanelEffect::None,
        }
    }

    /// The first `x` asks to press it again; the second, on the same hook within the window,
    /// disables the hook or restores a disabled one.
    fn disable_or_restore_selected(&mut self) -> HooksPanelEffect {
        let Some(hook) = self.selected_hook() else {
            return HooksPanelEffect::None;
        };
        let (source, key) = (hook.source.clone(), hook.key.clone());
        let (name, is_disabled) = (hook_name(hook), hook.trust == HookTrust::Disabled);
        let confirmed = self
            .confirmation
            .as_ref()
            .is_some_and(|(pending, pending_key, at)| {
                *pending == source && *pending_key == key && at.elapsed() < CONFIRMATION_WINDOW
            });
        if !confirmed {
            self.confirmation = Some((source, key, Instant::now()));
            return HooksPanelEffect::None;
        }
        self.confirmation = None;
        let (action, pending) = if is_disabled {
            (HookTrustAction::Enable, Pending::Restore(name))
        } else {
            (HookTrustAction::Disable, Pending::Disable(name))
        };
        self.write(
            vec![HookTrustChange {
                source,
                key,
                action,
            }],
            pending,
        )
    }

    fn write(&mut self, changes: Vec<HookTrustChange>, pending: Pending) -> HooksPanelEffect {
        if self.pending.is_some() {
            self.notice = Some(Notice::failure(
                "Wait for it to finish before starting another".to_owned(),
            ));
            return HooksPanelEffect::None;
        }
        let Some(expected_revision) = self.revision() else {
            return HooksPanelEffect::None;
        };
        if changes.is_empty() {
            return HooksPanelEffect::None;
        }
        self.pending = Some(pending);
        self.notice = None;
        HooksPanelEffect::Work(HooksRequest::Write {
            changes,
            expected_revision,
        })
    }

    /// Selects the list row or option drawn on `row`; `true` when the click should also act on
    /// it as Enter does.
    pub(super) fn select_at_row(&mut self, row: usize) -> bool {
        self.confirmation = None;
        let Some(index) = self
            .row_targets
            .iter()
            .find(|(rows, _)| rows.contains(&row))
            .map(|(_, index)| *index)
        else {
            return false;
        };
        match &mut self.view {
            View::Review { selected } => *selected = index,
            View::List => self.selected = index,
            View::Details => return false,
        }
        true
    }

    fn status_lines(&self, width: usize) -> Vec<RoleLine> {
        let mut lines = Vec::new();
        let mut add = |text: &str, role: Role| {
            lines.extend(
                wrap_text(&safe_line(text), width)
                    .into_iter()
                    .map(|line| RoleLine::role(line, role)),
            );
        };
        if let Some(pending) = &self.pending {
            add(&pending.busy_text(), Role::Accent);
        }
        if let Some(notice) = &self.notice {
            add(
                &notice.text,
                if notice.is_failure {
                    Role::Error
                } else {
                    Role::Success
                },
            );
        }
        lines
    }

    fn body(&self, width: usize, display: DisplayProfile) -> Body {
        match self.view {
            View::Review { selected } => self.review_body(selected, width, display),
            View::Details => self.details_body(width, display),
            View::List => self.list_body(width, display),
        }
    }

    fn review_body(&self, selected: usize, width: usize, display: DisplayProfile) -> Body {
        let mut body = Body::default();
        let count = self
            .hooks()
            .iter()
            .filter(|hook| needs_review(hook))
            .count();
        for text in [
            format!(
                "{} in this folder {} new or changed.",
                hook_count(count),
                if count == 1 { "is" } else { "are" }
            ),
            "Plugin and repository hooks run commands on this computer only after you trust them."
                .to_owned(),
        ] {
            push_wrapped(&mut body, &text, "", width, Role::MutedText);
        }
        for (index, choice) in REVIEW_OPTIONS.iter().enumerate() {
            let row = body.lines.len();
            body.targets.push((row..row + 1, index));
            body.lines.push(if index == selected {
                body.focus = row..row + 1;
                RoleLine::role(
                    format!("{} {}", display.glyph(Glyph::Prompt), choice.label()),
                    Role::Text,
                )
                .filled(Role::Selection)
            } else {
                RoleLine::role(format!("{INDENT}{}", choice.label()), Role::Text)
            });
        }
        body.hint = fit_hint(
            &[("enter confirm".into(), 10), ("esc skip".into(), 9)],
            width,
            display,
        );
        body
    }

    fn details_body(&self, width: usize, display: DisplayProfile) -> Body {
        let mut body = Body::default();
        let Some(hook) = self.selected_hook() else {
            return body;
        };
        body.lines
            .push(RoleLine::role(safe_line(&hook_name(hook)), Role::Text));
        body.focus = 0..1;
        let mut field = |label: &str, value: &str, role: Role| {
            push_wrapped(&mut body, &format!("{label}: {value}"), INDENT, width, role);
        };
        field("Source", &source_heading(&hook.source), Role::MutedText);
        field("File", &hook.file.display().to_string(), Role::MutedText);
        field("Command", &hook.command, Role::Text);
        if let Some(condition) = &hook.condition {
            field("If", condition, Role::Text);
        }
        match &hook.resolved_command {
            Some(resolved) if *resolved != hook.command => field("Runs", resolved, Role::Text),
            Some(_) => {}
            None => field(
                "Needs configuration",
                "set the plugin's options in /plugins before it can run",
                Role::Warning,
            ),
        }
        field(
            "Timeout",
            &format!("{} seconds", hook.timeout_secs),
            Role::MutedText,
        );
        match &hook.background {
            None => {}
            Some(HookBackground::Async) => field(
                "Background",
                "async: runs without holding up the event",
                Role::Text,
            ),
            Some(HookBackground::AsyncRewake { .. }) => field(
                "Background",
                "asyncRewake: runs without holding up the event and wakes the agent on exit 2",
                Role::Text,
            ),
        }
        field("State", trust_label(hook.trust), trust_role(hook.trust));
        let coverage = match hook.source {
            HookSource::User => USER_COVERAGE,
            HookSource::Plugin { .. } => PLUGIN_COVERAGE,
            HookSource::Repository => REPOSITORY_COVERAGE,
        };
        push_wrapped(&mut body, coverage, INDENT, width, Role::MutedText);
        body.hint = self.details_hint(hook, false, width, display);
        body
    }

    /// The details' key hints, naming the scrolling when the details do not fit.
    fn details_hint(
        &self,
        hook: &ConfiguredHook,
        scrolls: bool,
        width: usize,
        display: DisplayProfile,
    ) -> String {
        let mut hints = Vec::new();
        if scrolls {
            hints.push(("↑↓ scroll".to_owned(), 9));
        }
        hints.extend(self.action_hints(hook));
        hints.push(("esc back".to_owned(), 9));
        fit_hint(&hints, width, display)
    }

    fn list_body(&self, width: usize, display: DisplayProfile) -> Body {
        let mut body = Body::default();
        let glyph = display.glyph(Glyph::AgentViewed);
        let mut heading = None;
        for (index, hook) in self.hooks().iter().enumerate() {
            let group = source_heading(&hook.source);
            if heading.as_ref() != Some(&group) {
                body.lines
                    .push(RoleLine::role(safe_line(&group), Role::Accent));
                heading = Some(group);
            }
            let row = body.lines.len();
            let name = format!(
                "{} · {}",
                hook_name(hook),
                compact_line(&safe_line(&hook.command))
            );
            let mut details = trust_label(hook.trust).to_owned();
            if hook.needs_configuration() {
                details.push_str(" · needs configuration");
            }
            body.lines.extend(
                ListRow {
                    selected: index == self.selected,
                    glyph,
                    glyph_role: trust_role(hook.trust),
                    name: &name,
                    details,
                    fill: None,
                    subscription: None,
                    bar_columns: None,
                }
                .lines(width),
            );
            if index == self.selected {
                body.focus = row..body.lines.len();
            }
            body.targets.push((row..body.lines.len(), index));
        }
        if body.lines.is_empty() {
            let (text, role) = match &self.listing {
                Listing::Loading => ("Loading…".to_owned(), Role::MutedText),
                Listing::Failed(error) => (safe_line(error), Role::Error),
                Listing::Loaded(_) => (
                    "No hooks are configured for this folder.".to_owned(),
                    Role::MutedText,
                ),
            };
            push_wrapped(&mut body, &text, INDENT, width, role);
        }
        if let Listing::Loaded(listing) = &self.listing {
            for warning in &listing.warnings {
                push_wrapped(&mut body, warning, "", width, Role::Warning);
            }
        }
        let mut hints = Vec::new();
        if let Some(hook) = self.selected_hook() {
            hints.push(("enter details".to_owned(), 10));
            hints.extend(self.action_hints(hook));
        }
        hints.push(("r refresh".to_owned(), 4));
        hints.push(("esc close".to_owned(), 9));
        body.hint = fit_hint(&hints, width, display);
        body
    }

    /// What Space and `x` do to `hook`, as the key hints name them.
    fn action_hints(&self, hook: &ConfiguredHook) -> Vec<(String, u8)> {
        let mut hints = Vec::new();
        if needs_review(hook) {
            hints.push(("space trust".to_owned(), 8));
        }
        hints.push((
            if hook.trust == HookTrust::Disabled {
                "x restore"
            } else {
                "x disable"
            }
            .to_owned(),
            7,
        ));
        hints
    }

    /// The red line a first `x` puts in place of the key hints.
    fn confirmation_line(&self) -> Option<RoleLine> {
        let (source, key, _) = self.confirmation.as_ref()?;
        let hook = self
            .hooks()
            .iter()
            .find(|hook| hook.source == *source && hook.key == *key)?;
        let verb = if hook.trust == HookTrust::Disabled {
            "restore"
        } else {
            "disable"
        };
        Some(RoleLine::role(
            format!("Press x again to {verb} {}", safe_line(&hook_name(hook))),
            Role::Error,
        ))
    }

    /// The title, one row of the focus, and the key hints.
    pub(super) fn minimum_frame_rows(&self, width: usize) -> usize {
        let body = self.body(width, DisplayProfile::default());
        CHROME_ROWS + body.focus.len().max(1) + self.status_lines(width).len()
    }

    pub(super) fn content_rows(&self, width: usize) -> usize {
        CHROME_ROWS
            + self.body(width, DisplayProfile::default()).lines.len()
            + self.status_lines(width).len()
    }

    pub(super) fn frame(
        &mut self,
        width: usize,
        height: usize,
        display: DisplayProfile,
    ) -> ConversationPickerFrame {
        let title = if matches!(self.view, View::Review { .. }) {
            REVIEW_TITLE
        } else {
            TITLE
        };
        let mut lines = vec![RoleLine::role(title, Role::DialogTitle)];
        let status = self.status_lines(width);
        let capacity = height.saturating_sub(CHROME_ROWS + status.len());
        let mut body = self.body(width, display);
        if self.view == View::Details
            && body.lines.len() > capacity
            && let Some(hook) = self.selected_hook()
        {
            body.hint = self.details_hint(hook, true, width, display);
        }
        // The list keeps its place and the review scrolls to its focus alone; the details
        // scroll as the user moves them.
        let mut focus_top = 0;
        let top = match self.view {
            View::List => &mut self.top,
            View::Details => &mut self.details_top,
            View::Review { .. } => &mut focus_top,
        };
        if self.view != View::Details {
            if body.focus.start < *top {
                *top = body.focus.start;
            }
            if body.focus.end > *top + capacity {
                *top = body.focus.end.saturating_sub(capacity);
            }
        }
        *top = (*top).min(body.lines.len().saturating_sub(capacity));
        let skip = *top;
        self.row_targets = body
            .targets
            .into_iter()
            .filter_map(|(rows, index)| {
                let start = rows.start.max(skip);
                let end = rows.end.min(skip + capacity);
                (start < end).then(|| (start - skip + 1..end - skip + 1, index))
            })
            .collect();
        lines.extend(body.lines.into_iter().skip(skip).take(capacity));
        lines.resize(height.saturating_sub(1 + status.len()), RoleLine::default());
        lines.extend(status);
        lines.push(
            self.confirmation_line()
                .unwrap_or_else(|| RoleLine::role(body.hint, Role::MutedText)),
        );
        lines.truncate(height);
        for line in &mut lines {
            line.truncate(width);
        }
        ConversationPickerFrame {
            lines,
            cursor: None,
        }
    }
}

#[derive(Default)]
struct Body {
    lines: Vec<RoleLine>,
    focus: std::ops::Range<usize>,
    targets: Vec<(std::ops::Range<usize>, usize)>,
    hint: String,
}

fn push_wrapped(body: &mut Body, text: &str, indent: &str, width: usize, role: Role) {
    body.lines.extend(
        wrap_text(&safe_line(text), width.saturating_sub(indent.len()).max(1))
            .into_iter()
            .map(|line| RoleLine::role(format!("{indent}{line}"), role)),
    );
}

/// Whether the hook waits for the user's trust: a plugin or repository hook never trusted, or
/// changed since.
pub(super) fn needs_review(hook: &ConfiguredHook) -> bool {
    hook.source != HookSource::User
        && matches!(hook.trust, HookTrust::Pending | HookTrust::Modified)
}

fn trust_change(hook: &ConfiguredHook) -> Option<HookTrustChange> {
    Some(HookTrustChange {
        source: hook.source.clone(),
        key: hook.key.clone(),
        action: HookTrustAction::Trust {
            hash: hook.hash.clone()?,
        },
    })
}

/// The user's hooks first, then the Folder's, then each plugin's, each in its declared order.
fn sorted(mut listing: HookList) -> HookList {
    listing.hooks.sort_by_key(|hook| match &hook.source {
        HookSource::User => (0, String::new()),
        HookSource::Repository => (1, String::new()),
        HookSource::Plugin { id } => (2, id.clone()),
    });
    listing
}

fn source_heading(source: &HookSource) -> String {
    match source {
        HookSource::User => "Your settings".to_owned(),
        HookSource::Repository => "This folder".to_owned(),
        HookSource::Plugin { id } => format!("Plugin {id}"),
    }
}

/// The hook's event, with its matcher and its `if` condition when it has them, so hooks that
/// differ only by their condition read apart.
fn hook_name(hook: &ConfiguredHook) -> String {
    let mut name = hook.event.name().to_owned();
    if let Some(matcher) = hook
        .matcher
        .as_deref()
        .filter(|matcher| !matcher.is_empty())
    {
        name.push(' ');
        name.push_str(&compact_line(matcher));
    }
    if let Some(condition) = &hook.condition {
        name.push_str(" if ");
        name.push_str(&compact_line(condition));
    }
    name
}

fn hook_count(count: usize) -> String {
    match count {
        1 => "1 hook".to_owned(),
        count => format!("{count} hooks"),
    }
}

fn trust_label(trust: HookTrust) -> &'static str {
    match trust {
        HookTrust::Trusted => "trusted",
        HookTrust::Pending => "pending",
        HookTrust::Modified => "modified",
        HookTrust::Disabled => "disabled",
    }
}

fn trust_role(trust: HookTrust) -> Role {
    match trust {
        HookTrust::Trusted => Role::Success,
        HookTrust::Pending | HookTrust::Modified => Role::Warning,
        HookTrust::Disabled => Role::MutedText,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tickloop_harness::protocol::hooks::HookEvent;

    const WIDTH: usize = 100;
    const HEIGHT: usize = 20;

    fn hook(source: HookSource, key: &str, trust: HookTrust) -> ConfiguredHook {
        ConfiguredHook {
            key: key.into(),
            hash: (source != HookSource::User).then(|| format!("hash-{key}")),
            source,
            file: "/work/.claude/settings.json".into(),
            event: HookEvent::PreToolUse,
            matcher: Some("Bash".into()),
            command: format!("./{key}.sh"),
            resolved_command: Some(format!("./{key}.sh")),
            condition: None,
            timeout_secs: 30,
            background: None,
            plugin: None,
            trust,
        }
    }

    fn listing(hooks: Vec<ConfiguredHook>) -> HookList {
        HookList {
            hooks,
            warnings: Vec::new(),
            revision: "r1".into(),
        }
    }

    fn plugin() -> HookSource {
        HookSource::Plugin {
            id: "guard@market".into(),
        }
    }

    fn loaded(hooks: Vec<ConfiguredHook>) -> HooksPanel {
        let mut panel = HooksPanel::new(None);
        panel.apply(HooksResult::Listed {
            read: 1,
            listing: Ok(listing(hooks)),
        });
        panel
    }

    fn press(panel: &mut HooksPanel, code: KeyCode) -> HooksPanelEffect {
        panel.key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn text(panel: &mut HooksPanel) -> String {
        panel
            .frame(WIDTH, HEIGHT, DisplayProfile::default())
            .lines
            .iter()
            .map(RoleLine::text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn selected(panel: &mut HooksPanel) -> Vec<String> {
        panel
            .frame(WIDTH, HEIGHT, DisplayProfile::default())
            .lines
            .iter()
            .filter(|line| line.fill == Some(Role::Selection))
            .map(|line| line.text().trim().to_owned())
            .collect()
    }

    fn written(effect: HooksPanelEffect) -> Vec<HookTrustChange> {
        match effect {
            HooksPanelEffect::Work(HooksRequest::Write {
                changes,
                expected_revision,
            }) => {
                assert_eq!(expected_revision, "r1");
                changes
            }
            _ => panic!("expected a write"),
        }
    }

    #[test]
    fn the_list_groups_sources_and_moves_with_arrows_and_control_keys() {
        let mut panel = loaded(vec![
            hook(plugin(), "lint", HookTrust::Pending),
            hook(HookSource::Repository, "format", HookTrust::Modified),
            hook(HookSource::User, "guard", HookTrust::Trusted),
        ]);
        let shown = text(&mut panel);
        let order = [
            "Your settings",
            "PreToolUse Bash · ./guard.sh",
            "This folder",
            "./format.sh",
            "Plugin guard@market",
            "./lint.sh",
        ]
        .map(|part| {
            shown
                .find(part)
                .unwrap_or_else(|| panic!("{part} in {shown}"))
        });
        assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{shown}");
        assert!(shown.contains("trusted") && shown.contains("modified"));
        assert!(
            shown.contains("enter details · x disable · r refresh · esc close"),
            "{shown}"
        );
        assert!(!shown.contains('❯'), "{shown}");
        assert_eq!(selected(&mut panel).len(), 1);
        assert!(selected(&mut panel)[0].contains("./guard.sh"));

        press(&mut panel, KeyCode::Down);
        assert!(selected(&mut panel)[0].contains("./format.sh"));
        assert!(text(&mut panel).contains("space trust · x disable"));
        panel.key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL));
        assert!(selected(&mut panel)[0].contains("./lint.sh"));
        press(&mut panel, KeyCode::Down);
        assert!(selected(&mut panel)[0].contains("./lint.sh"));
        panel.key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
        press(&mut panel, KeyCode::Up);
        assert!(selected(&mut panel)[0].contains("./guard.sh"));
        for digit in ['1', '2'] {
            assert!(matches!(
                press(&mut panel, KeyCode::Char(digit)),
                HooksPanelEffect::None
            ));
        }
        assert!(selected(&mut panel)[0].contains("./guard.sh"));
    }

    #[test]
    fn space_and_t_trust_the_hash_shown_and_refuse_what_needs_no_trust() {
        let mut panel = loaded(vec![
            hook(HookSource::User, "guard", HookTrust::Trusted),
            hook(HookSource::Repository, "format", HookTrust::Pending),
            hook(plugin(), "lint", HookTrust::Modified),
        ]);
        press(&mut panel, KeyCode::Char(' '));
        assert!(text(&mut panel).contains("Your own hooks run without trust"));
        press(&mut panel, KeyCode::Down);
        let changes = written(press(&mut panel, KeyCode::Char(' ')));
        assert_eq!(
            changes,
            vec![HookTrustChange {
                source: HookSource::Repository,
                key: "format".into(),
                action: HookTrustAction::Trust {
                    hash: "hash-format".into()
                },
            }]
        );
        assert!(text(&mut panel).contains("Trusting PreToolUse Bash…"));
        press(&mut panel, KeyCode::Down);
        assert!(matches!(
            press(&mut panel, KeyCode::Char('t')),
            HooksPanelEffect::None
        ));
        assert!(text(&mut panel).contains("Wait for it to finish"));

        let trusted = listing(vec![
            hook(HookSource::User, "guard", HookTrust::Trusted),
            hook(HookSource::Repository, "format", HookTrust::Trusted),
            hook(plugin(), "lint", HookTrust::Modified),
        ]);
        assert!(panel.apply(HooksResult::Written(Ok(trusted))));
        let shown = text(&mut panel);
        assert!(shown.contains("Trusted PreToolUse Bash"), "{shown}");
        assert!(selected(&mut panel)[0].contains("./lint.sh"));
        let changes = written(press(&mut panel, KeyCode::Char('t')));
        assert_eq!(changes[0].key, "lint");
        panel.apply(HooksResult::Written(Err("hook trust changed".into())));
        assert!(text(&mut panel).contains("Cannot trust PreToolUse Bash: hook trust changed"));
    }

    #[test]
    fn x_disables_on_the_second_press_and_anything_else_cancels() {
        let mut panel = loaded(vec![
            hook(HookSource::Repository, "format", HookTrust::Trusted),
            hook(plugin(), "lint", HookTrust::Disabled),
        ]);
        assert!(matches!(
            press(&mut panel, KeyCode::Char('x')),
            HooksPanelEffect::None
        ));
        let shown = text(&mut panel);
        assert!(
            shown
                .lines()
                .last()
                .unwrap()
                .contains("Press x again to disable PreToolUse Bash"),
            "{shown}"
        );
        press(&mut panel, KeyCode::Down);
        assert!(!text(&mut panel).contains("Press x again"));
        assert!(matches!(
            press(&mut panel, KeyCode::Char('x')),
            HooksPanelEffect::None
        ));
        assert!(text(&mut panel).contains("Press x again to restore PreToolUse Bash"));
        press(&mut panel, KeyCode::Char('r'));
        assert!(!text(&mut panel).contains("Press x again"));

        press(&mut panel, KeyCode::Char('x'));
        let changes = written(press(&mut panel, KeyCode::Char('x')));
        assert_eq!(changes[0].action, HookTrustAction::Enable);
        assert!(text(&mut panel).contains("Restoring PreToolUse Bash…"));

        let mut panel = loaded(vec![hook(
            HookSource::Repository,
            "format",
            HookTrust::Trusted,
        )]);
        press(&mut panel, KeyCode::Char('x'));
        panel.confirmation.as_mut().unwrap().2 -= CONFIRMATION_WINDOW;
        assert!(panel.expire_confirmation());
        assert!(matches!(
            press(&mut panel, KeyCode::Char('x')),
            HooksPanelEffect::None
        ));
        let changes = written(press(&mut panel, KeyCode::Char('x')));
        assert_eq!(changes[0].action, HookTrustAction::Disable);
    }

    #[test]
    fn enter_shows_details_with_what_trust_covers() {
        let mut repository = hook(HookSource::Repository, "format", HookTrust::Pending);
        repository.command = "./format.sh --all".into();
        repository.resolved_command = Some("./format.sh --all".into());
        let mut configured = hook(plugin(), "lint", HookTrust::Pending);
        configured.resolved_command = None;
        configured.background = Some(HookBackground::AsyncRewake {
            message: None,
            summary: None,
        });
        let mut panel = loaded(vec![repository, configured]);
        press(&mut panel, KeyCode::Enter);
        let shown = text(&mut panel);
        for part in [
            "Source: This folder",
            "File: /work/.claude/settings.json",
            "Command: ./format.sh --all",
            "Timeout: 30 seconds",
            "State: pending",
            "Folder's .claude/hooks/ directory",
            "anywhere else are not covered",
            "space trust · x disable · esc back",
        ] {
            assert!(shown.contains(part), "{part} in {shown}");
        }
        press(&mut panel, KeyCode::Esc);
        press(&mut panel, KeyCode::Down);
        press(&mut panel, KeyCode::Enter);
        let shown = text(&mut panel);
        assert!(shown.contains("Source: Plugin guard@market"), "{shown}");
        assert!(shown.contains("plugin's whole content"), "{shown}");
        assert!(shown.contains("Needs configuration"), "{shown}");
        assert!(shown.contains("Background: asyncRewake"), "{shown}");
        press(&mut panel, KeyCode::Esc);
        assert!(text(&mut panel).contains("enter details"));
        assert!(matches!(
            press(&mut panel, KeyCode::Esc),
            HooksPanelEffect::Close
        ));
    }

    /// The frame's rows run together without whitespace, so text the frame wraps anywhere
    /// reads as written; then its key hints.
    fn unwrapped(panel: &mut HooksPanel, width: usize, height: usize) -> (String, String) {
        let lines = panel
            .frame(width, height, DisplayProfile::default())
            .lines
            .iter()
            .map(RoleLine::text)
            .collect::<Vec<_>>();
        let hint = lines.last().cloned().unwrap_or_default();
        let shown = lines.concat().split_whitespace().collect();
        (shown, hint)
    }

    #[test]
    fn details_taller_than_a_short_screen_scroll_with_arrows_and_control_keys() {
        const SHORT_WIDTH: usize = 40;
        const SHORT_HEIGHT: usize = 12;
        let flags = (1..=30)
            .map(|flag| format!("--flag-{flag}"))
            .collect::<Vec<_>>()
            .join(" ");
        let mut long = hook(HookSource::Repository, "format", HookTrust::Pending);
        long.command = format!("./format.sh {flags} --last-flag");
        long.resolved_command = Some(long.command.clone());
        let mut panel = loaded(vec![long, hook(plugin(), "lint", HookTrust::Pending)]);
        press(&mut panel, KeyCode::Enter);

        let (shown, hint) = unwrapped(&mut panel, SHORT_WIDTH, SHORT_HEIGHT);
        assert!(shown.contains("Source:Thisfolder"), "{shown}");
        assert!(!shown.contains("--last-flag"), "{shown}");
        assert!(
            hint.contains("↑↓ scroll") && hint.contains("esc back"),
            "{hint}"
        );

        let mut seen = shown;
        for step in 0..40 {
            if step % 2 == 0 {
                press(&mut panel, KeyCode::Down);
            } else {
                panel.key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL));
            }
            seen.push(' ');
            seen.push_str(&unwrapped(&mut panel, SHORT_WIDTH, SHORT_HEIGHT).0);
        }
        let (bottom, _) = unwrapped(&mut panel, SHORT_WIDTH, SHORT_HEIGHT);
        assert!(seen.contains("--flag-15--flag-16"), "{seen}");
        assert!(seen.contains("--last-flag"), "{seen}");
        assert!(
            bottom.contains("Filesthecommandrunsfromanywhereelsearenotcovered"),
            "the coverage note is reachable and scrolling stops at it: {bottom}"
        );
        assert!(
            !seen.contains("./lint.sh"),
            "scrolling never moves to another hook"
        );

        for step in 0..40 {
            if step % 2 == 0 {
                press(&mut panel, KeyCode::Up);
            } else {
                panel.key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
            }
        }
        let (top, _) = unwrapped(&mut panel, SHORT_WIDTH, SHORT_HEIGHT);
        assert!(top.contains("Source:Thisfolder"), "{top}");

        press(&mut panel, KeyCode::Esc);
        press(&mut panel, KeyCode::Down);
        press(&mut panel, KeyCode::Enter);
        let (short, hint) = unwrapped(&mut panel, WIDTH, HEIGHT);
        assert!(short.contains("Command:./lint.sh"), "{short}");
        assert!(
            !hint.contains("scroll"),
            "details that fit name no scrolling: {hint}"
        );
    }

    #[test]
    fn rows_that_differ_only_by_their_if_condition_show_it_before_the_command() {
        let command = "bash \"${CLAUDE_PLUGIN_ROOT}/hooks/sg-python.sh\" \"${CLAUDE_PLUGIN_ROOT}/hooks/security_reminder_hook.py\"";
        let reviewer = |key: &str, condition: &str| {
            let mut reviewer = hook(plugin(), key, HookTrust::Trusted);
            reviewer.event = HookEvent::PostToolUse;
            reviewer.command = command.into();
            reviewer.condition = Some(condition.into());
            reviewer
        };
        let mut panel = loaded(vec![
            reviewer("commit", "Bash(git*commit*)"),
            reviewer("push", "Bash(git*push*)"),
            reviewer("stack", "Bash(gt\n  *)"),
        ]);
        let shown = text(&mut panel);
        for row in [
            "PostToolUse Bash if Bash(git*commit*) · bash",
            "PostToolUse Bash if Bash(git*push*) · bash",
            "PostToolUse Bash if Bash(gt *) · bash",
        ] {
            assert!(shown.contains(row), "{row} in {shown}");
        }
        press(&mut panel, KeyCode::Down);
        press(&mut panel, KeyCode::Char('x'));
        assert!(
            text(&mut panel)
                .contains("Press x again to disable PostToolUse Bash if Bash(git*push*)"),
            "{}",
            text(&mut panel)
        );
    }

    #[test]
    fn the_review_offers_three_choices_with_the_prompt_cursor() {
        let hooks = listing(vec![
            hook(HookSource::User, "guard", HookTrust::Trusted),
            hook(HookSource::Repository, "format", HookTrust::Pending),
            hook(plugin(), "lint", HookTrust::Modified),
            hook(plugin(), "old", HookTrust::Trusted),
        ]);
        let mut panel = HooksPanel::review(hooks.clone());
        let shown = text(&mut panel);
        assert!(shown.starts_with("Hooks need review"), "{shown}");
        assert!(shown.contains("2 hooks in this folder are new or changed."));
        assert!(shown.contains("❯ Review them one by one"), "{shown}");
        assert!(shown.contains("  Trust all and continue"));
        assert!(shown.contains("  Continue without trusting"));
        assert!(shown.contains("enter confirm · esc skip"));

        press(&mut panel, KeyCode::Enter);
        let shown = text(&mut panel);
        assert!(!shown.starts_with("Hooks need review"), "{shown}");
        assert!(selected(&mut panel)[0].contains("./format.sh"));

        let mut panel = HooksPanel::review(hooks.clone());
        panel.key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL));
        assert!(text(&mut panel).contains("❯ Trust all and continue"));
        let changes = written(press(&mut panel, KeyCode::Enter));
        assert_eq!(
            changes
                .iter()
                .map(|change| change.key.as_str())
                .collect::<Vec<_>>(),
            ["format", "lint"]
        );
        assert!(text(&mut panel).contains("Trusting 2 hooks…"));
        assert!(!panel.apply(HooksResult::Written(Ok(hooks.clone()))));

        let mut panel = HooksPanel::review(hooks.clone());
        press(&mut panel, KeyCode::Down);
        press(&mut panel, KeyCode::Enter);
        assert!(
            panel.apply(HooksResult::Written(Err("cannot write".into()))),
            "a failed trust keeps the review"
        );
        assert!(text(&mut panel).contains("Cannot trust the hooks: cannot write"));

        let mut panel = HooksPanel::review(hooks.clone());
        press(&mut panel, KeyCode::Down);
        press(&mut panel, KeyCode::Down);
        assert!(text(&mut panel).contains("❯ Continue without trusting"));
        assert!(matches!(
            press(&mut panel, KeyCode::Enter),
            HooksPanelEffect::Close
        ));
        let mut panel = HooksPanel::review(hooks);
        assert!(matches!(
            press(&mut panel, KeyCode::Esc),
            HooksPanelEffect::Close
        ));
    }
}

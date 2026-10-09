//! The "Add to Monitor" picker. Its Workspace tab lists the View's Herdr programs one page at a
//! time; its Removed tab lists instances removed from this Chat's Monitor. Choosing a row only
//! adds membership: it never subscribes, reads content or calls the model.

use std::{collections::BTreeSet, sync::Arc};

use async_trait::async_trait;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tickhost_connect::page::{MAX_PAGE_SIZE, Page, PageCursor};
use uuid::Uuid;

use super::tick_actions::{
    PreparedTickAction, TickActionBackend, TickActionContext, TickActionExecutor, TickActionUi,
    TickDetailKey, TickDetailLine, TickDetailView,
};
use crate::monitor_rows::{DiscoveredTick, DiscoveryError, MONITOR_ADD_KEY, TickDiscovery};
use tick_protocol::InstanceRef;

/// The Monitor list key that opens this picker; the picker's own actions report it too.
/// The list-level action has no single target instance.
const NO_INSTANCE: &str = "";

pub(super) fn open(discovery: Option<Arc<dyn TickDiscovery>>) -> Arc<dyn TickActionExecutor> {
    Arc::new(OpenPicker { discovery })
}

struct OpenPicker {
    discovery: Option<Arc<dyn TickDiscovery>>,
}

impl TickActionExecutor for OpenPicker {
    fn prepare(&self, ui: &mut TickActionUi<'_>) -> Result<PreparedTickAction, String> {
        let chat = ui.chat().ok_or("No selected Chat")?;
        let removed = ui.monitor_removed();
        if self.discovery.is_none() && removed.is_empty() {
            return Err(
                "Nothing to add: this View has no Herdr Workspace and this Chat removed no Ticks"
                    .into(),
            );
        }
        let picker =
            MonitorPicker::new(chat, ui.monitor_members(), removed, self.discovery.clone());
        let panel = picker.id;
        let backend: Arc<dyn TickActionBackend> = match &self.discovery {
            Some(discovery) => Arc::new(LoadPage {
                discovery: discovery.clone(),
                panel,
                cursor: None,
            }),
            None => Arc::new(Nothing),
        };
        ui.open_detail(Box::new(picker));
        Ok(PreparedTickAction::new(
            NO_INSTANCE,
            MONITOR_ADD_KEY,
            backend,
        ))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    Workspace,
    Removed,
}

enum Listing {
    Loading,
    Loaded(Page<DiscoveredTick>),
    Failed(String),
}

pub(super) struct MonitorPicker {
    id: Uuid,
    chat: Uuid,
    /// Instances this Chat opened or added explicitly; rows other sources show may be added.
    members: BTreeSet<InstanceRef>,
    removed: BTreeSet<InstanceRef>,
    discovery: Option<Arc<dyn TickDiscovery>>,
    source: Source,
    workspace: Listing,
    /// Where the visible Workspace page starts, and where each earlier page started.
    workspace_start: Option<PageCursor>,
    earlier_workspace_pages: Vec<Option<PageCursor>>,
    removed_page: usize,
    selected: usize,
    notice: Option<String>,
}

impl MonitorPicker {
    fn new(
        chat: Uuid,
        members: BTreeSet<InstanceRef>,
        removed: BTreeSet<InstanceRef>,
        discovery: Option<Arc<dyn TickDiscovery>>,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            chat,
            members,
            removed,
            source: if discovery.is_some() {
                Source::Workspace
            } else {
                Source::Removed
            },
            discovery,
            workspace: Listing::Loading,
            workspace_start: None,
            earlier_workspace_pages: Vec::new(),
            removed_page: 0,
            selected: 0,
            notice: None,
        }
    }

    /// Shows a loaded page. A restarted listing (the Workspace changed) drops earlier pages.
    fn load(&mut self, page: Page<DiscoveredTick>, restarted: bool) -> Option<()> {
        if restarted {
            self.workspace_start = None;
            self.earlier_workspace_pages.clear();
        }
        self.workspace = Listing::Loaded(page);
        self.selected = 0;
        Some(())
    }

    fn fail(&mut self, error: String) -> Option<()> {
        self.workspace = Listing::Failed(error);
        Some(())
    }

    fn added(&mut self, instance: InstanceRef) -> Option<()> {
        self.removed.remove(&instance);
        self.members.insert(instance);
        self.removed_page = self
            .removed_page
            .min(self.removed.len().saturating_sub(1) / MAX_PAGE_SIZE);
        self.selected = self.selected.min(self.rows().len().saturating_sub(1));
        self.notice = Some("Added to Monitor".into());
        Some(())
    }

    /// The Removed tab pages its local set with the same bound as every listing.
    fn removed_rows(&self) -> Vec<DiscoveredTick> {
        self.removed
            .iter()
            .skip(self.removed_page * MAX_PAGE_SIZE)
            .take(MAX_PAGE_SIZE)
            .map(|instance| DiscoveredTick {
                instance: Some(instance.clone()),
                title: instance.to_string(),
                kind: instance.tick_type().into(),
                state: "removed".into(),
            })
            .collect()
    }

    fn rows(&self) -> Vec<DiscoveredTick> {
        match (self.source, &self.workspace) {
            (Source::Removed, _) => self.removed_rows(),
            (Source::Workspace, Listing::Loaded(page)) => page.items.clone(),
            (Source::Workspace, _) => Vec::new(),
        }
    }

    fn has_next_page(&self) -> bool {
        match (self.source, &self.workspace) {
            (Source::Removed, _) => (self.removed_page + 1) * MAX_PAGE_SIZE < self.removed.len(),
            (Source::Workspace, Listing::Loaded(page)) => page.next.is_some(),
            (Source::Workspace, _) => false,
        }
    }

    fn has_previous_page(&self) -> bool {
        match self.source {
            Source::Removed => self.removed_page > 0,
            Source::Workspace => {
                !self.earlier_workspace_pages.is_empty()
                    && matches!(self.workspace, Listing::Loaded(_))
            }
        }
    }

    fn load_workspace(&mut self, cursor: Option<PageCursor>) -> TickDetailKey {
        let Some(discovery) = self.discovery.clone() else {
            return TickDetailKey::None;
        };
        self.workspace_start = cursor.clone();
        self.workspace = Listing::Loading;
        TickDetailKey::Action(PreparedTickAction::new(
            NO_INSTANCE,
            MONITOR_ADD_KEY,
            Arc::new(LoadPage {
                discovery,
                panel: self.id,
                cursor,
            }),
        ))
    }

    /// An explicit member that is not removed needs no second add.
    fn is_shown_member(&self, instance: &InstanceRef) -> bool {
        self.members.contains(instance) && !self.removed.contains(instance)
    }

    fn can_switch(&self) -> bool {
        self.discovery.is_some() && !self.removed.is_empty()
    }

    fn heading(&self) -> String {
        match (self.source, &self.workspace) {
            (Source::Removed, _) => format!(
                "Add to Monitor · Removed · page {} · {} Ticks",
                self.removed_page + 1,
                self.removed.len()
            ),
            (Source::Workspace, Listing::Loading) => "Add to Monitor · loading…".into(),
            (Source::Workspace, Listing::Failed(_)) => "Add to Monitor".into(),
            (Source::Workspace, Listing::Loaded(page)) => format!(
                "Add to Monitor · Workspace · page {} · {} programs",
                self.earlier_workspace_pages.len() + 1,
                page.total
            ),
        }
    }

    fn add_action(&mut self, row: DiscoveredTick) -> TickDetailKey {
        let Some(instance) = row.instance else {
            self.notice = Some("This program's identity is uncertain".into());
            return TickDetailKey::None;
        };
        if self.is_shown_member(&instance) {
            self.notice = Some("Already added to this Chat's Monitor".into());
            return TickDetailKey::None;
        }
        TickDetailKey::Action(PreparedTickAction::new(
            instance.to_string(),
            MONITOR_ADD_KEY,
            Arc::new(AddMember {
                panel: self.id,
                title: (self.source == Source::Workspace).then_some(row.title),
                instance,
            }),
        ))
    }
}

impl TickDetailView for MonitorPicker {
    fn id(&self) -> Uuid {
        self.id
    }
    fn chat(&self) -> Uuid {
        self.chat
    }
    fn keeps_empty_monitor(&self) -> bool {
        true
    }
    fn lines(&mut self, width: usize, height: usize, _now: u64) -> Vec<TickDetailLine> {
        let mut lines = vec![TickDetailLine::new(self.heading())];
        if let (Source::Workspace, Listing::Failed(error)) = (self.source, &self.workspace) {
            lines.push(TickDetailLine::new(super::safe_line(error)));
        }
        let rows = self.rows();
        let loading =
            self.source == Source::Workspace && matches!(self.workspace, Listing::Loading);
        if rows.is_empty() && !loading {
            lines.push(TickDetailLine::new("Nothing to add here"));
        }
        let capacity = height.saturating_sub(lines.len() + 1).max(1);
        let offset = self.selected.saturating_sub(capacity.saturating_sub(1));
        for (index, row) in rows.iter().enumerate().skip(offset).take(capacity) {
            let marker = if index == self.selected { "› " } else { "  " };
            let note = match &row.instance {
                Some(instance) if self.is_shown_member(instance) => " · added",
                Some(_) => "",
                None => " · identity uncertain",
            };
            lines.push(TickDetailLine::new(format!(
                "{marker}{} · {} · {}{note}",
                super::safe_line(&row.title),
                row.kind,
                row.state
            )));
        }
        if let Some(notice) = &self.notice {
            lines.push(TickDetailLine::new(notice.clone()));
        }
        lines.truncate(height);
        for line in &mut lines {
            line.line.truncate(width);
        }
        lines
    }
    fn hint(&self) -> String {
        let mut hints = vec!["enter add"];
        if self.has_next_page() {
            hints.push("n next page");
        }
        if self.has_previous_page() {
            hints.push("p previous page");
        }
        if self.can_switch() {
            hints.push(match self.source {
                Source::Workspace => "tab removed",
                Source::Removed => "tab workspace",
            });
        }
        hints.push("esc back");
        hints.join(" · ")
    }
    fn key(&mut self, key: KeyEvent, _width: usize, _height: usize) -> TickDetailKey {
        self.notice = None;
        match (key.code, key.modifiers) {
            (KeyCode::Esc | KeyCode::Left, _) => TickDetailKey::Back,
            (KeyCode::Down | KeyCode::Char('j'), KeyModifiers::NONE)
            | (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
                self.selected = (self.selected + 1).min(self.rows().len().saturating_sub(1));
                TickDetailKey::None
            }
            (KeyCode::Up | KeyCode::Char('k'), KeyModifiers::NONE)
            | (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
                self.selected = self.selected.saturating_sub(1);
                TickDetailKey::None
            }
            (KeyCode::Tab, _) if self.can_switch() => {
                self.source = match self.source {
                    Source::Workspace => Source::Removed,
                    Source::Removed => Source::Workspace,
                };
                self.selected = 0;
                TickDetailKey::None
            }
            (KeyCode::Char('n'), KeyModifiers::NONE) if self.has_next_page() => {
                if self.source == Source::Removed {
                    self.removed_page += 1;
                    self.selected = 0;
                    return TickDetailKey::None;
                }
                let Listing::Loaded(page) = &self.workspace else {
                    return TickDetailKey::None;
                };
                let next = page.next.clone();
                self.earlier_workspace_pages
                    .push(self.workspace_start.clone());
                self.load_workspace(next)
            }
            (KeyCode::Char('p'), KeyModifiers::NONE) if self.has_previous_page() => {
                if self.source == Source::Removed {
                    self.removed_page -= 1;
                    self.selected = 0;
                    return TickDetailKey::None;
                }
                let earlier = self.earlier_workspace_pages.pop().flatten();
                self.load_workspace(earlier)
            }
            (KeyCode::Enter, _) => match self.rows().get(self.selected).cloned() {
                Some(row) => self.add_action(row),
                None => TickDetailKey::None,
            },
            _ => TickDetailKey::None,
        }
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

struct Nothing;

#[async_trait]
impl TickActionBackend for Nothing {
    async fn execute(&self, _: TickActionContext<'_>) -> Result<(), String> {
        Ok(())
    }
}

struct LoadPage {
    discovery: Arc<dyn TickDiscovery>,
    panel: Uuid,
    cursor: Option<PageCursor>,
}

#[async_trait]
impl TickActionBackend for LoadPage {
    async fn execute(&self, ctx: TickActionContext<'_>) -> Result<(), String> {
        let loaded = match self.discovery.page(self.cursor.clone()).await {
            Err(DiscoveryError::CursorExpired) => {
                ctx.notify("The Workspace changed; showing its first page again")?;
                self.discovery.page(None).await.map(|page| (page, true))
            }
            other => other.map(|page| (page, false)),
        };
        ctx.update_detail::<MonitorPicker, _>(self.panel, |picker| match loaded {
            Ok((page, restarted)) => picker.load(page, restarted),
            Err(error) => picker.fail(format!("Cannot list Workspace programs: {error}")),
        })?;
        Ok(())
    }
}

struct AddMember {
    panel: Uuid,
    instance: InstanceRef,
    title: Option<String>,
}

#[async_trait]
impl TickActionBackend for AddMember {
    async fn execute(&self, ctx: TickActionContext<'_>) -> Result<(), String> {
        let membership = ctx
            .agent
            .add_monitor_member(ctx.chat(), self.instance.clone(), self.title.clone())
            .await
            .map_err(|error| format!("Cannot add to Monitor: {error}"))?;
        ctx.update_detail::<MonitorPicker, _>(self.panel, |picker| {
            picker.added(self.instance.clone())
        })?;
        ctx.console
            .update_monitor_membership(ctx.chat(), membership)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoDiscovery;
    #[async_trait]
    impl TickDiscovery for NoDiscovery {
        async fn page(
            &self,
            _: Option<PageCursor>,
        ) -> Result<Page<DiscoveredTick>, DiscoveryError> {
            Err(DiscoveryError::Unavailable("unused".into()))
        }
    }
    #[async_trait]
    impl crate::monitor_rows::TickProbe for NoDiscovery {
        fn tick_type(&self) -> &'static str {
            "herdr"
        }
        async fn probe(
            &self,
            _: uuid::Uuid,
            _: &[InstanceRef],
        ) -> Result<crate::monitor_rows::MemberFindings, DiscoveryError> {
            Err(DiscoveryError::Unavailable("unused".into()))
        }
    }

    fn program(index: usize) -> DiscoveredTick {
        DiscoveredTick {
            instance: Some(InstanceRef::new("herdr", &format!("p{index:03}")).unwrap()),
            title: format!("Program {index}"),
            kind: "Terminal".into(),
            state: "live".into(),
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn text(picker: &mut MonitorPicker) -> String {
        picker
            .lines(80, 40, 0)
            .into_iter()
            .map(|line| line.line.text())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_workspace_tab_shows_the_full_total_and_adds_only_new_verified_rows() {
        let member = program(1).instance.unwrap();
        let mut picker = MonitorPicker::new(
            Uuid::new_v4(),
            [member].into(),
            BTreeSet::new(),
            Some(Arc::new(NoDiscovery)),
        );
        let mut items = (0..3).map(program).collect::<Vec<_>>();
        items.push(DiscoveredTick {
            instance: None,
            title: "Unverified".into(),
            kind: "Program".into(),
            state: "identity uncertain".into(),
        });
        picker.load(
            Page {
                generation: "g".into(),
                total: 300,
                items,
                next: Some(PageCursor {
                    generation: "g".into(),
                    after: "x".into(),
                }),
            },
            true,
        );
        let shown = text(&mut picker);
        assert!(
            shown.contains("Workspace · page 1 · 300 programs"),
            "{shown}"
        );
        assert!(
            shown.contains("Program 1 · Terminal · live · added"),
            "{shown}"
        );
        assert_eq!(picker.hint(), "enter add · n next page · esc back");

        assert!(matches!(
            picker.key(key(KeyCode::Enter), 80, 40),
            TickDetailKey::Action(action) if action.instance == program(0).instance.unwrap().to_string()
        ));
        picker.key(key(KeyCode::Char('j')), 80, 40);
        assert!(matches!(
            picker.key(key(KeyCode::Enter), 80, 40),
            TickDetailKey::None
        ));
        assert!(text(&mut picker).contains("Already added"));
        for _ in 0..5 {
            picker.key(key(KeyCode::Char('j')), 80, 40);
        }
        assert!(matches!(
            picker.key(key(KeyCode::Enter), 80, 40),
            TickDetailKey::None
        ));
        assert!(text(&mut picker).contains("identity is uncertain"));
        assert!(matches!(
            picker.key(key(KeyCode::Char('n')), 80, 40),
            TickDetailKey::Action(_)
        ));
        assert!(text(&mut picker).contains("loading"));
        assert!(matches!(
            picker.key(key(KeyCode::Esc), 80, 40),
            TickDetailKey::Back
        ));
        assert!(picker.keeps_empty_monitor());
    }

    #[test]
    fn the_removed_tab_restores_removed_rows_without_a_workspace() {
        let shell = InstanceRef::shell(Uuid::new_v4());
        let mut picker = MonitorPicker::new(
            Uuid::new_v4(),
            BTreeSet::new(),
            [shell.clone()].into(),
            None,
        );
        let shown = text(&mut picker);
        assert!(shown.contains("Removed · page 1 · 1 Ticks"), "{shown}");
        assert!(shown.contains(shell.as_str()));
        assert_eq!(picker.hint(), "enter add · esc back");
        assert!(matches!(
            picker.key(key(KeyCode::Tab), 80, 40),
            TickDetailKey::None
        ));
        assert!(matches!(
            picker.key(key(KeyCode::Enter), 80, 40),
            TickDetailKey::Action(action) if action.instance == shell.to_string()
        ));
        picker.added(shell);
        assert!(text(&mut picker).contains("Nothing to add here"));
    }

    #[test]
    fn tab_switches_between_workspace_and_removed_when_both_exist() {
        let removed = InstanceRef::new("herdr", "gone-from-view").unwrap();
        let mut picker = MonitorPicker::new(
            Uuid::new_v4(),
            BTreeSet::new(),
            [removed.clone()].into(),
            Some(Arc::new(NoDiscovery)),
        );
        assert!(picker.hint().contains("tab removed"));
        picker.key(key(KeyCode::Tab), 80, 40);
        assert!(picker.hint().contains("tab workspace"));
        assert!(text(&mut picker).contains(removed.as_str()));
    }
}

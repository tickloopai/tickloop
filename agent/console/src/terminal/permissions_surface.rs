use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tickloop_agent_tools::{WEB_FETCH_TOOL_NAME, permissions::is_supported_rule_content};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
use uuid::Uuid;

use super::denials_surface::{DenialsEffect, DenialsSurface};
use super::editor::{Editor, EditorCommand};
use super::style::{Role, RoleLine};
use tickloop_agent_protocol::{
    ApiKeyStatus, AutoEnvironmentSource, AutoPolicy, AutoReviewerHealth, AutoReviewerPreference,
    AutoRuleChange, AutoRuleKind, AutoRuleSettings, AutoRuleSource, AutoSettingsRevision, Decision,
    ExecutionMode, PermissionChange, PermissionRule, PermissionRuleScope, PermissionRuleSource,
    PermissionRuleView, PermissionsRevision, PermissionsView,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Page {
    RecentlyDenied,
    Ask,
    Deny,
    Auto,
    Directories,
}

impl Page {
    const ALL: [Self; 2] = [Self::Auto, Self::RecentlyDenied];
    const RECENT_ONLY: [Self; 1] = [Self::RecentlyDenied];

    fn visible_for(mode: ExecutionMode) -> &'static [Self] {
        if mode == ExecutionMode::Auto {
            &Self::ALL
        } else {
            &Self::RECENT_ONLY
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::RecentlyDenied => "Recent activity",
            Self::Ask => "Ask",
            Self::Deny => "Deny",
            Self::Auto => "Auto mode",
            Self::Directories => "Directories",
        }
    }

    fn decision(self) -> Option<Decision> {
        match self {
            Self::Ask => Some(Decision::Ask),
            Self::Deny => Some(Decision::Deny),
            Self::RecentlyDenied | Self::Auto | Self::Directories => None,
        }
    }
}

#[cfg(test)]
fn page_header(active: Page, width: usize) -> String {
    page_header_line(active, width, &Page::ALL).text()
}

fn page_header_line(active: Page, width: usize, pages: &[Page]) -> RoleLine {
    let labels: Vec<_> = pages.iter().map(|page| page.label()).collect();
    tab_header_line(
        "Permissions",
        pages.iter().position(|page| *page == active).unwrap_or(0),
        &labels,
        width,
    )
}

pub(super) fn tab_header_line(
    title: &str,
    active: usize,
    labels: &[&str],
    width: usize,
) -> RoleLine {
    if width < 45 {
        let compact = format!("[{}]", labels[active]);
        let titled = format!("{title} {compact}");
        let mut header = RoleLine::default();
        if titled.width() <= width {
            header.push(format!("{title} "), Some(Role::DialogTitle));
        }
        header.push(compact, Some(Role::Selection));
        return header;
    }

    let tabs = labels
        .iter()
        .enumerate()
        .map(|(index, label)| {
            if index == active {
                format!("[{label}]")
            } else {
                (*label).to_owned()
            }
        })
        .collect::<Vec<_>>();
    let active_index = active;
    let mut start = active_index;
    let mut end = active_index + 1;
    let render_window = |start: usize, end: usize| {
        let previous = if start > 0 { "‹ " } else { "" };
        let next = if end < tabs.len() { " ›" } else { "" };
        format!("{title} · {previous}{}{next}", tabs[start..end].join("  "))
    };
    while start > 0 && render_window(start - 1, end).width() <= width {
        start -= 1;
    }
    while end < tabs.len() && render_window(start, end + 1).width() <= width {
        end += 1;
    }
    let mut header = RoleLine::role(title, Role::DialogTitle);
    header.push(" · ", Some(Role::MutedText));
    if start > 0 {
        header.push("‹ ", Some(Role::MutedText));
    }
    for (index, page) in labels[start..end].iter().enumerate() {
        if index > 0 {
            header.push("  ", None);
        }
        if start + index == active {
            header.push(format!("[{}]", *page), Some(Role::Selection));
        } else {
            header.push(*page, Some(Role::MutedText));
        }
    }
    if end < tabs.len() {
        header.push(" ›", Some(Role::MutedText));
    }
    header
}

pub(super) fn selected_row(label: impl Into<String>, selected: bool) -> RoleLine {
    let mut row = RoleLine::default();
    row.push(if selected { "❯ " } else { "  " }, Some(Role::Permission));
    row.push(
        label,
        Some(if selected { Role::Strong } else { Role::Text }),
    );
    if selected {
        row.filled(Role::Selection)
    } else {
        row
    }
}

fn labelled_row(label: &str, value: impl Into<String>) -> RoleLine {
    let mut row = RoleLine::role(label, Role::MutedText);
    row.push(value, Some(Role::Text));
    row
}

pub(super) fn plain_row(value: impl Into<String>) -> RoleLine {
    RoleLine::plain(value)
}

pub(super) fn muted_row(value: impl Into<String>) -> RoleLine {
    RoleLine::role(value, Role::MutedText)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Matcher {
    WholeTool,
    RuleContent,
    ExactInput,
}

impl Matcher {
    fn label(self) -> &'static str {
        match self {
            Self::WholeTool => "Whole tool",
            Self::RuleContent => "Rule content (command pattern or domain:host)",
            Self::ExactInput => "Exact JSON input",
        }
    }

    fn next(self) -> Self {
        match self {
            Self::WholeTool => Self::RuleContent,
            Self::RuleContent => Self::ExactInput,
            Self::ExactInput => Self::WholeTool,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RuleScope {
    Chat,
    Loop,
    Project,
}

impl RuleScope {
    fn label(self) -> &'static str {
        match self {
            Self::Chat => "Chat session",
            Self::Loop => "Current Loop",
            Self::Project => "Project",
        }
    }

    fn next(self, has_loop: bool) -> Self {
        match self {
            Self::Chat if has_loop => Self::Loop,
            Self::Chat | Self::Loop => Self::Project,
            Self::Project => Self::Chat,
        }
    }
}

fn source_label(source: PermissionRuleSource) -> &'static str {
    match source {
        PermissionRuleSource::ChatSession => "Chat session",
        PermissionRuleSource::ProjectUser => "Project rule",
        PermissionRuleSource::ProjectConstraint => "Project constraint",
        PermissionRuleSource::ManagedConstraint => "Managed constraint",
    }
}

fn scope_label(scope: &PermissionRuleScope) -> String {
    match scope {
        PermissionRuleScope::Chat => "Chat session".into(),
        PermissionRuleScope::Loop(id) => format!("Loop {id}"),
        PermissionRuleScope::Project(path) => format!("Project {}", path.display()),
    }
}

fn key_status_label(status: ApiKeyStatus) -> &'static str {
    match status {
        ApiKeyStatus::Saved => "saved",
        ApiKeyStatus::Environment => "from environment",
        ApiKeyStatus::Missing => "missing",
    }
}

fn auto_kind_label(kind: AutoRuleKind) -> &'static str {
    match kind {
        AutoRuleKind::SoftAllow => "Soft allow",
        AutoRuleKind::SoftDeny => "Soft deny",
        AutoRuleKind::HardDeny => "Hard deny",
    }
}

fn auto_source_label(source: &AutoRuleSource) -> String {
    match source {
        AutoRuleSource::Builtin { version } => format!("Built-in v{version}"),
        AutoRuleSource::User => "User".into(),
    }
}

fn environment_source_label(source: &AutoEnvironmentSource) -> String {
    match source {
        AutoEnvironmentSource::Builtin { version } => format!("Built-in v{version}"),
        AutoEnvironmentSource::UserReplacement => "User replacement".into(),
    }
}

struct AutoGroupState {
    kind: AutoRuleKind,
    selected: usize,
    scroll: usize,
    query: String,
}

impl AutoGroupState {
    fn new(kind: AutoRuleKind) -> Self {
        Self {
            kind,
            selected: 0,
            scroll: 0,
            query: String::new(),
        }
    }
}

enum AutoDraftPurpose {
    Add(AutoRuleKind),
    Update { kind: AutoRuleKind, id: String },
    Replace(AutoRuleKind),
    Environment,
    EnvironmentEntry(tickloop_agent_protocol::AutoEnvironmentEntry),
}

impl AutoDraftPurpose {
    fn title(&self) -> &'static str {
        match self {
            Self::Add(_) => "Add Auto rule",
            Self::Update { .. } => "Edit Auto rule",
            Self::Replace(_) => "Replace group contents",
            Self::Environment => "Replace Environment",
            Self::EnvironmentEntry(_) => "Edit Environment entry",
        }
    }
}

struct AutoDraft {
    purpose: AutoDraftPurpose,
    editor: Editor,
}

impl AutoDraft {
    fn new(purpose: AutoDraftPurpose, text: String) -> Self {
        let mut editor = Editor::new();
        editor.replace_draft(text.clone(), text.len());
        Self { purpose, editor }
    }

    fn change(&self) -> Result<PermissionChange, String> {
        let text = self.editor.draft().to_owned();
        let change = match &self.purpose {
            AutoDraftPurpose::Add(kind) => AutoRuleChange::Add { kind: *kind, text },
            AutoDraftPurpose::Update { id, .. } => AutoRuleChange::Update {
                id: id.clone(),
                text,
            },
            AutoDraftPurpose::Replace(kind) => AutoRuleChange::ReplaceGroup {
                kind: *kind,
                texts: text
                    .lines()
                    .filter(|line| !line.trim().is_empty())
                    .map(str::to_owned)
                    .collect(),
            },
            AutoDraftPurpose::Environment => AutoRuleChange::ReplaceEnvironment { text },
            AutoDraftPurpose::EnvironmentEntry(entry) => AutoRuleChange::UpdateEnvironmentEntry {
                previous: entry.clone(),
                text,
                directory: entry.directory.clone(),
                include_descendants: entry.include_descendants,
            },
        };
        Ok(PermissionChange::ChangeAutoRules { change })
    }
}

struct RuleDraft {
    original_id: Option<Uuid>,
    tool: String,
    matcher: Matcher,
    match_text: String,
    scope: RuleScope,
    scoped_loop_id: Option<Uuid>,
    field: usize,
    editor: Editor,
}

impl RuleDraft {
    fn new() -> Self {
        Self {
            original_id: None,
            tool: String::new(),
            matcher: Matcher::WholeTool,
            match_text: String::new(),
            scope: RuleScope::Chat,
            scoped_loop_id: None,
            field: 0,
            editor: Editor::new(),
        }
    }

    fn existing(rule: &PermissionRule) -> Self {
        let mut draft = Self::new();
        draft.original_id = Some(rule.id);
        draft.tool = rule.tool.clone();
        draft.matcher = if rule.specifier.is_some() {
            Matcher::RuleContent
        } else if rule.input.is_some() {
            Matcher::ExactInput
        } else {
            Matcher::WholeTool
        };
        draft.match_text = rule
            .specifier
            .clone()
            .or_else(|| rule.input.as_ref().map(ToString::to_string))
            .unwrap_or_default();
        draft.scope = if rule.persistent {
            RuleScope::Project
        } else if rule.loop_id.is_some() {
            RuleScope::Loop
        } else {
            RuleScope::Chat
        };
        draft.scoped_loop_id = rule.loop_id;
        draft.sync_editor();
        draft
    }

    fn sync_editor(&mut self) {
        let value = match self.field {
            0 => &self.tool,
            2 => &self.match_text,
            _ => return,
        };
        self.editor.replace_draft(value.clone(), value.len());
    }

    fn commit_editor(&mut self) {
        match self.field {
            0 => self.tool = self.editor.draft().to_owned(),
            2 => self.match_text = self.editor.draft().to_owned(),
            _ => {}
        }
    }

    fn set_field(&mut self, field: usize) {
        self.commit_editor();
        self.field = field.min(4);
        self.sync_editor();
    }

    fn cycle_scope(&mut self, current_loop_id: Option<Uuid>) {
        self.scope = self.scope.next(current_loop_id.is_some());
        self.scoped_loop_id = if self.scope == RuleScope::Loop {
            current_loop_id
        } else {
            None
        };
    }

    fn change(
        &mut self,
        decision: Decision,
        loop_id: Option<Uuid>,
    ) -> Result<PermissionChange, String> {
        self.commit_editor();
        let tool = self.tool.as_str();
        if tool.trim().is_empty() {
            return Err("Tool is required".into());
        }
        let (specifier, input) = match self.matcher {
            Matcher::WholeTool => (None, None),
            Matcher::RuleContent => {
                if self.match_text.trim().is_empty()
                    || !is_supported_rule_content(tool, &self.match_text)
                {
                    return Err(
                        "Rule content requires Bash or * with a command pattern, or WebFetch with domain:{hostname}"
                            .into(),
                    );
                }
                (Some(self.match_text.clone()), None)
            }
            Matcher::ExactInput => {
                let value = serde_json::from_str(&self.match_text)
                    .map_err(|error| format!("Invalid exact JSON input: {error}"))?;
                (None, Some(value))
            }
        };
        let scoped_loop = match self.scope {
            RuleScope::Loop => Some(
                self.scoped_loop_id
                    .or(loop_id)
                    .ok_or("No current Loop is available")?,
            ),
            RuleScope::Chat | RuleScope::Project => None,
        };
        Ok(PermissionChange::UpsertRule {
            rule: PermissionRule {
                id: self.original_id.unwrap_or_else(Uuid::new_v4),
                decision,
                tool: tool.to_owned(),
                specifier,
                input,
                loop_id: scoped_loop,
                persistent: self.scope == RuleScope::Project,
            },
        })
    }
}

enum Focus {
    AutoSetup(super::auto_setup_surface::AutoSetupSurface),
    List,
    Search(Editor),
    Detail,
    RuleForm(RuleDraft),
    DirectoryForm(Editor),
    ConfirmDelete(PermissionChange),
    ReviewerChoice(usize),
    AutoGroup(AutoGroupState),
    AutoGroupSearch {
        group: AutoGroupState,
        editor: Editor,
    },
    AutoRuleDetail {
        group: AutoGroupState,
        id: String,
        scroll: usize,
    },
    AutoEnvironment {
        selected: usize,
        scroll: usize,
    },
    AutoDraft(AutoDraft),
    AutoPreview {
        change: PermissionChange,
        title: String,
        removed: Vec<String>,
        scroll: usize,
    },
}

pub(super) enum PermissionsEffect {
    GenerateEnvironment {
        generation: Uuid,
        options: tickloop_agent_protocol::AutoSetupOptions,
    },
    None,
    Close {
        destination: PermissionsReturn,
    },
    Apply(PermissionChange),
    OpenJudgmentSettings {
        select_reviewer: bool,
    },
    RefreshDenials,
    RetryDenial(Uuid),
}

#[derive(Clone, Copy)]
pub(super) enum PermissionsReturn {
    ModePicker { settings: bool },
}

/// The saved-policy revisions a New Agent Permissions view was read at. A saved edit from that
/// view is refused when either changed, like a visible Agent's `PermissionsRevision`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct DraftPolicyRevision {
    pub(super) project_rules: String,
    pub(super) auto_settings: AutoSettingsRevision,
}

/// Whose permissions a surface shows: a visible Agent at the revision its view was read, or the
/// New Agent screen for a project at the saved-policy revisions its view was read.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum PermissionsSubject {
    Agent {
        chat: Uuid,
        revision: PermissionsRevision,
    },
    NewDraft {
        project: PathBuf,
        revision: DraftPolicyRevision,
    },
}

impl PermissionsSubject {
    pub(super) fn chat(&self) -> Option<Uuid> {
        match self {
            Self::Agent { chat, .. } => Some(*chat),
            Self::NewDraft { .. } => None,
        }
    }

    /// Whether `other` is a newer view of the same Agent or New Agent screen.
    fn same_target(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Agent { chat, .. }, Self::Agent { chat: other, .. }) => chat == other,
            (Self::NewDraft { project, .. }, Self::NewDraft { project: other, .. }) => {
                project == other
            }
            _ => false,
        }
    }
}

/// What the surface shows: a `PermissionsView` without its identity. Reviewer health and Loops
/// exist only once an Agent runs, so a New Agent has none.
#[derive(Clone, Debug)]
pub(super) struct PermissionsContent {
    pub(super) execution_mode: ExecutionMode,
    pub(super) chat_model_label: String,
    pub(super) preferred_reviewer: AutoReviewerPreference,
    pub(super) auto_policy: AutoPolicy,
    pub(super) auto_rule_settings: AutoRuleSettings,
    pub(super) jev_key_status: ApiKeyStatus,
    pub(super) auto_reviewer_health: Option<AutoReviewerHealth>,
    pub(super) selected_loop_id: Option<Uuid>,
    pub(super) rules: Vec<PermissionRuleView>,
    pub(super) working_directory: PathBuf,
    pub(super) additional_directories: Vec<PathBuf>,
    pub(super) warnings: Vec<String>,
}

/// A Permissions view with the subject it belongs to.
pub(super) struct PermissionsPage {
    pub(super) subject: PermissionsSubject,
    pub(super) content: PermissionsContent,
}

impl From<PermissionsView> for PermissionsPage {
    fn from(view: PermissionsView) -> Self {
        Self {
            subject: PermissionsSubject::Agent {
                chat: view.chat_id,
                revision: view.revision,
            },
            content: PermissionsContent {
                execution_mode: view.execution_mode,
                chat_model_label: view.chat_model_label,
                preferred_reviewer: view.preferred_reviewer,
                auto_policy: view.auto_policy,
                auto_rule_settings: view.auto_rule_settings,
                jev_key_status: view.jev_key_status,
                auto_reviewer_health: Some(view.auto_reviewer_health),
                selected_loop_id: view.selected_loop_id,
                rules: view.rules,
                working_directory: view.working_directory,
                additional_directories: view.additional_directories,
                warnings: view.warnings,
            },
        }
    }
}

struct ListContent {
    intro: Vec<RoleLine>,
    rows: Vec<RoleLine>,
    hint: &'static str,
}

pub(super) struct PermissionsSurface {
    subject: PermissionsSubject,
    view: PermissionsContent,
    denials: DenialsSurface,
    page: Page,
    selected: usize,
    scroll: usize,
    detail_scroll: usize,
    query: String,
    focus: Focus,
    destination: PermissionsReturn,
    message: Option<String>,
    /// Neutral information about the last change, shown when there is no status to report; the
    /// next key clears it.
    notice: Option<String>,
    /// Why rules, directories and Auto settings cannot change here; activity stays readable.
    read_only: Option<&'static str>,
}

/// Footer hints that open an editing form; a read-only surface never offers them.
const EDITING_HINTS: &[&str] = &[
    "a add",
    "e edit",
    "d remove",
    "Enter edit/preview",
    "Enter configure",
];

impl PermissionsSurface {
    pub(super) fn open(page: impl Into<PermissionsPage>, destination: PermissionsReturn) -> Self {
        let PermissionsPage { subject, content } = page.into();
        Self {
            subject,
            view: content,
            denials: DenialsSurface::default(),
            page: Page::RecentlyDenied,
            selected: 0,
            scroll: 0,
            detail_scroll: 0,
            query: String::new(),
            focus: Focus::List,
            destination,
            message: None,
            notice: None,
            read_only: None,
        }
    }

    pub(super) fn make_read_only(&mut self, reason: &'static str) {
        self.read_only = Some(reason);
        self.message = Some(format!("Read-only: {reason}"));
    }

    pub(super) fn open_auto(&mut self) {
        self.page = Page::Auto;
        self.selected = 0;
        self.scroll = 0;
        self.focus = Focus::List;
    }

    pub(super) fn chat_id(&self) -> Option<Uuid> {
        self.subject.chat()
    }

    pub(super) fn open_environment_setup(&mut self) {
        if self.read_only.is_some() {
            self.message = Some("Environment setup is unavailable in this read-only view".into());
            return;
        }
        if self.subject.chat().is_none() {
            self.message = Some("Enter a task before generating an Environment draft".into());
            return;
        }
        if let Some(text) = &self.view.auto_rule_settings.environment_replacement {
            self.preview(
                PermissionChange::ChangeAutoRules {
                    change: AutoRuleChange::MigrateEnvironment {
                        expected_text: text.clone(),
                    },
                },
                "Migrate legacy Environment".into(),
            );
            return;
        }
        self.page = Page::Auto;
        self.focus = Focus::AutoSetup(super::auto_setup_surface::AutoSetupSurface::new(
            self.view.working_directory.clone(),
        ));
    }

    pub(super) fn environment_generated(
        &mut self,
        generation: Uuid,
        result: Result<tickloop_agent_protocol::AutoEnvironmentDraft, String>,
    ) {
        let result = result.and_then(|draft| match &self.subject {
            PermissionsSubject::Agent { revision, .. } if *revision == draft.revision => Ok(draft),
            _ => Err(
                "Permissions changed; refresh and generate a new draft. Nothing applied.".into(),
            ),
        });
        if let Focus::AutoSetup(setup) = &mut self.focus {
            setup.finish(generation, result);
        }
    }

    pub(super) fn subject(&self) -> &PermissionsSubject {
        &self.subject
    }

    /// Whether `page` is a newer view of what this surface shows.
    pub(super) fn shows(&self, page: &PermissionsPage) -> bool {
        self.subject.same_target(&page.subject)
    }

    fn set_page(&mut self, page: impl Into<PermissionsPage>) {
        let PermissionsPage { subject, content } = page.into();
        self.subject = subject;
        self.view = content;
    }

    pub(super) fn replace_view(&mut self, view: impl Into<PermissionsPage>) {
        let next_focus = match std::mem::replace(&mut self.focus, Focus::List) {
            Focus::AutoDraft(draft) => match draft.purpose {
                AutoDraftPurpose::Environment | AutoDraftPurpose::EnvironmentEntry(_) => {
                    Focus::AutoEnvironment {
                        selected: 0,
                        scroll: 0,
                    }
                }
                AutoDraftPurpose::Add(kind)
                | AutoDraftPurpose::Replace(kind)
                | AutoDraftPurpose::Update { kind, .. } => {
                    Focus::AutoGroup(AutoGroupState::new(kind))
                }
            },
            Focus::AutoPreview {
                change: PermissionChange::ChangeAutoRules { change },
                ..
            } => match change {
                AutoRuleChange::ReplaceEnvironment { .. }
                | AutoRuleChange::RestoreEnvironment
                | AutoRuleChange::MigrateEnvironment { .. }
                | AutoRuleChange::AddEnvironmentEntries { .. }
                | AutoRuleChange::RemoveEnvironmentEntries { .. }
                | AutoRuleChange::UpdateEnvironmentEntry { .. } => Focus::AutoEnvironment {
                    selected: 0,
                    scroll: 0,
                },
                AutoRuleChange::ReplaceGroup { kind, .. }
                | AutoRuleChange::RestoreGroup { kind } => {
                    Focus::AutoGroup(AutoGroupState::new(kind))
                }
                AutoRuleChange::Remove { id } => self
                    .view
                    .auto_policy
                    .rules
                    .iter()
                    .find(|rule| rule.id == id)
                    .map(|rule| Focus::AutoGroup(AutoGroupState::new(rule.kind)))
                    .unwrap_or(Focus::List),
                _ => Focus::List,
            },
            _ => Focus::List,
        };
        self.set_page(view);
        self.message = Some("Permissions saved".into());
        self.focus = next_focus;
        self.clamp_selection();
    }

    pub(super) fn refresh_view(&mut self, view: impl Into<PermissionsPage>) {
        self.set_page(view);
        self.focus = Focus::List;
        self.clamp_selection();
    }

    pub(super) fn key_and_reviewer_saved(&mut self, view: impl Into<PermissionsPage>) {
        self.set_page(view);
        self.focus = Focus::List;
        self.message = Some(
            "Jev (TypeSafe) key saved and selected; authentication has not been verified".into(),
        );
    }

    pub(super) fn key_saved_choice_failed(
        &mut self,
        view: impl Into<PermissionsPage>,
        reason: String,
    ) {
        self.set_page(view);
        self.focus = Focus::ReviewerChoice(1);
        self.message = Some(format!(
            "Jev (TypeSafe) key saved; reviewer choice not applied: {reason}"
        ));
    }

    pub(super) fn conflict(&mut self, view: impl Into<PermissionsPage>) {
        self.set_page(view);
        if let Focus::AutoPreview {
            change:
                PermissionChange::ChangeAutoRules {
                    change: AutoRuleChange::UpdateEnvironmentEntry { previous, .. },
                },
            ..
        } = &mut self.focus
            && let Some(current) = self
                .view
                .auto_rule_settings
                .environment_entries
                .iter()
                .find(|entry| entry.id == previous.id)
        {
            *previous = current.clone();
        }
        if let Focus::AutoPreview { change, .. } = &self.focus {
            let refreshed = self.preview_removed(change);
            if let Focus::AutoPreview {
                removed, scroll, ..
            } = &mut self.focus
            {
                *removed = refreshed;
                *scroll = 0;
            }
            self.message = Some(
                "Permissions changed elsewhere; preview refreshed. Review before confirming again"
                    .into(),
            );
        } else {
            self.message =
                Some("Permissions changed elsewhere; review and save this draft again".into());
        }
        self.clamp_selection();
    }

    pub(super) fn replace_denials(&mut self, view: tickloop_agent_protocol::ApprovalHistoryView) {
        self.denials.replace_view(view);
    }

    pub(super) fn denials_error(&mut self, message: String) {
        self.denials.set_error(message);
    }

    pub(super) fn error(&mut self, message: String) {
        self.message = Some(message);
    }

    pub(super) fn notice(&mut self, notice: String) {
        self.message = None;
        self.notice = Some(notice);
    }

    fn has_status_row(&self) -> bool {
        self.message.is_some() || self.notice.is_some() || !self.view.warnings.is_empty()
    }

    pub(super) fn external_draft(&self) -> Option<String> {
        match &self.focus {
            Focus::AutoSetup(setup) => setup.external_draft(),
            Focus::AutoDraft(draft) => Some(draft.editor.draft().to_owned()),
            _ => None,
        }
    }

    pub(super) fn replace_external_draft(&mut self, text: String) {
        match &mut self.focus {
            Focus::AutoSetup(setup) => setup.replace_external_draft(text),
            Focus::AutoDraft(draft) => {
                let len = text.len();
                draft.editor.replace_draft(text, len);
            }
            _ => {}
        }
    }

    pub(super) fn paste(&mut self, text: &str) {
        if let Focus::AutoSetup(setup) = &mut self.focus {
            setup.paste(text);
            return;
        }
        let text = if matches!(self.focus, Focus::AutoDraft(_)) {
            text.to_owned()
        } else {
            text.replace(['\r', '\n'], " ")
        };
        let editor = match &mut self.focus {
            Focus::Search(editor)
            | Focus::DirectoryForm(editor)
            | Focus::AutoGroupSearch { editor, .. } => Some(editor),
            Focus::AutoDraft(draft) => Some(&mut draft.editor),
            Focus::RuleForm(draft) if matches!(draft.field, 0 | 2) => Some(&mut draft.editor),
            _ => None,
        };
        if let Some(editor) = editor {
            editor.apply(EditorCommand::InsertPasted(text));
        }
        match &mut self.focus {
            Focus::Search(editor) => self.query = editor.draft().to_owned(),
            Focus::AutoGroupSearch { group, editor } => group.query = editor.draft().to_owned(),
            Focus::RuleForm(draft) => draft.commit_editor(),
            _ => {}
        }
    }

    fn matching_rule_indices(&self) -> Vec<usize> {
        let query = self.query.to_lowercase();
        self.view
            .rules
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                (Some(entry.rule.decision) == self.page.decision()
                    && (query.is_empty()
                        || format!(
                            "{} {:?} {:?} {:?}",
                            entry.rule.tool, entry.rule.specifier, entry.rule.input, entry.source
                        )
                        .to_lowercase()
                        .contains(&query)))
                .then_some(index)
            })
            .collect()
    }

    fn directory_rows(&self) -> Vec<PathBuf> {
        let query = self.query.to_lowercase();
        self.view
            .additional_directories
            .iter()
            .filter(|path| path.to_string_lossy().to_lowercase().contains(&query))
            .cloned()
            .collect()
    }

    fn row_count(&self) -> usize {
        if self.page == Page::Auto {
            return 6;
        }
        1 + if self.page == Page::Directories {
            self.directory_rows().len()
        } else {
            self.matching_rule_indices().len()
        }
    }

    fn clamp_selection(&mut self) {
        self.selected = self.selected.min(self.row_count().saturating_sub(1));
    }

    fn selected_change(&self) -> Option<PermissionChange> {
        if self.page == Page::Auto || self.selected == 0 {
            return None;
        }
        if self.page == Page::Directories {
            self.directory_rows()
                .get(self.selected - 1)
                .cloned()
                .map(|path| PermissionChange::RemoveDirectory { path })
        } else {
            self.matching_rule_indices()
                .get(self.selected - 1)
                .and_then(|index| self.view.rules.get(*index))
                .filter(|entry| entry.editable)
                .map(|entry| PermissionChange::RemoveRule { id: entry.rule.id })
        }
    }

    fn begin_add(&mut self) {
        self.message = None;
        if self.page == Page::Auto {
            self.focus = Focus::ReviewerChoice(0);
        } else if self.page == Page::Directories {
            self.focus = Focus::DirectoryForm(Editor::new());
        } else {
            self.focus = Focus::RuleForm(RuleDraft::new());
        }
    }

    fn begin_edit(&mut self) {
        self.message = None;
        if self.page == Page::Auto {
            self.open_auto_row();
            return;
        }
        if self.selected == 0 {
            self.begin_add();
            return;
        }
        if self.page == Page::Directories {
            self.message = Some("Remove and re-add a directory to change its path".into());
        } else if let Some(entry) = self
            .matching_rule_indices()
            .get(self.selected - 1)
            .and_then(|index| self.view.rules.get(*index))
        {
            if entry.editable {
                self.focus = Focus::RuleForm(RuleDraft::existing(&entry.rule));
            } else {
                self.message = Some("This constraint is read-only".into());
            }
        }
    }

    fn open_auto_row(&mut self) {
        match self.selected {
            0 => self.focus = Focus::ReviewerChoice(0),
            1 => {}
            2 => self.focus = Focus::AutoGroup(AutoGroupState::new(AutoRuleKind::SoftAllow)),
            3 => self.focus = Focus::AutoGroup(AutoGroupState::new(AutoRuleKind::SoftDeny)),
            4 => self.focus = Focus::AutoGroup(AutoGroupState::new(AutoRuleKind::HardDeny)),
            _ => {
                self.focus = Focus::AutoEnvironment {
                    selected: 0,
                    scroll: 0,
                }
            }
        }
    }

    fn auto_group_rules(&self, group: &AutoGroupState) -> Vec<tickloop_agent_protocol::AutoRule> {
        let query = group.query.to_lowercase();
        self.view
            .auto_policy
            .rules
            .iter()
            .filter(|rule| {
                rule.kind == group.kind
                    && (query.is_empty()
                        || format!("{} {}", rule.id, rule.text)
                            .to_lowercase()
                            .contains(&query))
            })
            .cloned()
            .collect()
    }

    fn preview_removed(&self, change: &PermissionChange) -> Vec<String> {
        let PermissionChange::ChangeAutoRules { change } = change else {
            return Vec::new();
        };
        match change {
            AutoRuleChange::ReplaceGroup { kind, .. } => self
                .view
                .auto_policy
                .rules
                .iter()
                .filter(|rule| rule.kind == *kind)
                .map(|rule| {
                    format!(
                        "{} · {} · {}",
                        rule.id,
                        auto_source_label(&rule.source),
                        rule.text
                    )
                })
                .collect(),
            AutoRuleChange::RestoreGroup { kind } => self
                .view
                .auto_rule_settings
                .group(*kind)
                .custom
                .iter()
                .map(|rule| format!("{} · User · {}", rule.id, rule.text))
                .collect(),
            AutoRuleChange::Remove { id } => self
                .view
                .auto_policy
                .rules
                .iter()
                .filter(|rule| &rule.id == id)
                .map(|rule| {
                    format!(
                        "{} · {} · {}",
                        rule.id,
                        auto_source_label(&rule.source),
                        rule.text
                    )
                })
                .collect(),
            AutoRuleChange::UpdateEnvironmentEntry { previous, .. } => vec![format!(
                "{} · scope {:?}: {}",
                previous.id, previous.directory, previous.text
            )],
            AutoRuleChange::RemoveEnvironmentEntries { entries } => entries
                .iter()
                .filter(|entry| {
                    self.view
                        .auto_rule_settings
                        .environment_entries
                        .contains(entry)
                })
                .map(|entry| format!("{} · {:?}: {}", entry.id, entry.source, entry.text))
                .collect(),
            AutoRuleChange::ReplaceEnvironment { .. } | AutoRuleChange::RestoreEnvironment => {
                vec![format!(
                    "{}: {}",
                    environment_source_label(&self.view.auto_policy.environment.source),
                    self.view.auto_policy.environment.text
                )]
            }
            _ => Vec::new(),
        }
    }

    fn preview(&mut self, change: PermissionChange, title: String) {
        let removed = self.preview_removed(&change);
        self.focus = Focus::AutoPreview {
            change,
            title,
            removed,
            scroll: 0,
        };
    }

    fn edit_auto_rule(&mut self, group: AutoGroupState, id: String) {
        let Some(rule) = self
            .view
            .auto_policy
            .rules
            .iter()
            .find(|rule| rule.id == id)
        else {
            self.message = Some("Rule changed elsewhere; refresh the group".into());
            self.focus = Focus::AutoGroup(group);
            return;
        };
        if rule.source != AutoRuleSource::User {
            self.message =
                Some("Built-in rules are read-only; choose Replace group to remove them".into());
            self.focus = Focus::AutoGroup(group);
            return;
        }
        self.focus = Focus::AutoDraft(AutoDraft::new(
            AutoDraftPurpose::Update {
                kind: group.kind,
                id,
            },
            rule.text.clone(),
        ));
    }

    fn auto_group_key(&mut self, mut group: AutoGroupState, key: KeyEvent) -> PermissionsEffect {
        let rules = self.auto_group_rules(&group);
        let replace_row = rules.len() + 1;
        let restore_row = rules.len() + 2;
        match (key.code, key.modifiers) {
            (KeyCode::Esc | KeyCode::Left, _) => return PermissionsEffect::None,
            (KeyCode::Up | KeyCode::Char('k'), KeyModifiers::NONE)
            | (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
                group.selected = group.selected.saturating_sub(1)
            }
            (KeyCode::Down | KeyCode::Char('j'), KeyModifiers::NONE)
            | (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
                group.selected = (group.selected + 1).min(restore_row)
            }
            (KeyCode::Home, _) => group.selected = 0,
            (KeyCode::End, _) => group.selected = restore_row,
            (KeyCode::PageUp, _) => group.selected = group.selected.saturating_sub(8),
            (KeyCode::PageDown, _) => group.selected = (group.selected + 8).min(restore_row),
            (KeyCode::Char('/'), KeyModifiers::NONE) => {
                let mut editor = Editor::new();
                editor.replace_draft(group.query.clone(), group.query.len());
                self.focus = Focus::AutoGroupSearch { group, editor };
                return PermissionsEffect::None;
            }
            (KeyCode::Char('a'), KeyModifiers::NONE) => {
                self.focus = Focus::AutoDraft(AutoDraft::new(
                    AutoDraftPurpose::Add(group.kind),
                    String::new(),
                ));
                return PermissionsEffect::None;
            }
            (KeyCode::Char('e'), KeyModifiers::NONE)
                if group.selected > 0 && group.selected < replace_row =>
            {
                let id = rules[group.selected - 1].id.clone();
                self.edit_auto_rule(group, id);
                return PermissionsEffect::None;
            }
            (KeyCode::Char('d'), KeyModifiers::NONE)
                if group.selected > 0 && group.selected < replace_row =>
            {
                let rule = &rules[group.selected - 1];
                if rule.source == AutoRuleSource::User {
                    self.preview(
                        PermissionChange::ChangeAutoRules {
                            change: AutoRuleChange::Remove {
                                id: rule.id.clone(),
                            },
                        },
                        format!("Remove {} rule", auto_kind_label(group.kind)),
                    );
                } else {
                    self.message =
                        Some("Built-in rules are read-only; choose Replace group".into());
                    self.focus = Focus::AutoGroup(group);
                }
                return PermissionsEffect::None;
            }
            (KeyCode::Enter, _) => {
                if group.selected == 0 {
                    self.focus = Focus::AutoDraft(AutoDraft::new(
                        AutoDraftPurpose::Add(group.kind),
                        String::new(),
                    ));
                } else if group.selected == replace_row {
                    self.focus = Focus::AutoDraft(AutoDraft::new(
                        AutoDraftPurpose::Replace(group.kind),
                        String::new(),
                    ));
                } else if group.selected == restore_row {
                    self.preview(
                        PermissionChange::ChangeAutoRules {
                            change: AutoRuleChange::RestoreGroup { kind: group.kind },
                        },
                        format!("Restore {} defaults", auto_kind_label(group.kind)),
                    );
                } else {
                    self.focus = Focus::AutoRuleDetail {
                        id: rules[group.selected - 1].id.clone(),
                        group,
                        scroll: 0,
                    };
                }
                return PermissionsEffect::None;
            }
            _ => {}
        }
        self.focus = Focus::AutoGroup(group);
        PermissionsEffect::None
    }

    fn selected_rule_editable(&self) -> bool {
        self.matching_rule_indices()
            .get(self.selected.saturating_sub(1))
            .and_then(|index| self.view.rules.get(*index))
            .is_some_and(|entry| entry.editable)
    }

    /// Routes `key`; a read-only surface never enters an editing form or emits a change.
    pub(super) fn key(&mut self, key: KeyEvent) -> PermissionsEffect {
        self.notice = None;
        let Some(reason) = self.read_only else {
            return self.route_key(key);
        };
        let typing = matches!(self.focus, Focus::Search(_) | Focus::AutoGroupSearch { .. });
        if !typing
            && self.page != Page::RecentlyDenied
            && key.modifiers.difference(KeyModifiers::SHIFT).is_empty()
            && matches!(key.code, KeyCode::Char('a' | 'e' | 'd'))
        {
            self.message = Some(format!("Read-only: {reason}"));
            return PermissionsEffect::None;
        }
        let effect = self.route_key(key);
        let editing = matches!(
            self.focus,
            Focus::RuleForm(_)
                | Focus::DirectoryForm(_)
                | Focus::ConfirmDelete(_)
                | Focus::ReviewerChoice(_)
                | Focus::AutoDraft(_)
                | Focus::AutoPreview { .. }
                | Focus::AutoSetup(_)
        );
        if editing
            || matches!(
                effect,
                PermissionsEffect::Apply(_)
                    | PermissionsEffect::OpenJudgmentSettings { .. }
                    | PermissionsEffect::GenerateEnvironment { .. }
            )
        {
            self.focus = Focus::List;
            self.message = Some(format!("Read-only: {reason}"));
            return PermissionsEffect::None;
        }
        effect
    }

    fn route_key(&mut self, key: KeyEvent) -> PermissionsEffect {
        if self.page == Page::RecentlyDenied {
            if !self.denials.is_detail() && matches!(key.code, KeyCode::Left | KeyCode::Right) {
                self.switch_page(if key.code == KeyCode::Left { -1 } else { 1 });
                return PermissionsEffect::None;
            }
            return match self.denials.handle(key) {
                DenialsEffect::None => PermissionsEffect::None,
                DenialsEffect::Back => PermissionsEffect::Close {
                    destination: self.destination,
                },
                DenialsEffect::Refresh => PermissionsEffect::RefreshDenials,
                DenialsEffect::Retry(id) => PermissionsEffect::RetryDenial(id),
            };
        }
        let focus = std::mem::replace(&mut self.focus, Focus::List);
        match focus {
            Focus::AutoSetup(mut setup) => {
                use super::auto_setup_surface::SetupEffect;
                let effect = setup.key(key);
                self.focus = Focus::AutoSetup(setup);
                match effect {
                    SetupEffect::None => PermissionsEffect::None,
                    SetupEffect::Back => {
                        self.focus = Focus::AutoEnvironment {
                            selected: 2,
                            scroll: 0,
                        };
                        PermissionsEffect::None
                    }
                    SetupEffect::Generate(generation, options) => {
                        PermissionsEffect::GenerateEnvironment {
                            generation,
                            options,
                        }
                    }
                    SetupEffect::Preview(entries) => {
                        if entries.is_empty() {
                            self.message = Some("No entries selected; nothing applied".into());
                        } else {
                            self.preview(
                                PermissionChange::ChangeAutoRules {
                                    change: AutoRuleChange::AddEnvironmentEntries { entries },
                                },
                                "Confirm Environment additions".into(),
                            );
                        }
                        PermissionsEffect::None
                    }
                }
            }
            Focus::List => self.list_key(key),
            Focus::Search(mut editor) => {
                if key.code == KeyCode::Esc {
                    self.query = editor.draft().to_owned();
                    self.clamp_selection();
                    return PermissionsEffect::None;
                }
                if key.code == KeyCode::Enter {
                    self.query = editor.draft().to_owned();
                    self.clamp_selection();
                    return PermissionsEffect::None;
                }
                edit_text(&mut editor, key);
                self.query = editor.draft().to_owned();
                self.selected = 0;
                self.focus = Focus::Search(editor);
                PermissionsEffect::None
            }
            Focus::Detail => {
                if matches!(key.code, KeyCode::Esc | KeyCode::Left | KeyCode::Enter) {
                    self.focus = Focus::List;
                } else if matches!(key.code, KeyCode::Down | KeyCode::Char('j')) {
                    self.detail_scroll = self.detail_scroll.saturating_add(1);
                    self.focus = Focus::Detail;
                } else if matches!(key.code, KeyCode::Up | KeyCode::Char('k')) {
                    self.detail_scroll = self.detail_scroll.saturating_sub(1);
                    self.focus = Focus::Detail;
                } else if key.code == KeyCode::PageDown {
                    self.detail_scroll = self.detail_scroll.saturating_add(8);
                    self.focus = Focus::Detail;
                } else if key.code == KeyCode::PageUp {
                    self.detail_scroll = self.detail_scroll.saturating_sub(8);
                    self.focus = Focus::Detail;
                } else if key.code == KeyCode::Char('e') {
                    self.begin_edit();
                } else {
                    self.focus = Focus::Detail;
                }
                PermissionsEffect::None
            }
            Focus::RuleForm(mut draft) => {
                if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
                    let change =
                        draft.change(self.page.decision().unwrap(), self.view.selected_loop_id);
                    return match change {
                        Ok(change) => {
                            self.focus = Focus::RuleForm(draft);
                            PermissionsEffect::Apply(change)
                        }
                        Err(error) => {
                            self.message = Some(error);
                            self.focus = Focus::RuleForm(draft);
                            PermissionsEffect::None
                        }
                    };
                }
                if key.code == KeyCode::Esc {
                    self.message = Some("No change".into());
                    self.focus = Focus::List;
                    return PermissionsEffect::None;
                }
                if key.code == KeyCode::Tab {
                    draft.set_field((draft.field + 1) % 5);
                } else if key.code == KeyCode::BackTab {
                    draft.set_field((draft.field + 4) % 5);
                } else if key.code == KeyCode::Enter {
                    match draft.field {
                        1 => draft.matcher = draft.matcher.next(),
                        3 => draft.cycle_scope(self.view.selected_loop_id),
                        4 => {
                            let change = draft
                                .change(self.page.decision().unwrap(), self.view.selected_loop_id);
                            return match change {
                                Ok(change) => {
                                    self.focus = Focus::RuleForm(draft);
                                    PermissionsEffect::Apply(change)
                                }
                                Err(error) => {
                                    self.message = Some(error);
                                    self.focus = Focus::RuleForm(draft);
                                    PermissionsEffect::None
                                }
                            };
                        }
                        _ => draft.set_field(draft.field + 1),
                    }
                } else if matches!(draft.field, 0 | 2) {
                    edit_text(&mut draft.editor, key);
                    draft.commit_editor();
                } else if matches!(
                    key.code,
                    KeyCode::Left | KeyCode::Right | KeyCode::Char(' ')
                ) {
                    if draft.field == 1 {
                        draft.matcher = draft.matcher.next();
                    }
                    if draft.field == 3 {
                        draft.cycle_scope(self.view.selected_loop_id);
                    }
                }
                self.focus = Focus::RuleForm(draft);
                PermissionsEffect::None
            }
            Focus::DirectoryForm(mut editor) => {
                if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s')
                    || key.code == KeyCode::Enter
                {
                    let path = editor.draft().trim();
                    if path.is_empty() {
                        self.message = Some("Directory path is required".into());
                        self.focus = Focus::DirectoryForm(editor);
                        return PermissionsEffect::None;
                    }
                    let effect = PermissionsEffect::Apply(PermissionChange::AddDirectory {
                        path: PathBuf::from(path),
                    });
                    self.focus = Focus::DirectoryForm(editor);
                    return effect;
                }
                if key.code == KeyCode::Esc {
                    self.message = Some("No change".into());
                    self.focus = Focus::List;
                    return PermissionsEffect::None;
                }
                edit_text(&mut editor, key);
                self.focus = Focus::DirectoryForm(editor);
                PermissionsEffect::None
            }
            Focus::ConfirmDelete(change) => match key.code {
                KeyCode::Enter | KeyCode::Char('y') => {
                    self.focus = Focus::ConfirmDelete(change.clone());
                    PermissionsEffect::Apply(change)
                }
                KeyCode::Esc | KeyCode::Char('n') => {
                    self.message = Some("No change".into());
                    PermissionsEffect::None
                }
                _ => {
                    self.focus = Focus::ConfirmDelete(change);
                    PermissionsEffect::None
                }
            },
            Focus::ReviewerChoice(mut selected) => {
                match (key.code, key.modifiers) {
                    (KeyCode::Up | KeyCode::Char('k'), KeyModifiers::NONE)
                    | (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
                        selected = selected.saturating_sub(1)
                    }
                    (KeyCode::Down | KeyCode::Char('j'), KeyModifiers::NONE)
                    | (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
                        selected = (selected + 1).min(1)
                    }
                    (KeyCode::Esc | KeyCode::Left, _) => return PermissionsEffect::None,
                    (KeyCode::Enter, _) if selected == 0 => {
                        if self.view.preferred_reviewer == AutoReviewerPreference::Llm {
                            self.message = Some("LLM is already the preferred reviewer".into());
                            return PermissionsEffect::None;
                        }
                        self.focus = Focus::ReviewerChoice(selected);
                        return PermissionsEffect::Apply(PermissionChange::SelectReviewer {
                            reviewer: AutoReviewerPreference::Llm,
                        });
                    }
                    (KeyCode::Enter, _) => {
                        if self.view.jev_key_status == ApiKeyStatus::Missing {
                            self.focus = Focus::ReviewerChoice(selected);
                            return PermissionsEffect::OpenJudgmentSettings {
                                select_reviewer: self.view.preferred_reviewer
                                    != AutoReviewerPreference::Jev,
                            };
                        }
                        if self.view.preferred_reviewer == AutoReviewerPreference::Jev {
                            self.message =
                                Some("Judgment is already the preferred reviewer".into());
                            return PermissionsEffect::None;
                        }
                        self.focus = Focus::ReviewerChoice(selected);
                        return PermissionsEffect::Apply(PermissionChange::SelectReviewer {
                            reviewer: AutoReviewerPreference::Jev,
                        });
                    }
                    _ => {}
                }
                self.focus = Focus::ReviewerChoice(selected);
                PermissionsEffect::None
            }
            Focus::AutoGroup(group) => self.auto_group_key(group, key),
            Focus::AutoGroupSearch {
                mut group,
                mut editor,
            } => {
                if matches!(key.code, KeyCode::Enter | KeyCode::Esc) {
                    group.query = editor.draft().to_owned();
                    group.selected = 0;
                    self.focus = Focus::AutoGroup(group);
                    return PermissionsEffect::None;
                }
                edit_text(&mut editor, key);
                group.query = editor.draft().to_owned();
                group.selected = 0;
                self.focus = Focus::AutoGroupSearch { group, editor };
                PermissionsEffect::None
            }
            Focus::AutoRuleDetail {
                group,
                id,
                mut scroll,
            } => {
                match (key.code, key.modifiers) {
                    (KeyCode::Esc | KeyCode::Left | KeyCode::Enter, _) => {
                        self.focus = Focus::AutoGroup(group)
                    }
                    (KeyCode::Up | KeyCode::Char('k'), KeyModifiers::NONE) => {
                        scroll = scroll.saturating_sub(1);
                        self.focus = Focus::AutoRuleDetail { group, id, scroll };
                    }
                    (KeyCode::Down | KeyCode::Char('j'), KeyModifiers::NONE) => {
                        scroll += 1;
                        self.focus = Focus::AutoRuleDetail { group, id, scroll };
                    }
                    (KeyCode::PageUp, _) => {
                        scroll = scroll.saturating_sub(8);
                        self.focus = Focus::AutoRuleDetail { group, id, scroll };
                    }
                    (KeyCode::PageDown, _) => {
                        scroll += 8;
                        self.focus = Focus::AutoRuleDetail { group, id, scroll };
                    }
                    (KeyCode::Char('e'), KeyModifiers::NONE) => self.edit_auto_rule(group, id),
                    (KeyCode::Char('d'), KeyModifiers::NONE) => {
                        if self
                            .view
                            .auto_policy
                            .rules
                            .iter()
                            .any(|rule| rule.id == id && rule.source == AutoRuleSource::User)
                        {
                            self.preview(
                                PermissionChange::ChangeAutoRules {
                                    change: AutoRuleChange::Remove { id },
                                },
                                "Remove Auto rule".into(),
                            );
                        } else {
                            self.message =
                                Some("Built-in rules are read-only; choose Replace group".into());
                            self.focus = Focus::AutoGroup(group);
                        }
                    }
                    _ => self.focus = Focus::AutoRuleDetail { group, id, scroll },
                }
                PermissionsEffect::None
            }
            Focus::AutoEnvironment {
                mut selected,
                mut scroll,
            } => {
                match (key.code, key.modifiers) {
                    (KeyCode::Esc | KeyCode::Left, _) => return PermissionsEffect::None,
                    (KeyCode::Up | KeyCode::Char('k'), KeyModifiers::NONE) => {
                        selected = selected.saturating_sub(1)
                    }
                    (KeyCode::Down | KeyCode::Char('j'), KeyModifiers::NONE) => {
                        selected = (selected + 1)
                            .min(3 + self.view.auto_rule_settings.environment_entries.len())
                    }
                    (KeyCode::Char('d'), KeyModifiers::NONE) if selected >= 4 => {
                        let entry =
                            self.view.auto_rule_settings.environment_entries[selected - 4].clone();
                        self.preview(
                            PermissionChange::ChangeAutoRules {
                                change: AutoRuleChange::RemoveEnvironmentEntries {
                                    entries: vec![entry],
                                },
                            },
                            "Remove Environment entry".into(),
                        );
                        return PermissionsEffect::None;
                    }
                    (KeyCode::Char('s' | 'r'), KeyModifiers::NONE) if selected >= 4 => {
                        let entry =
                            self.view.auto_rule_settings.environment_entries[selected - 4].clone();
                        let (directory, include_descendants) = if key.code == KeyCode::Char('s') {
                            (
                                if entry.directory.is_some() {
                                    None
                                } else {
                                    Some(self.view.working_directory.clone())
                                },
                                false,
                            )
                        } else {
                            (
                                entry.directory.clone(),
                                entry.directory.is_some() && !entry.include_descendants,
                            )
                        };
                        self.preview(
                            PermissionChange::ChangeAutoRules {
                                change: AutoRuleChange::UpdateEnvironmentEntry {
                                    text: entry.text.clone(),
                                    previous: entry,
                                    directory,
                                    include_descendants,
                                },
                            },
                            "Change Environment scope".into(),
                        );
                        return PermissionsEffect::None;
                    }
                    (KeyCode::Enter, _) if selected >= 4 => {
                        let entry =
                            self.view.auto_rule_settings.environment_entries[selected - 4].clone();
                        self.focus = Focus::AutoDraft(AutoDraft::new(
                            AutoDraftPurpose::EnvironmentEntry(entry.clone()),
                            entry.text,
                        ));
                        return PermissionsEffect::None;
                    }
                    (KeyCode::PageUp, _) => scroll = scroll.saturating_sub(8),
                    (KeyCode::PageDown, _) => scroll += 8,
                    (KeyCode::Enter, _) if selected == 0 => {
                        self.focus = Focus::AutoDraft(AutoDraft::new(
                            AutoDraftPurpose::Environment,
                            self.view.auto_policy.environment.text.clone(),
                        ));
                        return PermissionsEffect::None;
                    }
                    (KeyCode::Enter, _) if selected == 2 => {
                        self.open_environment_setup();
                        return PermissionsEffect::None;
                    }
                    (KeyCode::Enter, _) if selected == 3 => {
                        let latest_batch = self.view.auto_rule_settings.environment_entries.iter().filter_map(|entry| match entry.source { tickloop_agent_protocol::AutoEnvironmentEntrySource::Generated { batch, generated_at_ms, .. } => Some((generated_at_ms, batch)), _ => None }).max().map(|(_, batch)| batch);
                        let entries = self.view.auto_rule_settings.environment_entries.iter().filter(|entry| entry.changes.is_empty() && matches!(entry.source, tickloop_agent_protocol::AutoEnvironmentEntrySource::Generated { batch, .. } if Some(batch) == latest_batch)).cloned().collect();
                        self.preview(
                            PermissionChange::ChangeAutoRules {
                                change: AutoRuleChange::RemoveEnvironmentEntries { entries },
                            },
                            "Undo latest setup additions".into(),
                        );
                        return PermissionsEffect::None;
                    }
                    (KeyCode::Enter, _) => {
                        self.preview(
                            PermissionChange::ChangeAutoRules {
                                change: AutoRuleChange::RestoreEnvironment,
                            },
                            "Restore Environment default".into(),
                        );
                        return PermissionsEffect::None;
                    }
                    _ => {}
                }
                self.focus = Focus::AutoEnvironment { selected, scroll };
                PermissionsEffect::None
            }
            Focus::AutoDraft(mut draft) => {
                if key.code == KeyCode::Enter
                    || key.modifiers.contains(KeyModifiers::CONTROL)
                        && key.code == KeyCode::Char('s')
                {
                    match draft.change() {
                        Ok(change) => {
                            let preview = matches!(
                                draft.purpose,
                                AutoDraftPurpose::Replace(_)
                                    | AutoDraftPurpose::Environment
                                    | AutoDraftPurpose::EnvironmentEntry(_)
                            );
                            if preview {
                                let title = draft.purpose.title().to_owned();
                                self.preview(change, title);
                                return PermissionsEffect::None;
                            }
                            self.focus = Focus::AutoDraft(draft);
                            return PermissionsEffect::Apply(change);
                        }
                        Err(error) => self.message = Some(error),
                    }
                } else if key.code == KeyCode::Esc {
                    self.message = Some("No change".into());
                    self.focus = match draft.purpose {
                        AutoDraftPurpose::Environment | AutoDraftPurpose::EnvironmentEntry(_) => {
                            Focus::AutoEnvironment {
                                selected: 0,
                                scroll: 0,
                            }
                        }
                        AutoDraftPurpose::Add(kind)
                        | AutoDraftPurpose::Replace(kind)
                        | AutoDraftPurpose::Update { kind, .. } => {
                            Focus::AutoGroup(AutoGroupState::new(kind))
                        }
                    };
                    return PermissionsEffect::None;
                } else {
                    edit_text(&mut draft.editor, key);
                }
                self.focus = Focus::AutoDraft(draft);
                PermissionsEffect::None
            }
            Focus::AutoPreview {
                change,
                title,
                removed,
                mut scroll,
            } => match (key.code, key.modifiers) {
                (KeyCode::Enter | KeyCode::Char('y'), _) => {
                    self.focus = Focus::AutoPreview {
                        change: change.clone(),
                        title,
                        removed,
                        scroll,
                    };
                    PermissionsEffect::Apply(change)
                }
                (KeyCode::Esc | KeyCode::Char('n'), _) => {
                    self.message = Some("No change".into());
                    self.focus = match &change {
                        PermissionChange::ChangeAutoRules {
                            change:
                                AutoRuleChange::ReplaceEnvironment { .. }
                                | AutoRuleChange::RestoreEnvironment
                                | AutoRuleChange::MigrateEnvironment { .. }
                                | AutoRuleChange::AddEnvironmentEntries { .. }
                                | AutoRuleChange::RemoveEnvironmentEntries { .. }
                                | AutoRuleChange::UpdateEnvironmentEntry { .. },
                        } => Focus::AutoEnvironment {
                            selected: 0,
                            scroll: 0,
                        },
                        PermissionChange::ChangeAutoRules {
                            change:
                                AutoRuleChange::ReplaceGroup { kind, .. }
                                | AutoRuleChange::RestoreGroup { kind },
                        } => Focus::AutoGroup(AutoGroupState::new(*kind)),
                        PermissionChange::ChangeAutoRules {
                            change: AutoRuleChange::Remove { id },
                        } => self
                            .view
                            .auto_policy
                            .rules
                            .iter()
                            .find(|rule| &rule.id == id)
                            .map(|rule| Focus::AutoGroup(AutoGroupState::new(rule.kind)))
                            .unwrap_or(Focus::List),
                        _ => Focus::List,
                    };
                    PermissionsEffect::None
                }
                (KeyCode::Up | KeyCode::Char('k'), KeyModifiers::NONE) => {
                    scroll = scroll.saturating_sub(1);
                    self.focus = Focus::AutoPreview {
                        change,
                        title,
                        removed,
                        scroll,
                    };
                    PermissionsEffect::None
                }
                (KeyCode::Down | KeyCode::Char('j'), KeyModifiers::NONE) => {
                    scroll += 1;
                    self.focus = Focus::AutoPreview {
                        change,
                        title,
                        removed,
                        scroll,
                    };
                    PermissionsEffect::None
                }
                _ => {
                    self.focus = Focus::AutoPreview {
                        change,
                        title,
                        removed,
                        scroll,
                    };
                    PermissionsEffect::None
                }
            },
        }
    }

    fn list_key(&mut self, key: KeyEvent) -> PermissionsEffect {
        match (key.code, key.modifiers) {
            (KeyCode::Left, _) => self.switch_page(-1),
            (KeyCode::Right, _) => self.switch_page(1),
            (KeyCode::Up | KeyCode::Char('k'), KeyModifiers::NONE)
            | (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
                self.selected = self.selected.saturating_sub(1)
            }
            (KeyCode::Down | KeyCode::Char('j'), KeyModifiers::NONE)
            | (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
                self.selected = (self.selected + 1).min(self.row_count().saturating_sub(1))
            }
            (KeyCode::PageUp, _) => self.selected = self.selected.saturating_sub(8),
            (KeyCode::PageDown, _) => {
                self.selected = (self.selected + 8).min(self.row_count().saturating_sub(1))
            }
            (KeyCode::Home, _) => self.selected = 0,
            (KeyCode::End, _) => self.selected = self.row_count().saturating_sub(1),
            (KeyCode::Char('/'), KeyModifiers::NONE) => {
                if self.page == Page::Auto {
                    return PermissionsEffect::None;
                }
                let mut editor = Editor::new();
                editor.replace_draft(self.query.clone(), self.query.len());
                self.focus = Focus::Search(editor);
            }
            (KeyCode::Char('a'), KeyModifiers::NONE) => self.begin_add(),
            (KeyCode::Char('e'), KeyModifiers::NONE) => {
                if self.page == Page::Auto && self.selected == 1 {
                    return PermissionsEffect::OpenJudgmentSettings {
                        select_reviewer: false,
                    };
                }
                self.begin_edit();
            }
            (KeyCode::Char('d'), KeyModifiers::NONE) => {
                if let Some(change) = self.selected_change() {
                    self.focus = Focus::ConfirmDelete(change);
                }
            }
            (KeyCode::Enter, _) => {
                if self.page == Page::Auto {
                    if self.selected == 1 {
                        return PermissionsEffect::OpenJudgmentSettings {
                            select_reviewer: false,
                        };
                    }
                    self.open_auto_row();
                } else if self.selected == 0 {
                    self.begin_add();
                } else {
                    self.detail_scroll = 0;
                    self.focus = Focus::Detail;
                }
            }
            (KeyCode::Esc, _) => {
                return PermissionsEffect::Close {
                    destination: self.destination,
                };
            }
            _ => {}
        }
        if self.page == Page::RecentlyDenied {
            PermissionsEffect::RefreshDenials
        } else {
            PermissionsEffect::None
        }
    }

    fn switch_page(&mut self, offset: isize) {
        let pages = Page::visible_for(self.view.execution_mode);
        let index = pages.iter().position(|page| *page == self.page).unwrap();
        let next = (index as isize + offset).rem_euclid(pages.len() as isize) as usize;
        self.page = pages[next];
        self.selected = 0;
        self.scroll = 0;
    }

    fn overview_content(&self, width: usize) -> ListContent {
        let mut body = Vec::new();
        if self.page == Page::Auto {
            body.push(labelled_row("Mode  ", self.view.execution_mode.to_string()));
            let status = match self.view.preferred_reviewer {
                AutoReviewerPreference::Llm => "LLM reviews with this Chat model".to_owned(),
                AutoReviewerPreference::Jev => match self.view.auto_reviewer_health {
                    Some(AutoReviewerHealth::Ready) => {
                        "Jev ready · LLM fallback on service failure".into()
                    }
                    Some(AutoReviewerHealth::CoolingDown { remaining_seconds }) => {
                        format!("Jev cooling down {remaining_seconds}s · LLM reviewing")
                    }
                    Some(AutoReviewerHealth::Probing) => {
                        "Jev recovery probe · LLM reviews concurrently".into()
                    }
                    None => "Jev selected · its status appears once the Agent runs".into(),
                },
            };
            body.push(muted_row(status));
            body.push(RoleLine::default());
            let mut rows = Vec::new();
            let reviewer = match self.view.preferred_reviewer {
                AutoReviewerPreference::Llm => {
                    format!("LLM · {}", self.view.chat_model_label)
                }
                AutoReviewerPreference::Jev => "Judgment · Jev (TypeSafe)".into(),
            };
            rows.push(labelled_row("Reviewer     ", reviewer));
            rows.push(labelled_row(
                "Jev (TypeSafe) key  ",
                key_status_label(self.view.jev_key_status),
            ));
            for kind in [
                AutoRuleKind::SoftAllow,
                AutoRuleKind::SoftDeny,
                AutoRuleKind::HardDeny,
            ] {
                let count = self
                    .view
                    .auto_policy
                    .rules
                    .iter()
                    .filter(|rule| rule.kind == kind)
                    .count();
                rows.push(labelled_row(
                    format!("{:<13}", auto_kind_label(kind)).as_str(),
                    format!("{count} effective"),
                ));
            }
            rows.push(labelled_row(
                "Environment  ",
                environment_source_label(&self.view.auto_policy.environment.source),
            ));
            let footer = "Enter configure · Esc back";
            return ListContent {
                intro: body,
                rows,
                hint: footer,
            };
        }

        let search = match &self.focus {
            Focus::Search(editor) => editor_window(editor, width.saturating_sub(18)),
            _ if self.query.is_empty() => "(none)".into(),
            _ => self.query.clone(),
        };
        body.push(labelled_row("Mode  ", self.view.execution_mode.to_string()));
        if self.page == Page::Directories {
            body.push(labelled_row(
                "Working directory  ",
                self.view.working_directory.display().to_string(),
            ));
        } else {
            body.push(labelled_row("Search  ", search));
            if self.page == Page::Ask {
                body.push(muted_row("Matching operations require a human answer"));
            } else if self.page == Page::Deny {
                body.push(muted_row("Matching operations are blocked"));
            } else if self.view.execution_mode == ExecutionMode::Auto {
                body.push(muted_row(
                    "Broad shell/code Allow does not bypass Auto review",
                ));
            }
        }
        body.push(RoleLine::default());
        let mut rows = vec![RoleLine::role(
            format!(
                "+ Add {}",
                if self.page == Page::Directories {
                    "directory".into()
                } else {
                    format!("{} rule", self.page.label())
                }
            ),
            Role::Permission,
        )];
        if self.page == Page::Directories {
            for path in self.directory_rows() {
                let mut row = plain_row(path.display().to_string());
                row.push(" · additional", Some(Role::MutedText));
                rows.push(row);
            }
        } else {
            for entry in self
                .matching_rule_indices()
                .iter()
                .filter_map(|index| self.view.rules.get(*index))
            {
                let rule = &entry.rule;
                let matcher = rule
                    .specifier
                    .as_ref()
                    .map(|content| {
                        if rule.tool == WEB_FETCH_TOOL_NAME {
                            content.clone()
                        } else {
                            format!("shell: {content}")
                        }
                    })
                    .or_else(|| rule.input.as_ref().map(|input| format!("exact: {input}")))
                    .unwrap_or_else(|| "whole tool".into());
                let mut row = RoleLine::role(&rule.tool, Role::Strong);
                row.push(format!(" · {matcher}"), Some(Role::Text));
                row.push(
                    format!(
                        " · {} · {}{}",
                        scope_label(&entry.scope),
                        source_label(entry.source),
                        if entry.editable { "" } else { " · read-only" }
                    ),
                    Some(Role::MutedText),
                );
                rows.push(row);
            }
        }
        let hint = if matches!(self.focus, Focus::Search(_)) {
            "Search · Enter apply · Esc back"
        } else {
            "Enter details · a add · / search · Esc back"
        };
        ListContent {
            intro: body,
            rows,
            hint,
        }
    }

    fn styled_overview(
        &mut self,
        width: usize,
        height: usize,
        body: &mut Vec<RoleLine>,
    ) -> &'static str {
        let content = self.overview_content(width);
        body.extend(content.intro);
        self.push_selected_rows(content.rows, width, height, body);
        content.hint
    }

    fn push_selected_rows(
        &mut self,
        rows: Vec<RoleLine>,
        width: usize,
        height: usize,
        body: &mut Vec<RoleLine>,
    ) {
        let reserved = 2 + usize::from(self.has_status_row());
        let available = height.saturating_sub(body.len() + reserved).max(1);
        if self.selected < self.scroll {
            self.scroll = self.selected;
        }
        if self.selected >= self.scroll + available {
            self.scroll = self.selected + 1 - available;
        }
        for (index, row) in rows
            .into_iter()
            .enumerate()
            .skip(self.scroll)
            .take(available)
        {
            let mut selected = selected_row("", index == self.selected);
            selected.spans.extend(row.spans);
            selected.truncate(width);
            body.push(selected);
        }
    }

    fn styled_detail(&self, width: usize, height: usize, body: &mut Vec<RoleLine>) -> &'static str {
        match &self.focus {
            Focus::AutoSetup(setup) => {
                body.push(plain_row(format!(
                    "Directory: {}",
                    self.view.working_directory.display()
                )));
                body.extend(setup.lines(width));
                "j/k select · Enter toggle/continue · e edit · s scope · r descendants · Ctrl+G editor · Esc back"
            }
            Focus::Detail => {
                if self.page == Page::Directories {
                    if let Some(path) = self.directory_rows().get(self.selected.saturating_sub(1)) {
                        body.push(RoleLine::role("Additional directory", Role::DialogTitle));
                        body.push(plain_row(path.display().to_string()));
                        body.push(muted_row("Canonical path checked by Agent"));
                    }
                } else if let Some(entry) = self
                    .matching_rule_indices()
                    .get(self.selected.saturating_sub(1))
                    .and_then(|index| self.view.rules.get(*index))
                {
                    let rule = &entry.rule;
                    body.push(RoleLine::role(&rule.tool, Role::DialogTitle));
                    body.push(RoleLine::default());
                    body.push(labelled_row("Decision  ", format!("{:?}", rule.decision)));
                    body.push(labelled_row(
                        "Matcher   ",
                        rule.specifier
                            .as_ref()
                            .map(|pattern| format!("Shell pattern {pattern}"))
                            .or_else(|| {
                                rule.input
                                    .as_ref()
                                    .map(|input| format!("Exact input {input}"))
                            })
                            .unwrap_or_else(|| "Whole tool".into()),
                    ));
                    body.push(labelled_row("Scope     ", scope_label(&entry.scope)));
                    body.push(labelled_row("Source    ", source_label(entry.source)));
                    if let Some(note) = &entry.effective_note {
                        body.push(muted_row(note));
                    }
                }
                if self.selected_rule_editable() {
                    "e edit · s scope · r descendants · Ctrl+G editor · Esc back"
                } else {
                    "Esc back"
                }
            }
            Focus::RuleForm(draft) => {
                body.push(RoleLine::role(
                    format!(
                        "{} {} rule",
                        if draft.original_id.is_some() {
                            "Edit"
                        } else {
                            "Add"
                        },
                        self.page.label()
                    ),
                    Role::DialogTitle,
                ));
                body.push(RoleLine::default());
                let tool = if draft.field == 0 {
                    editor_window(&draft.editor, width.saturating_sub(9))
                } else {
                    draft.tool.clone()
                };
                let matching = if draft.field == 2 {
                    editor_window(&draft.editor, width.saturating_sub(10))
                } else {
                    draft.match_text.clone()
                };
                body.push(selected_row(format!("Tool      {tool}"), draft.field == 0));
                body.push(selected_row(
                    format!("Matcher   {}", draft.matcher.label()),
                    draft.field == 1,
                ));
                if draft.matcher != Matcher::WholeTool {
                    body.push(selected_row(
                        format!("Match     {matching}"),
                        draft.field == 2,
                    ));
                }
                body.push(selected_row(
                    format!("Scope     {}", draft.scope.label()),
                    draft.field == 3,
                ));
                body.push(selected_row("Save", draft.field == 4));
                "Tab next · Enter next · Ctrl+S save · Esc back"
            }
            Focus::DirectoryForm(editor) => {
                body.push(RoleLine::role(
                    "Add additional directory",
                    Role::DialogTitle,
                ));
                body.push(RoleLine::default());
                body.push(labelled_row(
                    "Path  ",
                    editor_window(editor, width.saturating_sub(7)),
                ));
                "Enter save · Esc back"
            }
            Focus::ConfirmDelete(_) => {
                body.push(RoleLine::role(
                    "Remove this permission entry?",
                    Role::DialogTitle,
                ));
                body.push(muted_row("This removes the selected rule or directory."));
                "Enter/y remove · Esc/n cancel"
            }
            Focus::ReviewerChoice(selected) => {
                body.push(RoleLine::role("Choose Auto reviewer", Role::DialogTitle));
                body.push(muted_row(format!(
                    "Execution mode: {}",
                    self.view.execution_mode
                )));
                body.push(RoleLine::default());
                body.push(selected_row(
                    format!(
                        "LLM{}",
                        if self.view.preferred_reviewer == AutoReviewerPreference::Llm {
                            "  ● Current"
                        } else {
                            ""
                        }
                    ),
                    *selected == 0,
                ));
                body.push(muted_row(format!(
                    "    This Chat model · {}",
                    self.view.chat_model_label
                )));
                body.push(RoleLine::default());
                body.push(selected_row(
                    format!(
                        "Judgment{}",
                        if self.view.preferred_reviewer == AutoReviewerPreference::Jev {
                            "  ● Current"
                        } else {
                            ""
                        }
                    ),
                    *selected == 1,
                ));
                body.push(muted_row(format!(
                    "    Jev (TypeSafe) · key {}",
                    key_status_label(self.view.jev_key_status)
                )));
                "Enter select · Esc cancel"
            }
            Focus::AutoGroup(group) | Focus::AutoGroupSearch { group, .. } => {
                self.styled_auto_group(width, height, group, body)
            }
            Focus::AutoRuleDetail { id, .. } => {
                if let Some(rule) = self
                    .view
                    .auto_policy
                    .rules
                    .iter()
                    .find(|rule| &rule.id == id)
                {
                    body.push(RoleLine::role(
                        auto_kind_label(rule.kind),
                        Role::DialogTitle,
                    ));
                    body.push(RoleLine::default());
                    body.push(plain_row(&rule.text));
                    body.push(RoleLine::default());
                    body.push(labelled_row("Source  ", auto_source_label(&rule.source)));
                    body.push(labelled_row("ID      ", &rule.id));
                    if rule.source == AutoRuleSource::User {
                        "e edit · d remove · Esc back"
                    } else {
                        "Built-in read-only · Esc back"
                    }
                } else {
                    body.push(muted_row("Rule changed elsewhere"));
                    "Esc back"
                }
            }
            Focus::AutoEnvironment { selected, .. } => {
                body.push(RoleLine::role("Environment", Role::DialogTitle));
                body.push(muted_row(format!(
                    "{} · context only, no new permission",
                    environment_source_label(&self.view.auto_policy.environment.source)
                )));
                body.push(RoleLine::default());
                body.push(selected_row("Replace Environment", *selected == 0));
                body.push(selected_row("Restore built-in Environment", *selected == 1));
                body.push(selected_row("Generate Environment draft", *selected == 2));
                body.push(selected_row("Undo latest setup additions", *selected == 3));
                for (index, entry) in self
                    .view
                    .auto_rule_settings
                    .environment_entries
                    .iter()
                    .enumerate()
                {
                    body.push(selected_row(
                        format!(
                            "{} · {}",
                            entry
                                .directory
                                .as_ref()
                                .map_or_else(|| "Global".into(), |path| path.display().to_string()),
                            entry.text
                        ),
                        *selected == index + 4,
                    ));
                    if *selected == index + 4 {
                        body.push(muted_row(format!(
                            "Origin: {} · confirmed {} · {} recent manual changes",
                            entry.source,
                            entry.confirmed_at_ms,
                            entry.changes.len()
                        )));
                        for change in &entry.changes {
                            body.push(muted_row(format!(
                                "Changed at {}: previous scope {:?}, previous text {}",
                                change.changed_at_ms,
                                change.previous_directory,
                                change.previous_text
                            )));
                        }
                    }
                }
                body.push(RoleLine::default());
                body.extend(
                    self.view
                        .auto_policy
                        .environment
                        .text
                        .lines()
                        .map(plain_row),
                );
                "Enter edit · s scope · r descendants · d remove · Esc back"
            }
            Focus::AutoDraft(draft) => {
                body.push(RoleLine::role(draft.purpose.title(), Role::DialogTitle));
                if matches!(draft.purpose, AutoDraftPurpose::Replace(_)) {
                    body.push(muted_row(
                        "One rule per line · empty replacement removes the group",
                    ));
                }
                if matches!(draft.purpose, AutoDraftPurpose::Environment) {
                    body.push(muted_row("Context only; grants no permission"));
                }
                body.push(RoleLine::default());
                body.push(labelled_row(
                    "Text  ",
                    editor_window(&draft.editor, width.saturating_sub(8)),
                ));
                "Enter/Ctrl+S save · Esc cancel"
            }
            Focus::AutoPreview {
                title,
                removed,
                change,
                ..
            } => {
                if let PermissionChange::ChangeAutoRules {
                    change:
                        AutoRuleChange::UpdateEnvironmentEntry {
                            text,
                            directory,
                            include_descendants,
                            ..
                        },
                } = change
                {
                    body.push(plain_row(format!(
                        "+ {text}\nScope: {}{}",
                        directory
                            .as_ref()
                            .map_or_else(|| "Global".into(), |path| path.display().to_string()),
                        if *include_descendants {
                            " and descendants"
                        } else {
                            ""
                        }
                    )));
                    body.push(muted_row(
                        "This manual change will be recorded in the entry's recent history.",
                    ));
                }
                if let PermissionChange::ChangeAutoRules {
                    change: AutoRuleChange::MigrateEnvironment { expected_text },
                } = change
                {
                    body.push(muted_row("Not applied. This restores the built-in authority boundary and retains your entire legacy Environment as one global entry with historical source unknown. Review its scope separately after migration."));
                    body.push(plain_row(format!("+ Global legacy entry: {expected_text}")));
                }
                if let PermissionChange::ChangeAutoRules {
                    change: AutoRuleChange::AddEnvironmentEntries { entries },
                } = change
                {
                    body.push(muted_row("Not applied. Built-in Environment stays active. Entries describe facts, not permission."));
                    for entry in entries {
                        body.push(plain_row(format!(
                            "+ {}\nScope: {:?} · descendants {}\nSource: {}",
                            entry.text, entry.directory, entry.include_descendants, entry.source
                        )));
                    }
                }
                body.push(RoleLine::role(title, Role::DialogTitle));
                if let Focus::AutoPreview {
                    change:
                        PermissionChange::ChangeAutoRules {
                            change: AutoRuleChange::RestoreGroup { .. },
                        },
                    ..
                } = &self.focus
                {
                    body.push(muted_row("Built-in defaults restored; user edits removed"));
                }
                body.push(muted_row(format!("Will remove {} item(s)", removed.len())));
                body.extend(removed.iter().map(|item| plain_row(format!("− {item}"))));
                "Enter/y confirm · Esc/n cancel"
            }
            Focus::List | Focus::Search(_) => unreachable!(),
        }
    }

    fn auto_group_content(&self, width: usize, group: &AutoGroupState) -> ListContent {
        let mut body = Vec::new();
        let settings = self.view.auto_rule_settings.group(group.kind);
        body.push(RoleLine::role(
            auto_kind_label(group.kind),
            Role::DialogTitle,
        ));
        body.push(muted_row(format!(
            "Built-in defaults {} · {} custom",
            if settings.include_builtins {
                "included"
            } else {
                "replaced"
            },
            settings.custom.len()
        )));
        let search = match &self.focus {
            Focus::AutoGroupSearch { editor, .. } => editor_window(editor, width.saturating_sub(9)),
            _ if group.query.is_empty() => "(none)".into(),
            _ => group.query.clone(),
        };
        body.push(labelled_row("Search  ", search));
        body.push(RoleLine::default());
        let rules = self.auto_group_rules(group);
        let replace_row = rules.len() + 1;
        let restore_row = replace_row + 1;
        let mut rows = Vec::new();
        for index in 0..=restore_row {
            let mut row = if index == 0 {
                RoleLine::role("+ Add custom rule", Role::Permission)
            } else if index == replace_row {
                plain_row("Replace group")
            } else if index == restore_row {
                plain_row("Restore group defaults")
            } else {
                let rule = &rules[index - 1];
                let mut row = RoleLine::role(&rule.text, Role::Text);
                row.push(
                    format!(" · {}", auto_source_label(&rule.source)),
                    Some(Role::MutedText),
                );
                row
            };
            let mut selected = selected_row("", index == group.selected);
            selected.spans.append(&mut row.spans);
            rows.push(selected);
        }
        let hint = if matches!(self.focus, Focus::AutoGroupSearch { .. }) {
            "Search · Enter apply · Esc back"
        } else {
            "Enter details · a add · e edit · d remove · / search · Esc back"
        };
        ListContent {
            intro: body,
            rows,
            hint,
        }
    }

    fn styled_auto_group(
        &self,
        width: usize,
        height: usize,
        group: &AutoGroupState,
        body: &mut Vec<RoleLine>,
    ) -> &'static str {
        let content = self.auto_group_content(width, group);
        body.extend(content.intro);
        let available = height
            .saturating_sub(body.len() + 2 + usize::from(self.has_status_row()))
            .max(1);
        let start = group
            .scroll
            .max(group.selected.saturating_sub(available.saturating_sub(1)))
            .min(group.selected);
        body.extend(content.rows.into_iter().skip(start).take(available));
        content.hint
    }

    fn finish_styled_frame(
        &mut self,
        title: RoleLine,
        body: Vec<RoleLine>,
        footer: RoleLine,
        width: usize,
        height: usize,
    ) -> Vec<RoleLine> {
        let notice = self
            .message
            .as_ref()
            .map(|message| RoleLine::role(format!("Status: {message}"), Role::Warning))
            .or_else(|| {
                self.notice
                    .as_ref()
                    .map(|notice| RoleLine::role(notice.clone(), Role::MutedText))
            })
            .or_else(|| {
                self.view
                    .warnings
                    .first()
                    .map(|warning| RoleLine::role(format!("Warning: {warning}"), Role::Warning))
            });
        let body_height = height.saturating_sub(2 + usize::from(notice.is_some()));
        let is_list = matches!(
            self.focus,
            Focus::List | Focus::Search(_) | Focus::AutoGroup(_) | Focus::AutoGroupSearch { .. }
        );
        let mut rows = Vec::new();
        for mut line in body {
            for span in &mut line.spans {
                span.text = super::safe_line(&span.text);
            }
            if is_list {
                line.truncate(width);
                rows.push(line);
            } else {
                rows.extend(wrap_role_line(line, width));
            }
        }
        let max_scroll = rows.len().saturating_sub(body_height);
        let start = match &self.focus {
            Focus::Detail => self.detail_scroll.min(max_scroll),
            Focus::AutoRuleDetail { scroll, .. } | Focus::AutoPreview { scroll, .. } => {
                (*scroll).min(max_scroll)
            }
            Focus::AutoSetup(_)
            | Focus::ReviewerChoice(_)
            | Focus::AutoEnvironment { .. }
            | Focus::RuleForm(_) => rows
                .iter()
                .position(|line| line.fill == Some(Role::Selection))
                .unwrap_or_default()
                .saturating_sub(body_height.saturating_sub(2))
                .min(max_scroll),
            Focus::DirectoryForm(_) | Focus::AutoDraft(_) => max_scroll,
            _ => 0,
        };
        self.detail_scroll = start;
        let mut frame = vec![title];
        frame.extend(rows.into_iter().skip(start).take(body_height));
        if let Some(notice) = notice {
            frame.push(notice);
        }
        frame.push(footer);
        frame.truncate(height);
        for line in &mut frame {
            for span in &mut line.spans {
                span.text = super::safe_line(&span.text);
            }
            line.truncate(width);
        }
        frame
    }

    pub(super) fn click(&mut self, row: usize, width: usize, height: usize) -> PermissionsEffect {
        let frame = self.styled_lines(width, height);
        let selectable = frame
            .iter()
            .enumerate()
            .filter(|(_, line)| {
                line.spans.first().is_some_and(|span| {
                    (span.role == Some(Role::Permission)
                        || self.page == Page::RecentlyDenied && !self.denials.is_detail())
                        && matches!(span.text.as_str(), "  " | "❯ ")
                })
            })
            .map(|(row, _)| row)
            .collect::<Vec<_>>();
        let Some(clicked) = selectable.iter().position(|candidate| *candidate == row) else {
            return PermissionsEffect::None;
        };
        let Some(selected) = selectable.iter().position(|candidate| {
            frame[*candidate]
                .spans
                .first()
                .is_some_and(|span| span.text == "❯ ")
        }) else {
            return PermissionsEffect::None;
        };
        let delta = clicked as isize - selected as isize;
        let direction = if delta < 0 {
            KeyCode::Up
        } else {
            KeyCode::Down
        };
        for _ in 0..delta.unsigned_abs() {
            self.key(KeyEvent::new(direction, KeyModifiers::NONE));
        }
        self.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
    }

    pub(super) fn minimum_frame_rows(&self, width: usize) -> usize {
        let intro = match &self.focus {
            _ if self.page == Page::RecentlyDenied => return 1 + self.denials.minimum_frame_rows(),
            Focus::List | Focus::Search(_) => self.overview_content(width).intro.len(),
            Focus::AutoGroup(group) | Focus::AutoGroupSearch { group, .. } => {
                self.auto_group_content(width, group).intro.len()
            }
            Focus::RuleForm(_) | Focus::DirectoryForm(_) | Focus::AutoDraft(_) => {
                let mut body = Vec::new();
                self.styled_detail(width, usize::MAX, &mut body);
                let field = if matches!(self.focus, Focus::RuleForm(_)) {
                    body.into_iter()
                        .find(|line| line.fill == Some(Role::Selection))
                } else {
                    body.pop()
                };
                let rows = field.map_or(1, |mut line| {
                    for span in &mut line.spans {
                        span.text = super::safe_line(&span.text);
                    }
                    wrap_role_line(line, width).len()
                });
                return 2 + rows + usize::from(self.has_status_row());
            }
            _ => 0,
        };
        3 + intro + usize::from(self.has_status_row())
    }

    pub(super) fn content_rows(&self, width: usize, limit: usize) -> usize {
        if self.page == Page::RecentlyDenied {
            return (1 + self.denials.content_rows(width)).min(limit);
        }
        let body = match &self.focus {
            Focus::List | Focus::Search(_) => {
                let content = self.overview_content(width);
                content.intro.len() + content.rows.len()
            }
            Focus::AutoGroup(group) | Focus::AutoGroupSearch { group, .. } => {
                let content = self.auto_group_content(width, group);
                content.intro.len() + content.rows.len()
            }
            _ => {
                let mut body = Vec::new();
                self.styled_detail(width, limit, &mut body);
                body.into_iter()
                    .map(|mut line| {
                        for span in &mut line.spans {
                            span.text = super::safe_line(&span.text);
                        }
                        wrap_role_line(line, width).len()
                    })
                    .sum()
            }
        };
        (body + 2 + usize::from(self.has_status_row())).min(limit)
    }

    pub(super) fn styled_lines(&mut self, width: usize, height: usize) -> Vec<RoleLine> {
        if height == 0 {
            return Vec::new();
        }
        let title = page_header_line(
            self.page,
            width,
            Page::visible_for(self.view.execution_mode),
        );
        if self.page == Page::RecentlyDenied {
            let mut frame = vec![title];
            frame.extend(self.denials.styled_lines(width, height.saturating_sub(1)));
            frame.truncate(height);
            return frame;
        }
        let mut body = Vec::new();
        let footer = if matches!(self.focus, Focus::List | Focus::Search(_)) {
            self.styled_overview(width, height, &mut body)
        } else {
            self.styled_detail(width, height, &mut body)
        };
        let footer = if self.read_only.is_some() {
            footer
                .split(" · ")
                .filter(|hint| !EDITING_HINTS.contains(hint))
                .collect::<Vec<_>>()
                .join(" · ")
        } else {
            footer.to_owned()
        };
        self.finish_styled_frame(title, body, muted_row(&footer), width, height)
    }

    #[cfg(test)]
    pub(super) fn lines(&mut self, width: usize, height: usize) -> Vec<String> {
        self.styled_lines(width, height)
            .iter()
            .map(RoleLine::text)
            .collect()
    }
}

fn wrap_role_line(line: RoleLine, width: usize) -> Vec<RoleLine> {
    if width == 0 {
        return vec![RoleLine::default()];
    }
    let mut rows = Vec::new();
    let mut current = RoleLine {
        fill: line.fill,
        link: line.link.clone(),
        ..RoleLine::default()
    };
    let mut used = 0;
    for span in line.spans {
        for character in span.text.chars() {
            let columns = character.width().unwrap_or(0);
            if columns > width {
                continue;
            }
            if used + columns > width {
                rows.push(current);
                current = RoleLine {
                    fill: line.fill,
                    link: line.link.clone(),
                    ..RoleLine::default()
                };
                used = 0;
            }
            current.push(character.to_string(), span.role);
            used += columns;
        }
    }
    rows.push(current);
    rows
}

pub(super) fn editor_window(editor: &Editor, available_width: usize) -> String {
    let available_width = available_width.max(3);
    let draft = editor.draft();
    let before = &draft[..editor.cursor()];
    let after = &draft[editor.cursor()..];
    let caret = '▏';
    let left_budget = available_width.saturating_sub(2);
    let mut left = String::new();
    let mut left_width = 0;
    for character in before.chars().rev() {
        let character_width = character.width().unwrap_or(0);
        if left_width + character_width > left_budget {
            break;
        }
        left.insert(0, character);
        left_width += character_width;
    }
    let hidden_left = left.len() < before.len();
    if hidden_left && left_width >= left_budget && !left.is_empty() {
        left.remove(0);
    }
    let mut visible = String::new();
    if hidden_left {
        visible.push('…');
    }
    visible.push_str(&left);
    visible.push(caret);
    let mut used = visible
        .chars()
        .map(|character| character.width().unwrap_or(0))
        .sum::<usize>();
    for character in after.chars() {
        let character_width = character.width().unwrap_or(0);
        if used + character_width > available_width {
            break;
        }
        visible.push(character);
        used += character_width;
    }
    visible
}

pub(super) fn edit_text(editor: &mut Editor, key: KeyEvent) {
    match (key.code, key.modifiers) {
        (KeyCode::Backspace, _) => {
            editor.apply(EditorCommand::DeleteBackward);
        }
        (KeyCode::Delete, _) => {
            editor.apply(EditorCommand::DeleteForward);
        }
        (KeyCode::Left, _) => {
            editor.apply(EditorCommand::MoveLeft);
        }
        (KeyCode::Right, _) => {
            editor.apply(EditorCommand::MoveRight);
        }
        (KeyCode::Home, _) => {
            editor.apply(EditorCommand::MoveLineStart);
        }
        (KeyCode::End, _) => {
            editor.apply(EditorCommand::MoveLineEnd);
        }
        (KeyCode::Char('a'), KeyModifiers::CONTROL) => {
            editor.apply(EditorCommand::MoveLineStart);
        }
        (KeyCode::Char('e'), KeyModifiers::CONTROL) => {
            editor.apply(EditorCommand::MoveLineEnd);
        }
        (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
            editor.apply(EditorCommand::KillToLineStart);
        }
        (KeyCode::Char('k'), KeyModifiers::CONTROL) => {
            editor.apply(EditorCommand::KillToLineEnd);
        }
        (KeyCode::Char('z'), KeyModifiers::CONTROL) => {
            editor.apply(EditorCommand::Undo);
        }
        (KeyCode::Char(character), modifiers) if !modifiers.contains(KeyModifiers::CONTROL) => {
            editor.apply(EditorCommand::Insert(character.to_string()));
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permissions_header_keeps_the_active_tab_visible_at_workbench_width() {
        for width in [18, 30, 62, 100] {
            for page in Page::ALL {
                let header = page_header(page, width);
                assert!(header.contains(&format!("[{}]", page.label())), "{header}");
                assert!(UnicodeWidthStr::width(header.as_str()) <= width);
            }
        }
    }

    #[test]
    fn j_and_k_stay_tool_text() {
        let mut draft = RuleDraft::new();
        for character in "jkmove".chars() {
            edit_text(
                &mut draft.editor,
                KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE),
            );
        }
        draft.commit_editor();
        assert_eq!(draft.tool, "jkmove");
    }

    #[test]
    fn editing_a_loop_rule_preserves_its_original_loop_id() {
        let original_loop_id = Uuid::new_v4();
        let rule = PermissionRule {
            id: Uuid::new_v4(),
            decision: Decision::Ask,
            tool: "Write".into(),
            specifier: None,
            input: None,
            loop_id: Some(original_loop_id),
            persistent: false,
        };
        let mut draft = RuleDraft::existing(&rule);
        let PermissionChange::UpsertRule { rule: edited } =
            draft.change(Decision::Ask, Some(Uuid::new_v4())).unwrap()
        else {
            panic!("expected rule update");
        };
        assert_eq!(edited.loop_id, Some(original_loop_id));
        assert_eq!(edited.id, rule.id);
    }

    #[test]
    fn matcher_retains_exact_json_and_accepts_rule_content_only_where_a_tool_matches_it() {
        let mut draft = RuleDraft::new();
        draft.editor.replace_draft("Bash".into(), 4);
        draft.commit_editor();
        draft.matcher = Matcher::ExactInput;
        draft.match_text = r#"{"command":"echo hi"}"#.into();
        let PermissionChange::UpsertRule { rule } = draft.change(Decision::Deny, None).unwrap()
        else {
            panic!("expected rule update");
        };
        assert_eq!(rule.input.unwrap()["command"], "echo hi");
        draft.editor.replace_draft("Write".into(), 5);
        draft.commit_editor();
        draft.matcher = Matcher::RuleContent;
        assert!(draft.change(Decision::Deny, None).is_err());

        draft.editor.replace_draft("WebFetch".into(), 8);
        draft.commit_editor();
        draft.match_text = "docs.example.com".into();
        assert!(draft.change(Decision::Allow, None).is_err());
        draft.match_text = "domain:docs.example.com".into();
        let PermissionChange::UpsertRule { rule } = draft.change(Decision::Allow, None).unwrap()
        else {
            panic!("expected rule update");
        };
        assert_eq!(rule.specifier.as_deref(), Some("domain:docs.example.com"));
        assert!(rule.input.is_none());
    }

    #[test]
    fn editing_a_shell_pattern_does_not_trim_its_matcher() {
        let rule = PermissionRule {
            id: Uuid::new_v4(),
            decision: Decision::Ask,
            tool: "Bash".into(),
            specifier: Some("  cargo *  ".into()),
            input: None,
            loop_id: None,
            persistent: false,
        };
        let mut draft = RuleDraft::existing(&rule);
        let PermissionChange::UpsertRule { rule: edited } =
            draft.change(Decision::Ask, None).unwrap()
        else {
            panic!("expected rule update");
        };
        assert_eq!(edited.specifier, rule.specifier);
    }

    #[test]
    fn long_editor_field_keeps_the_caret_visible_without_changing_the_draft() {
        let mut editor = Editor::new();
        editor.replace_draft("/a/very/long/path/for/project".into(), 22);
        let visible = editor_window(&editor, 12);
        assert!(visible.contains('▏'));
        assert!(visible.starts_with('…'));
        assert!(visible.chars().count() <= 12);
        assert_eq!(editor.draft(), "/a/very/long/path/for/project");
    }

    #[test]
    fn replacement_draft_keeps_distinct_rule_texts_and_empty_replacement() {
        let mut draft = AutoDraft::new(
            AutoDraftPurpose::Replace(AutoRuleKind::SoftDeny),
            "One  \n Two".into(),
        );
        let PermissionChange::ChangeAutoRules {
            change: AutoRuleChange::ReplaceGroup { kind, texts },
        } = draft.change().unwrap()
        else {
            panic!("replace expected")
        };
        assert_eq!(kind, AutoRuleKind::SoftDeny);
        assert_eq!(texts, ["One  ", " Two"]);
        draft.editor.replace_draft(String::new(), 0);
        let PermissionChange::ChangeAutoRules {
            change: AutoRuleChange::ReplaceGroup { texts, .. },
        } = draft.change().unwrap()
        else {
            panic!("replace expected")
        };
        assert!(texts.is_empty());
    }
}

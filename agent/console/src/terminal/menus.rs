use super::execution::ConsoleExecution as Agent;
mod judgment;
pub(super) mod providers;
#[cfg(test)]
mod settings_ux_pty;
use providers::{ModelChoice, ProviderModels, ProviderRow};

use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    path::PathBuf,
};

use tickloop_agent_base::output_style::OutputStyle;
use tickloop_agent_config::{DisplayPreferences, GlyphSet, UserSettings};
use tickloop_agent_tools::{
    ENTER_PLAN_MODE_TOOL_NAME,
    file_change::load_file_change_review,
    permissions::{PermissionRuleCandidate, permission_rule_candidates},
};
use uuid::Uuid;

use super::{
    completion::{CommandGroup, CommandSpec, builtin_commands},
    console::{Console, Menu, PickerAction, PickerKind, RequestPresentation},
    conversation_picker::ConversationPicker,
    diff_view::{DiffNavigation, DiffViewport, render_diff, render_error},
    display::{DisplayProfile, Glyph},
    font_manager::{FontAction, FontOutcome, NerdFontManager, SystemFontAdapter},
    new_agent_draft::ConfigTarget,
    permissions_surface::PermissionsSubject,
    presentation::permission_context,
    questions::{FlowUpdate, QuestionFlow, question_lines},
    safe_line, safe_text, settings,
    style::ThemeName,
    workflows::{self, Workflow},
};
use tickloop_agent_model::{ModelConfigExt, ReasoningEffortExt};
use tickloop_agent_protocol::{
    AutoReviewerPreference, ChatSnapshot, ConfigurationChange, CounterSummary, CounterTotal,
    DirectedSummaryRequest, DisplayedView, EventKind, ExecutionMode, ExecutionRef, ModelConfig,
    ModelRequestKind, PermissionApprovalSource, PermissionChange, PermissionRequest,
    PermissionScope, Protocol, ProtocolFailure, QuestionRequest, ReasoningEffort, Receipt,
    RequestFailure, RequestOutcome, ResponseRejection, RewindMode, RewindOutcome, SummaryDirection,
    UsageCounter, UsageCoverage, UsageSummary, chat_display_title,
};

pub(super) const REDUCE_MOTION_SETTING_INDEX: usize = 9;
pub(super) const OUTPUT_STYLE_SETTING_INDEX: usize = 10;
pub(super) const PERMISSIONS_SETTING_INDEX: usize = 11;
const PROMPT_SUGGESTION_SETTING_INDEX: usize = 13;

enum Selection {
    ArtifactSignOut {
        target: ConfigTarget,
    },
    Locations {
        generation: u64,
        views: Vec<DisplayedView>,
    },
    Permission {
        chat: Uuid,
        request: PermissionRequest,
        goal: String,
        /// Whether the requesting Agent may save a grant as a rule; a delegated child's grants
        /// stay within its session.
        persistent: bool,
    },
    PermissionRule {
        chat: Uuid,
        request: PermissionRequest,
        goal: String,
        candidates: Vec<PermissionRuleCandidate>,
    },
    Provider {
        target: ConfigTarget,
        choices: Vec<ProviderModels>,
        rows: Vec<ProviderRow>,
    },
    ConnectionDetails {
        target: ConfigTarget,
        provider: ProviderModels,
        actions: Vec<providers::ConnectionAction>,
    },
    ConnectionConfirm {
        target: ConfigTarget,
        provider: ProviderModels,
        action: providers::ConnectionAction,
    },
    CredentialDetails {
        target: ConfigTarget,
    },
    LoginMethods {
        target: ConfigTarget,
        provider: ProviderModels,
    },
    Model {
        target: ConfigTarget,
        choices: Vec<ModelChoice>,
        actions: Vec<providers::ModelAction>,
    },

    ApiKey {
        target: ConfigTarget,
        provider: ProviderModels,
    },
    ServiceAccount {
        target: ConfigTarget,
        provider: ProviderModels,
    },
    JudgmentSettings {
        return_to: JudgmentReturn,
        has_key: bool,
    },
    JudgmentKey {
        return_to: JudgmentReturn,
    },
    Effort {
        target: ConfigTarget,
        choices: Vec<Option<ReasoningEffort>>,
        return_to_settings: bool,
    },
    Settings {
        target: ConfigTarget,
    },
    Theme {
        /// Present when opened from Settings, so confirming returns there.
        settings_target: Option<ConfigTarget>,
    },
    GlyphSet {
        target: ConfigTarget,
        font_action: Option<FontAction>,
    },
    GlyphSetConfirmation {
        target: ConfigTarget,
        glyph_set: GlyphSet,
    },
    FontActionConfirmation {
        target: ConfigTarget,
        action: FontAction,
    },
    SettingsInfo {
        target: ConfigTarget,
    },
    OutputStyle {
        target: ConfigTarget,
    },
    Mode {
        target: ConfigTarget,
        choices: Vec<ExecutionMode>,
    },
    PermissionMode {
        target: ConfigTarget,
        choices: Vec<ExecutionMode>,
        current: ExecutionMode,
        return_to_settings: bool,
    },
    Question(QuestionFlow),
    Workflow(Workflow),
    Help {
        section: Option<usize>,
    },
    HelpReference,
    Rewind {
        chat: Uuid,
        points: Vec<RewindPoint>,
    },
    RewindActions {
        chat: Uuid,
        points: Vec<RewindPoint>,
        message_id: Uuid,
        actions: Vec<RewindAction>,
    },
}

#[derive(Clone)]
enum JudgmentReturn {
    Providers {
        target: ConfigTarget,
    },
    /// Opened from a Permissions view; its subject names the Agent or New Agent screen and the
    /// revision the view was read at.
    AutoSettings {
        subject: PermissionsSubject,
        select_reviewer: bool,
    },
}

impl JudgmentReturn {
    fn target(&self) -> ConfigTarget {
        match self {
            Self::Providers { target } => *target,
            Self::AutoSettings { subject, .. } => ConfigTarget::visible(subject.chat()),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct RewindPoint {
    message_id: Uuid,
    message_index: usize,
    prompt: String,
    /// 恢复代码会改变的文件；选中该提示后才比较磁盘，`None` 表示这条提示没有文件快照。
    changed_files: Option<Vec<PathBuf>>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum RewindAction {
    RestoreCodeAndConversation,
    RestoreConversation,
    RestoreCode,
    SummarizeFromHere,
    SummarizeUpToHere,
    Back,
}

impl RewindAction {
    fn label(self) -> &'static str {
        match self {
            Self::RestoreCodeAndConversation => "Restore code and conversation",
            Self::RestoreConversation => "Restore conversation",
            Self::RestoreCode => "Restore code",
            Self::SummarizeFromHere => "Summarize from here",
            Self::SummarizeUpToHere => "Summarize up to here",
            Self::Back => "Never mind",
        }
    }

    fn summary_direction(self) -> Option<SummaryDirection> {
        match self {
            Self::SummarizeFromHere => Some(SummaryDirection::FromHere),
            Self::SummarizeUpToHere => Some(SummaryDirection::UpToHere),
            _ => None,
        }
    }
}

fn directed_summary_request(
    action: RewindAction,
    message_id: Uuid,
    guidance: Option<String>,
) -> Option<DirectedSummaryRequest> {
    Some(DirectedSummaryRequest {
        message_id,
        direction: action.summary_direction()?,
        guidance: guidance.filter(|value| !value.trim().is_empty()),
    })
}

impl RewindPoint {
    /// 照 Claude Code：只有恢复代码会改变文件时才提供恢复代码的选项。
    fn can_restore_code(&self) -> bool {
        self.changed_files
            .as_ref()
            .is_some_and(|files| !files.is_empty())
    }

    fn file_history_description(&self) -> String {
        match &self.changed_files {
            None => "No file snapshot is recorded for this prompt; code cannot be restored.".into(),
            Some(files) if files.is_empty() => {
                "Tracked files already match this prompt; restoring code would change nothing."
                    .into()
            }
            Some(files) => format!(
                "Restoring code changes {} tracked file{}: {}",
                files.len(),
                if files.len() == 1 { "" } else { "s" },
                files
                    .iter()
                    .map(|file| safe_line(&file.to_string_lossy()))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    fn actions(&self) -> Vec<RewindAction> {
        let mut actions = Vec::new();
        if self.can_restore_code() {
            actions.push(RewindAction::RestoreCodeAndConversation);
        }
        actions.push(RewindAction::RestoreConversation);
        if self.can_restore_code() {
            actions.push(RewindAction::RestoreCode);
        }
        actions.extend([
            RewindAction::SummarizeFromHere,
            RewindAction::SummarizeUpToHere,
            RewindAction::Back,
        ]);
        actions
    }
}

impl Selection {
    fn configuration_retry(&self) -> Option<Self> {
        Some(match self {
            Self::Settings { target } => Self::Settings { target: *target },
            Self::SettingsInfo { target } => Self::SettingsInfo { target: *target },
            Self::Theme { settings_target } => Self::Theme {
                settings_target: *settings_target,
            },
            Self::OutputStyle { target } => Self::OutputStyle { target: *target },
            Self::GlyphSet {
                target,
                font_action,
            } => Self::GlyphSet {
                target: *target,
                font_action: *font_action,
            },
            Self::GlyphSetConfirmation { target, glyph_set } => Self::GlyphSetConfirmation {
                target: *target,
                glyph_set: *glyph_set,
            },
            Self::FontActionConfirmation { target, action } => Self::FontActionConfirmation {
                target: *target,
                action: *action,
            },
            Self::ArtifactSignOut { target } => Self::ArtifactSignOut { target: *target },
            Self::Effort {
                target,
                choices,
                return_to_settings,
            } => Self::Effort {
                target: *target,
                choices: choices.clone(),
                return_to_settings: *return_to_settings,
            },
            Self::Mode { target, choices } => Self::Mode {
                target: *target,
                choices: choices.clone(),
            },
            Self::Model {
                target,
                choices,
                actions,
            } => Self::Model {
                target: *target,
                choices: choices.clone(),
                actions: actions.clone(),
            },
            Self::Provider {
                target,
                choices,
                rows,
            } => Self::Provider {
                target: *target,
                choices: choices.clone(),
                rows: rows.clone(),
            },
            Self::ConnectionDetails {
                target,
                provider,
                actions,
            } => Self::ConnectionDetails {
                target: *target,
                provider: provider.clone(),
                actions: actions.clone(),
            },
            Self::LoginMethods { target, provider } => Self::LoginMethods {
                target: *target,
                provider: provider.clone(),
            },
            Self::ConnectionConfirm {
                target,
                provider,
                action,
            } => Self::ConnectionConfirm {
                target: *target,
                provider: provider.clone(),
                action: *action,
            },
            _ => return None,
        })
    }

    fn request_id(&self) -> Option<Uuid> {
        match self {
            Self::Permission { request, .. } | Self::PermissionRule { request, .. } => {
                Some(request.id)
            }
            Self::Question(flow) => Some(flow.request().id),
            Self::Workflow(Workflow::Plan { goal, request, .. }) => Some(request.unwrap_or(*goal)),
            _ => None,
        }
    }

    /// The visible Agent a menu belongs to; New Agent menus belong to no Agent yet.
    fn chat(&self) -> Option<Uuid> {
        self.config_target().map_or_else(
            || match self {
                Self::Permission { chat, .. }
                | Self::PermissionRule { chat, .. }
                | Self::Rewind { chat, .. }
                | Self::RewindActions { chat, .. } => Some(*chat),
                Self::Question(flow) => Some(flow.request().chat_id),
                Self::Workflow(workflow) => workflow.chat(),
                _ => None,
            },
            ConfigTarget::agent,
        )
    }

    fn config_target(&self) -> Option<ConfigTarget> {
        match self {
            Self::Provider { target, .. }
            | Self::ConnectionDetails { target, .. }
            | Self::ConnectionConfirm { target, .. }
            | Self::CredentialDetails { target }
            | Self::LoginMethods { target, .. }
            | Self::ApiKey { target, .. }
            | Self::ServiceAccount { target, .. }
            | Self::Model { target, .. }
            | Self::Effort { target, .. }
            | Self::ArtifactSignOut { target }
            | Self::Settings { target }
            | Self::SettingsInfo { target }
            | Self::OutputStyle { target }
            | Self::GlyphSet { target, .. }
            | Self::GlyphSetConfirmation { target, .. }
            | Self::FontActionConfirmation { target, .. }
            | Self::Mode { target, .. }
            | Self::PermissionMode { target, .. } => Some(*target),
            Self::JudgmentSettings { return_to, .. } | Self::JudgmentKey { return_to } => {
                Some(return_to.target())
            }
            Self::Theme { settings_target } => *settings_target,
            _ => None,
        }
    }
}

struct RestoredPlanApproval {
    chat: Uuid,
    loop_id: Uuid,
    version: u64,
}

#[derive(Clone, Copy)]
struct SettingsPosition {
    selected: usize,
    context_offset: usize,
}

pub(super) struct Menus {
    active: Option<(Uuid, Selection)>,
    dialog_chat: Option<Uuid>,
    active_cache: HashMap<Option<Uuid>, Option<(Uuid, Selection)>>,
    settings_positions: HashMap<Option<Uuid>, SettingsPosition>,
    provider_chat: Option<Option<Uuid>>,
    provider_sessions: HashMap<Option<Uuid>, providers::ProviderSession>,
    provider_login_results: HashMap<Option<Uuid>, bool>,
    suspended_active: HashMap<Option<Uuid>, (Uuid, Selection)>,
    request_order: VecDeque<(Uuid, Uuid)>,
    /// Questions whose dialog was closed; they wait for typed answers instead.
    deferred_questions: HashSet<Uuid>,
    /// Answers typed so far, one line per question.
    typed_questions: HashMap<Uuid, QuestionFlow>,
    restored_plans: HashMap<Uuid, RestoredPlanApproval>,
    scanned_plan_executions: HashMap<Uuid, ExecutionRef>,
    user_settings: Option<UserSettings>,
    provider: providers::ProviderSession,
    config_root: Option<PathBuf>,
    refresh_connections: HashMap<(Option<Uuid>, String), Uuid>,
    model_discovery: tickloop_agent_model::availability::AccountModelDiscovery,
    font_manager: Option<NerdFontManager<SystemFontAdapter>>,
    /// Where the next New Agent will be created; its saved project rules are shown before the
    /// first input.
    new_agent_directory: Option<PathBuf>,
}

impl Default for Menus {
    fn default() -> Self {
        Self::new(None, None)
    }
}

impl Menus {
    pub(super) fn new(user_settings: Option<UserSettings>, config_root: Option<PathBuf>) -> Self {
        Self {
            active: None,
            dialog_chat: None,
            active_cache: HashMap::new(),
            settings_positions: HashMap::new(),
            provider_chat: None,
            provider_sessions: HashMap::new(),
            provider_login_results: HashMap::new(),
            suspended_active: HashMap::new(),
            request_order: VecDeque::new(),
            deferred_questions: HashSet::new(),
            typed_questions: HashMap::new(),
            restored_plans: HashMap::new(),
            scanned_plan_executions: HashMap::new(),
            user_settings,
            provider: Default::default(),
            config_root: config_root.clone(),
            refresh_connections: HashMap::new(),
            model_discovery: Default::default(),
            font_manager: config_root.map(NerdFontManager::production),
            new_agent_directory: None,
        }
    }

    /// Keeps the New Agent directory current; the View sets it at startup and on `/cwd`.
    pub(super) fn set_new_agent_directory(&mut self, directory: PathBuf) {
        self.new_agent_directory = Some(directory);
    }

    fn new_agent_directory(&self, agent: &Agent) -> PathBuf {
        self.new_agent_directory
            .clone()
            .unwrap_or_else(|| agent.launch_spec().workdir)
    }

    /// Opens the New Agent's Permissions view: saved policy and staged choices, with no activity
    /// before the first input.
    pub(super) async fn open_draft_permissions(
        &mut self,
        agent: &Agent,
        console: &Console,
        return_to_settings: bool,
        auto_page: bool,
    ) -> Result<(), String> {
        let project = self.new_agent_directory(agent);
        let page = agent
            .draft_permissions(&project)
            .await
            .map_err(|error| error.to_string())?;
        let history = tickloop_agent_protocol::ApprovalHistoryView {
            records: Vec::new(),
            persistence: if agent.saves_chats() {
                tickloop_agent_protocol::DenialPersistence::Saved
            } else {
                tickloop_agent_protocol::DenialPersistence::Disabled
            },
            load_warning: None,
        };
        console.show_permissions(page, Ok(history), return_to_settings, auto_page)
    }
}

#[derive(Default)]
pub(super) struct MenuOutcome {
    pub provider_logout: bool,
    pub logout_target: super::login::SignInTarget,
    pub reauthenticate_after_logout: bool,
    pub provider_login: Option<super::login::LoginMethod>,
    pub permission: Option<Uuid>,
    pub question: Option<Uuid>,
    pub draft_question: Option<Uuid>,
    pub selection: Option<(Uuid, Option<Uuid>)>,
    pub restored: Option<ChatSnapshot>,
    pub receipt: Option<Receipt>,
    pub command: Option<String>,
    pub summary: Option<(Uuid, DirectedSummaryRequest)>,
    pub status: Option<String>,
}

const COMMAND_GROUPS: &[CommandGroup] = &[
    CommandGroup::Conversation,
    CommandGroup::ModelAndSettings,
    CommandGroup::ToolsAndPermissions,
    CommandGroup::PlanAndExecution,
    CommandGroup::Skills,
];

fn commands_in_group(group: CommandGroup) -> impl Iterator<Item = &'static CommandSpec> {
    builtin_commands()
        .iter()
        .filter(move |command| command.menu_group == Some(group))
}

impl Menus {
    pub(super) fn rewind(
        &mut self,
        console: &Console,
        snapshot: &ChatSnapshot,
    ) -> Result<Option<String>, String> {
        let points = rewind_points(snapshot);
        if points.is_empty() {
            let status =
                "No prompts to rewind to yet. Open Transcript with Ctrl+O to read history.";
            console.set_status(status)?;
            return Ok(Some(status.into()));
        }
        self.show_rewind_points(console, snapshot.id, points)?;
        Ok(None)
    }

    fn show_rewind_points(
        &mut self,
        console: &Console,
        chat: Uuid,
        points: Vec<RewindPoint>,
    ) -> Result<(), String> {
        let mut options = points
            .iter()
            .map(|point| format!("Before · {}", point.prompt))
            .collect::<Vec<_>>();
        options.push("Current point · keep conversation unchanged".into());
        self.show(
            console,
            Menu {
                id: Uuid::new_v4(),
                title: "Rewind".into(),
                context: vec![
                    "Choose a prompt to restore or summarize from.".into(),
                    "Abandoned messages stay in the Transcript file.".into(),
                ],
                selected: points.len(),
                options,
                confirmation: false,
            },
            Selection::Rewind { chat, points },
        )
    }

    fn show_rewind_actions(
        &mut self,
        console: &Console,
        chat: Uuid,
        points: Vec<RewindPoint>,
        message_id: Uuid,
    ) -> Result<(), String> {
        let point = points
            .iter()
            .find(|point| point.message_id == message_id)
            .ok_or("rewind point does not exist")?;
        let actions = point.actions();
        let selected = actions
            .iter()
            .position(|action| *action == RewindAction::RestoreConversation)
            .unwrap_or(0);
        let comment_options = actions
            .iter()
            .enumerate()
            .filter_map(|(index, action)| action.summary_direction().map(|_| index))
            .collect();
        self.show_focused_commentable(
            console,
            Menu {
                id: Uuid::new_v4(),
                title: "Rewind actions".into(),
                context: vec![
                    format!(
                        "Before message {} · {}",
                        point.message_index + 1,
                        point.prompt
                    ),
                    "Summary actions use the selected Chat model. Tab adds optional guidance; a displayed number runs immediately without guidance.".into(),
                    point.file_history_description(),
                    "Only files changed by the Agent's file tools are restored; Shell and external side effects are not covered.".into(),
                ],
                options: actions.iter().map(|action| action.label().into()).collect(),
                selected,
                confirmation: false,
            },
            Selection::RewindActions {
                chat,
                points,
                message_id,
                actions,
            },
            comment_options,
        )
    }

    pub(super) fn help(&mut self, console: &Console, section: Option<usize>) -> Result<(), String> {
        if section == Some(COMMAND_GROUPS.len()) {
            return self.show(
                console,
                Menu {
                    id: Uuid::new_v4(),
                    title: "Advanced commands".into(),
                    context: builtin_commands()
                        .iter()
                        .filter(|command| command.menu_group == Some(CommandGroup::Advanced))
                        .map(|command| {
                            format!(
                                "{} — {}",
                                command.syntax(),
                                command.description_in(console.command_scope())
                            )
                        })
                        .collect(),
                    options: vec!["Back to command groups".into()],
                    selected: 0,
                    confirmation: false,
                },
                Selection::HelpReference,
            );
        }
        let (title, options) = if let Some(index) = section {
            let group = *COMMAND_GROUPS
                .get(index)
                .ok_or("command group does not exist")?;
            (
                group.title(),
                commands_in_group(group)
                    .map(|command| {
                        format!(
                            "{} · {}",
                            command.description_in(console.command_scope()),
                            command.syntax()
                        )
                    })
                    .collect(),
            )
        } else {
            (
                "Commands",
                COMMAND_GROUPS
                    .iter()
                    .map(|group| group.title().into())
                    .chain(std::iter::once("Advanced commands and arguments".into()))
                    .collect(),
            )
        };
        self.show(
            console,
            Menu {
                id: Uuid::new_v4(),
                title: title.into(),
                context: vec!["Choose an action, or use its slash command directly.".into()],
                options,
                selected: 0,
                confirmation: false,
            },
            Selection::Help { section },
        )
    }
    pub(super) fn help_advanced(&mut self, console: &Console) -> Result<(), String> {
        self.help(console, Some(COMMAND_GROUPS.len()))
    }

    async fn edit_loop_timer(
        &mut self,
        agent: &Agent,
        console: &Console,
        chat: Uuid,
        goal: Uuid,
    ) -> Result<(), String> {
        let snapshot = agent
            .snapshot(chat)
            .await
            .map_err(|error| error.to_string())?;
        let selected = snapshot
            .loops
            .iter()
            .find(|item| item.id == goal)
            .ok_or("The selected goal no longer exists.")?;
        let trigger = selected.trigger.as_ref();
        let seconds = trigger
            .and_then(|trigger| trigger.interval_seconds)
            .unwrap_or(60);
        let duration = trigger
            .and_then(|trigger| trigger.expires_at_ms)
            .map(|expiry| {
                format!(
                    " --for {}",
                    expiry
                        .saturating_sub(super::current_time_ms())
                        .div_ceil(1000)
                        .max(1)
                )
            })
            .unwrap_or_default();
        let prompt = trigger
            .map(|trigger| trigger.prompt.as_str())
            .filter(|prompt| !prompt.is_empty())
            .unwrap_or(&selected.goal);
        console.prepare_command(format!(
            "/loops schedule {goal} {seconds}{duration} {prompt}"
        ))
    }
    pub(super) fn workflow(
        &mut self,
        console: &Console,
        (menu, workflow): (Menu, Workflow),
    ) -> Result<(), String> {
        self.show(console, menu, Selection::Workflow(workflow))
    }
    /// Opens the saved-conversation picker. A terminal without interaction lists the complete
    /// rows with their Chat IDs, which scripts match to `Chat:` lines, and takes a number.
    pub(super) fn conversations(
        &mut self,
        console: &Console,
        (picker, workflow): (ConversationPicker, Workflow),
    ) -> Result<(), String> {
        self.sync_dialog_chat(console)?;
        self.dismiss(console)?;
        console.set_request_layout(None)?;
        let id = picker.id();
        if console.is_interactive() {
            console.show_conversation_picker(picker)?;
        } else if let Workflow::Chats { choices, .. } = &workflow {
            let mut listing = format!("{}\n{}\n", picker.title(), picker.context());
            for (number, (entry, chat)) in picker.entries().iter().zip(choices).enumerate() {
                listing.push_str(&format!(
                    "  {}. {} · {chat}\n",
                    number + 1,
                    entry.plain_row()
                ));
            }
            listing.push_str(&format!(
                "  {}. Cancel\nEnter an option number, or cancel.\n",
                choices.len() + 1
            ));
            console.write(&listing)?;
        }
        self.active = Some((id, Selection::Workflow(workflow)));
        Ok(())
    }
    pub(super) fn id(&self) -> Option<Uuid> {
        self.active.as_ref().map(|(id, _)| *id)
    }

    pub(super) fn locations(
        &mut self,
        console: &Console,
        generation: u64,
        views: Vec<DisplayedView>,
    ) -> Result<(), String> {
        let options = views
            .iter()
            .map(|view| {
                let host = view
                    .host
                    .as_ref()
                    .expect("location choices have verified hosts");
                format!(
                    "Workspace {} · Tab {} · Pane {}",
                    host.workspace.as_deref().unwrap_or("unknown"),
                    host.tab.as_deref().unwrap_or("unknown"),
                    host.pane.as_deref().unwrap_or("unknown")
                )
            })
            .collect();
        self.show(
            console,
            Menu {
                id: Uuid::new_v4(),
                title: "Choose Agent location".into(),
                context: vec![
                    "Several live Views exist in this Herdr instance. Choose a verified location."
                        .into(),
                ],
                options,
                selected: 0,
                confirmation: false,
            },
            Selection::Locations { generation, views },
        )
    }

    pub(super) fn note_request(&mut self, chat: Uuid, request: Uuid) {
        if !self.request_order.iter().any(|(_, id)| *id == request) {
            self.request_order.push_back((chat, request));
        }
    }

    pub(super) fn note_snapshot_requests(&mut self, snapshot: &ChatSnapshot) {
        let pending = snapshot
            .permissions
            .iter()
            .map(|request| request.id)
            .chain(snapshot.questions.iter().map(|request| request.id))
            .collect::<Vec<_>>();
        let retained = snapshot
            .events
            .iter()
            .filter_map(|event| match &event.kind {
                EventKind::PermissionRequested { request } => Some(request.id),
                EventKind::QuestionAsked { request } => Some(request.id),
                _ => None,
            })
            .filter(|id| pending.contains(id))
            .collect::<Vec<_>>();
        for id in pending.iter().filter(|id| !retained.contains(id)) {
            self.note_request(snapshot.id, *id);
        }
        for id in retained {
            self.note_request(snapshot.id, id);
        }
    }

    pub(super) fn sync_dialog_chat(&mut self, console: &Console) -> Result<(), String> {
        let chat = console.dialog_chat()?;
        self.sync_provider_chat(chat);
        if self.dialog_chat != chat {
            self.active_cache
                .insert(self.dialog_chat, self.active.take());
            self.active = self.active_cache.remove(&chat).flatten();
            self.dialog_chat = chat;
        }
        if self.active.is_none()
            && self
                .suspended_active
                .get(&chat)
                .is_some_and(|(id, _)| console.dialog_menu_id().ok().flatten() == Some(*id))
        {
            self.active = self.suspended_active.remove(&chat);
        }
        Ok(())
    }

    fn prepare_dialog(
        &mut self,
        console: &Console,
        request: Option<Uuid>,
    ) -> Result<RequestPresentation, String> {
        self.sync_dialog_chat(console)?;
        if request.is_some()
            && self
                .active
                .as_ref()
                .is_some_and(|(_, selection)| selection.request_id().is_none())
        {
            let presentation = console.set_request_layout(request)?;
            if presentation == RequestPresentation::Deferred {
                return Ok(presentation);
            }
            if let Some(active) = self.active.take() {
                self.suspended_active.insert(self.dialog_chat, active);
            }
            Ok(RequestPresentation::Presented)
        } else {
            self.dismiss(console)?;
            console.set_request_layout(request)
        }
    }

    pub(super) fn dismiss(&mut self, console: &Console) -> Result<(), String> {
        if let Some((id, _)) = self.active.take() {
            self.cancel_model_discovery(id);
            console.clear_menu(id)?;
        }
        Ok(())
    }

    fn cancel_model_discovery(&mut self, menu_id: Uuid) {
        if self
            .provider
            .discovery_flow
            .as_ref()
            .is_some_and(|flow| flow.id == menu_id)
        {
            self.provider.discovery_flow.take();
        }
    }

    fn show(&mut self, console: &Console, menu: Menu, selection: Selection) -> Result<(), String> {
        let discovery = (self.id() == Some(menu.id))
            .then(|| self.provider.discovery_flow.take())
            .flatten();
        let presentation = if self.active.as_ref().is_some_and(|(id, previous)| {
            selection.config_target().is_some()
                && previous.config_target() == selection.config_target()
                && std::mem::discriminant(previous) == std::mem::discriminant(&selection)
                && console.dialog_menu_id().ok().flatten() == Some(*id)
        }) && !console.agent_request_is_open()?
        {
            RequestPresentation::Presented
        } else {
            self.prepare_dialog(console, selection.request_id())?
        };
        if let Some(discovery) = discovery {
            self.provider.discovery_flow = Some(discovery);
        }
        if presentation == RequestPresentation::Deferred {
            return Ok(());
        }
        if !console.is_interactive() {
            console.write(&format!("{}\n{}\n", menu.title, menu.context.join("\n")))?;
            for (index, option) in menu.options.iter().enumerate() {
                console.write(&format!("  {}. {}\n", index + 1, option))?;
            }
            console.write("Enter an option number, or cancel.\n")?;
        }
        let id = menu.id;
        if console.is_interactive() {
            let plan_feedback = matches!(&selection, Selection::Workflow(Workflow::Plan { .. }));
            let plan_dialog = plan_feedback
                || matches!(&selection, Selection::Workflow(Workflow::PlanView { .. }))
                || matches!(&selection, Selection::Permission { request, .. } if request.name == ENTER_PLAN_MODE_TOOL_NAME);
            if plan_dialog {
                console.show_plan_menu(menu, plan_feedback)?;
            } else if matches!(&selection, Selection::Provider { .. }) {
                console.show_picker_menu(menu, PickerKind::Provider)?;
            } else if matches!(
                &selection,
                Selection::ConnectionDetails { .. }
                    | Selection::ConnectionConfirm { .. }
                    | Selection::CredentialDetails { .. }
                    | Selection::LoginMethods { .. }
                    | Selection::JudgmentSettings { .. }
                    | Selection::PermissionMode { .. }
                    | Selection::Settings { .. }
                    | Selection::SettingsInfo { .. }
                    | Selection::Effort { .. }
                    | Selection::Mode { .. }
                    | Selection::Theme { .. }
                    | Selection::OutputStyle { .. }
                    | Selection::GlyphSet { .. }
                    | Selection::GlyphSetConfirmation { .. }
                    | Selection::FontActionConfirmation { .. }
                    | Selection::ArtifactSignOut { .. }
            ) {
                let login_hint =
                    format!("Enter start · {}", self.provider.provider_nav.escape_hint());
                let hint = match &selection {
                    Selection::Settings { .. } => "Enter change · Esc close",
                    Selection::Effort { .. }
                    | Selection::Mode { .. }
                    | Selection::Theme { .. }
                    | Selection::OutputStyle { .. } => "Enter apply · Esc back",
                    Selection::ConnectionConfirm { .. } => "Enter confirm · Esc Connection",
                    Selection::LoginMethods { .. } => &login_hint,
                    Selection::CredentialDetails { .. } => "Enter back · Esc Connection",
                    _ => "Enter open · Esc back",
                };
                console.show_presented_picker_menu(
                    menu,
                    PickerKind::Actions,
                    super::console::PickerPresentation {
                        hint: hint.into(),
                        failure_target: selection.config_target().and_then(|target| target.agent()),
                        failure_connections: if let Selection::ConnectionDetails {
                            provider,
                            actions,
                            ..
                        } = &selection
                        {
                            vec![Some(provider.models[0].provider_id.clone()); actions.len()]
                        } else {
                            Vec::new()
                        },
                        failure_message: matches!(&selection, Selection::ConnectionDetails { .. }),
                        restore_context_offset: if let Selection::Settings { target } = &selection {
                            self.settings_positions
                                .get(&target.agent())
                                .map(|position| position.context_offset)
                        } else {
                            None
                        },
                        ..Default::default()
                    },
                )?;
            } else if let Selection::Model {
                choices, actions, ..
            } = &selection
            {
                let refresh = actions.contains(&providers::ModelAction::RefreshAccountModels);
                let mut headers = Vec::new();
                let mut last_connection = None;
                let mut row_hints = Vec::new();
                let mut refresh_options = Vec::new();
                for (index, choice) in choices.iter().enumerate() {
                    let provider = self
                        .key_for_model(&choice.config)
                        .and_then(|key| self.connection(&key).ok());
                    if self.provider.model_provider.is_none()
                        && let Some(provider) = &provider
                        && last_connection.as_ref() != Some(&provider.key())
                    {
                        headers.push((
                            index,
                            safe_text(&format!(
                                "{} · {}",
                                provider.models[0].provider_name,
                                self.provider_auth_label(provider)
                            )),
                        ));
                        last_connection = Some(provider.key());
                    }
                    let supports_discovery = provider
                        .as_ref()
                        .is_some_and(|provider| provider.supports_discovery());
                    if supports_discovery {
                        refresh_options.push(index);
                    }
                    row_hints.push(
                        if supports_discovery {
                            "p Connection · r Refresh · Enter apply · Esc back"
                        } else {
                            "p Connection · Enter apply · Esc back"
                        }
                        .into(),
                    );
                }
                row_hints.extend(
                    actions
                        .iter()
                        .map(|action| format!("Enter {} · Esc back", action.label())),
                );
                console.show_presented_picker_menu(
                    menu,
                    PickerKind::Model,
                    super::console::PickerPresentation {
                        headers,
                        refresh,
                        refresh_options: Some(refresh_options),
                        row_hints,
                        failure_target: selection.config_target().and_then(|target| target.agent()),
                        failure_connections: choices
                            .iter()
                            .map(|choice| Some(choice.config.provider_id.clone()))
                            .chain(actions.iter().map(|_| {
                                self.provider
                                    .model_provider
                                    .as_ref()
                                    .and_then(|key| self.connection(key).ok())
                                    .map(|provider| provider.models[0].provider_id.clone())
                            }))
                            .collect(),
                        connection_options: Some(choices.len()),
                        hint: "p Connection · Enter apply · Esc back".into(),
                        ..Default::default()
                    },
                )?;
            } else if matches!(
                &selection,
                Selection::Rewind { .. } | Selection::RewindActions { .. }
            ) {
                console.show_focused_menu(menu)?;
            } else {
                console.show_menu(menu)?;
            }
        }
        self.active = Some((id, selection));
        Ok(())
    }

    fn show_question(&mut self, console: &Console, flow: QuestionFlow) -> Result<(), String> {
        if self.prepare_dialog(console, Some(flow.request().id))? == RequestPresentation::Deferred {
            return Ok(());
        }
        let (menu, keys) = flow.menu(console.display_profile()?);
        let id = menu.id;
        console.show_question_menu(menu, keys)?;
        self.active = Some((id, Selection::Question(flow)));
        Ok(())
    }

    /// Moves the question dialog to the previous or next question.
    pub(super) fn switch_question(
        &mut self,
        console: &Console,
        menu_id: Uuid,
        next: bool,
    ) -> Result<(), String> {
        let result = match self.active.take() {
            Some((id, Selection::Question(mut flow))) if id == menu_id => {
                flow.switch(next);
                self.show_question(console, flow)
            }
            other => {
                self.active = other;
                Ok(())
            }
        };
        console.finish_menu_submission(menu_id)?;
        result
    }

    /// Answers the next unanswered question of `request` with typed text. Returns the lines
    /// that ask the following question, or `None` once every answer was submitted.
    pub(super) async fn answer_typed(
        &mut self,
        agent: &Agent,
        console: &Console,
        request: &QuestionRequest,
        text: &str,
    ) -> Result<Option<Vec<String>>, String> {
        if matches!(&self.active, Some((_, Selection::Question(flow))) if flow.request().id == request.id)
        {
            self.dismiss(console)?;
        }
        self.deferred_questions.insert(request.id);
        let mut flow = self
            .typed_questions
            .remove(&request.id)
            .unwrap_or_else(|| QuestionFlow::new(request.clone(), String::new()));
        let answers = match flow.answer_typed(text) {
            Ok(answers) => answers,
            Err(error) => {
                self.typed_questions.insert(request.id, flow);
                return Err(error);
            }
        };
        let Some(answers) = answers else {
            let lines = flow.next_question_lines();
            self.typed_questions.insert(request.id, flow);
            return Ok(Some(lines));
        };
        agent
            .answer_question(request.chat_id, request.id, answers)
            .await
            .map_err(|error| error.to_string())?;
        Ok(None)
    }

    fn show_commentable(
        &mut self,
        console: &Console,
        menu: Menu,
        selection: Selection,
        comment_options: Vec<usize>,
    ) -> Result<(), String> {
        if self.prepare_dialog(console, selection.request_id())? == RequestPresentation::Deferred {
            return Ok(());
        }
        if !console.is_interactive() {
            return self.show(console, menu, selection);
        }
        let id = menu.id;
        console.show_commentable_menu(menu, comment_options)?;
        self.active = Some((id, selection));
        Ok(())
    }

    fn show_expandable_commentable(
        &mut self,
        console: &Console,
        menu: Menu,
        selection: Selection,
        comment_options: Vec<usize>,
        expand_options: Vec<usize>,
    ) -> Result<(), String> {
        if self.prepare_dialog(console, selection.request_id())? == RequestPresentation::Deferred {
            return Ok(());
        }
        if !console.is_interactive() {
            return self.show(console, menu, selection);
        }
        let id = menu.id;
        console.show_expandable_commentable_menu(menu, comment_options, expand_options)?;
        self.active = Some((id, selection));
        Ok(())
    }

    fn show_focused_commentable(
        &mut self,
        console: &Console,
        menu: Menu,
        selection: Selection,
        comment_options: Vec<usize>,
    ) -> Result<(), String> {
        if self.prepare_dialog(console, selection.request_id())? == RequestPresentation::Deferred {
            return Ok(());
        }
        if !console.is_interactive() {
            return self.show(console, menu, selection);
        }
        let id = menu.id;
        console.show_focused_commentable_menu(menu, comment_options)?;
        self.active = Some((id, selection));
        Ok(())
    }

    fn show_reviewable(
        &mut self,
        console: &Console,
        mut menu: Menu,
        selection: Selection,
        comment_options: Vec<usize>,
        review: Result<tickloop_agent_tools::file_change::FileChangeReview, String>,
    ) -> Result<(), String> {
        if self.prepare_dialog(console, selection.request_id())? == RequestPresentation::Deferred {
            return Ok(());
        }
        if !console.is_interactive() {
            let viewport = DiffViewport {
                width: 120,
                height: usize::MAX,
            };
            let frame = match &review {
                Ok(review) => render_diff(review, &DiffNavigation::default(), viewport),
                Err(error) => render_error(
                    "Review unavailable",
                    error,
                    &DiffNavigation::default(),
                    viewport,
                ),
            };
            menu.context
                .extend(frame.lines.into_iter().map(|line| line.text));
            return self.show(console, menu, selection);
        }
        let id = menu.id;
        console.show_reviewable_menu(menu, comment_options, review)?;
        self.active = Some((id, selection));
        Ok(())
    }

    async fn load_providers(&mut self, agent: &Agent, target: ConfigTarget) -> Result<(), String> {
        let snapshot = agent
            .configuration(target)
            .await
            .map_err(|error| error.to_string())?;
        self.refresh_provider_scope(agent, target).await?;
        self.provider.provider_failures = match target {
            ConfigTarget::Agent(chat) => {
                agent
                    .snapshot(chat)
                    .await
                    .map_err(|error| error.to_string())?
                    .provider_failures
            }
            ConfigTarget::NewDraft => HashMap::new(),
        };
        let catalog = tickloop_agent_model::cards::ModelCatalog::defaults();
        let defaults = tickloop_agent_model::cards::ModelCatalog::defaults();
        let access = tickloop_agent_model::access::AccessCatalog::defaults();
        access.validate(&catalog)?;
        let profiles = catalog
            .profiles()
            .into_iter()
            .filter(|profile| {
                access
                    .ordinary_candidate(profile, &defaults)
                    .unwrap_or_else(|| tickloop_agent_model::cards::is_default_visible(profile))
            })
            .collect();
        let mut choices = settings::model_choices_with_profiles(&snapshot.model, profiles);
        if let Some(pending) = &snapshot.pending_model
            && !choices.contains(pending)
            && tickloop_agent_model::access::AccessCatalog::defaults().ordinary_candidate(
                pending,
                &tickloop_agent_model::cards::ModelCatalog::defaults(),
            ) != Some(false)
        {
            choices.push(pending.clone());
        }
        self.provider.provider_choices = ProviderModels::group_with_catalog(choices, &catalog);
        self.provider.current_connection = self.key_for_model(&snapshot.model);
        self.provider.current_model = Some((snapshot.model.provider_id, snapshot.model.model_id));
        self.provider.provider_selected = self
            .provider
            .provider_choices
            .iter()
            .position(|provider| Some(&provider.key()) == self.provider.current_connection.as_ref())
            .unwrap_or(0);
        self.provider.provider_nav.focused = self.provider.current_connection.clone();
        Ok(())
    }

    pub(super) async fn models(
        &mut self,
        agent: &Agent,
        console: &Console,
        target: ConfigTarget,
    ) -> Result<(), String> {
        self.open_models(agent, console, target, false).await
    }

    async fn open_models(
        &mut self,
        agent: &Agent,
        console: &Console,
        target: ConfigTarget,
        return_to_settings: bool,
    ) -> Result<(), String> {
        self.sync_dialog_chat(console)?;
        self.provider.provider_nav = Default::default();
        self.load_providers(agent, target).await?;
        self.provider.model_return = Some(providers::ModelPickerDraft {
            provider: None,
            selected: self.provider.current_model.clone(),
            action: None,
            choices: Vec::new(),
        });
        if return_to_settings {
            self.provider
                .provider_nav
                .back_stack
                .push(providers::ProviderPage::Settings);
        }
        self.configured_models(agent, console, target).await
    }

    pub(super) async fn providers(
        &mut self,
        agent: &Agent,
        console: &Console,
        target: ConfigTarget,
    ) -> Result<(), String> {
        self.sync_dialog_chat(console)?;
        self.provider.model_return = None;
        self.provider.provider_nav = Default::default();
        self.load_providers(agent, target).await?;
        self.back_to_providers(console, target)
    }

    pub(super) async fn efforts(
        &mut self,
        agent: &Agent,
        console: &Console,
        target: ConfigTarget,
    ) -> Result<(), String> {
        self.show_efforts(agent, console, target, false).await
    }

    async fn show_efforts(
        &mut self,
        agent: &Agent,
        console: &Console,
        target: ConfigTarget,
        return_to_settings: bool,
    ) -> Result<(), String> {
        let snapshot = agent
            .configuration(target)
            .await
            .map_err(|error| error.to_string())?;
        let choices = if tickloop_agent_model::validate_reasoning_effort(
            &snapshot.model,
            Some(ReasoningEffort::On),
        )
        .is_ok()
        {
            providers::thinking_choices(&snapshot.model)
        } else {
            std::iter::once(None)
                .chain(
                    ReasoningEffort::choices(&snapshot.model)
                        .iter()
                        .copied()
                        .map(Some),
                )
                .collect::<Vec<_>>()
        };
        if choices.len() == 1 {
            return Err(format!(
                "{} does not support configurable Thinking",
                safe_text(&snapshot.model.model)
            ));
        }
        let options = choices
            .iter()
            .map(|choice| {
                let label = match choice {
                    None => "auto · model default",
                    Some(effort) => effort.label(),
                };
                let marker = if *choice == snapshot.reasoning_effort {
                    " · current"
                } else {
                    ""
                };
                format!("{label}{marker}")
            })
            .collect();
        self.show(
            console,
            Menu {
                id: Uuid::new_v4(),
                title: "Thinking level".into(),
                context: vec![
                    "Levels depend on the current model; auto uses its default. Use Settings to turn Thinking on or off. The choice is saved in user settings.".into(),
                    "Thinking is collapsed in conversation; Ctrl+O opens the full archive.".into(),
                ],
                options,
                selected: choices
                    .iter()
                    .position(|choice| *choice == snapshot.reasoning_effort)
                    .unwrap_or(0),
                confirmation: false,
            },
            Selection::Effort { target, choices, return_to_settings },
        )
    }

    pub(super) async fn settings(
        &mut self,
        agent: &Agent,
        console: &Console,
        target: ConfigTarget,
    ) -> Result<(), String> {
        self.sync_dialog_chat(console)?;
        let selected = self
            .settings_positions
            .get(&target.agent())
            .map_or(0, |position| position.selected);
        let snapshot = agent
            .configuration(target)
            .await
            .map_err(|error| error.to_string())?;
        let mut context = vec![
            "Model, Thinking, effort, persistent rules, output style, Glyph Set, reduced motion and prompt suggestions are saved in user settings; execution mode applies to this process.".into(),
            "Auto-scroll and Theme apply to this terminal session.".into(),
            safe_text(&format!(
                "Current model: {}",
                settings::model_label(&snapshot.model)
            )),
            format!("Execution mode: {}", snapshot.execution_mode),
        ];
        if let Some(pending) = snapshot.pending_model {
            context.push(safe_text(&format!(
                "Pending model: {}",
                settings::model_label(&pending)
            )));
        }
        let artifact_account = if let Some(root) = &self.config_root {
            match tickloop_agent_artifacts::ArtifactClient::new(root) {
                Ok(client) => match client.status().await {
                    Ok(Some(user)) => {
                        format!("{} · Sign out", super::safe_text(user.display_name()))
                    }
                    Ok(None) => "signed out · /artifacts to sign in".into(),
                    Err(error) => format!("{} · /artifacts to retry", error),
                },
                Err(error) => error.to_string(),
            }
        } else {
            "signed out · /artifacts to sign in".into()
        };
        self.show(
            console,
            Menu {
                id: Uuid::new_v4(),
                title: "Settings".into(),
                context,
                options: vec![
                    "Model".into(),
                    format!(
                        "Thinking: {}",
                        match tickloop_agent_model::cards::model_card(&snapshot.model)
                            .map(|card| card.thinking)
                        {
                            Some(tickloop_agent_protocol::ThinkingControl::Toggle) => {
                                if snapshot
                                    .model
                                    .thinking_enabled(
                                        snapshot.reasoning_effort,
                                        snapshot.thinking_enabled,
                                    )
                                    .unwrap_or(false)
                                {
                                    "On"
                                } else {
                                    "Off"
                                }
                            }
                            Some(tickloop_agent_protocol::ThinkingControl::AlwaysOn) => "Always on",
                            Some(tickloop_agent_protocol::ThinkingControl::Unsupported) | None =>
                                "Model default",
                        }
                    ),
                    format!(
                        "Thinking: {}",
                        snapshot
                            .reasoning_effort
                            .map_or("auto", ReasoningEffort::label)
                    ),
                    if snapshot.delegated {
                        format!(
                            "Execution mode: {} · fixed by parent",
                            snapshot.execution_mode
                        )
                    } else {
                        "Execution mode".into()
                    },
                    "Session status".into(),
                    "Usage".into(),
                    format!(
                        "Auto-scroll: {}",
                        if console.auto_scroll()? { "On" } else { "Off" }
                    ),
                    format!("Theme: {}", console.theme()?.label()),
                    format!(
                        "Glyph Set: {}",
                        console.display_preferences()?.glyph_set.label()
                    ),
                    format!(
                        "Reduce motion: {}",
                        if console.display_preferences()?.prefers_reduced_motion {
                            "On"
                        } else {
                            "Off"
                        }
                    ),
                    format!("Output style: {}", self.saved_output_style().label()),
                    "Permissions".into(),
                    format!("Artifact Service: {artifact_account}"),
                    format!(
                        "Prompt suggestions: {}",
                        if console.prompt_suggestion_enabled()? {
                            "On"
                        } else {
                            "Off"
                        }
                    ),
                ],
                selected,
                confirmation: false,
            },
            Selection::Settings { target },
        )
    }

    fn saved_output_style(&self) -> OutputStyle {
        self.user_settings
            .as_ref()
            .map_or_else(OutputStyle::default, UserSettings::output_style)
    }

    fn output_styles(&mut self, console: &Console, target: ConfigTarget) -> Result<(), String> {
        let current = self.saved_output_style();
        let options = OutputStyle::ALL
            .into_iter()
            .map(|style| {
                let option = format!("{} · {}", style.label(), style.description());
                if style == current {
                    format!("{option} · current")
                } else {
                    option
                }
            })
            .collect();
        self.show(
            console,
            Menu {
                id: Uuid::new_v4(),
                title: "Preferred output style".into(),
                context: vec![
                    "This changes how the Agent communicates with you.".into(),
                    "Applies to Chats opened after the change; open Chats keep their style.".into(),
                ],
                options,
                selected: OutputStyle::ALL
                    .into_iter()
                    .position(|style| style == current)
                    .unwrap_or(0),
                confirmation: false,
            },
            Selection::OutputStyle { target },
        )
    }

    pub(super) async fn glyph_sets(
        &mut self,
        console: &Console,
        target: ConfigTarget,
    ) -> Result<(), String> {
        self.glyph_sets_with_focus(console, target, None).await
    }

    async fn glyph_sets_with_focus(
        &mut self,
        console: &Console,
        target: ConfigTarget,
        selected: Option<usize>,
    ) -> Result<(), String> {
        let current = console.display_preferences()?.glyph_set;
        let unicode = DisplayProfile::new(DisplayPreferences::default()).preview();
        let nerd_font = DisplayProfile::new(DisplayPreferences {
            glyph_set: GlyphSet::NerdFont,
            prefers_reduced_motion: false,
        })
        .preview();
        let mut options = GlyphSet::ALL
            .into_iter()
            .map(|glyph_set| {
                if glyph_set == current {
                    format!("Use {} · current", glyph_set.label())
                } else {
                    format!("Use {}", glyph_set.label())
                }
            })
            .collect::<Vec<_>>();
        let (font_status, font_action) = match &self.font_manager {
            Some(manager) => {
                let status = manager.status().await.map_err(|error| error.to_string())?;
                let action = if status.can_install() {
                    Some(FontAction::Install)
                } else if status.can_uninstall() {
                    Some(FontAction::Uninstall)
                } else {
                    None
                };
                (status.label().to_owned(), action)
            }
            None => (
                "Automatic installation is unavailable because the settings directory is unknown"
                    .into(),
                None,
            ),
        };
        if let Some(action) = font_action {
            options.push(match action {
                FontAction::Install => "Install Symbols-only Nerd Font…".into(),
                FontAction::Uninstall => "Uninstall Tick Loop-owned Symbols-only Nerd Font…".into(),
            });
        }
        self.show(
            console,
            Menu {
                id: Uuid::new_v4(),
                title: "Choose Glyph Set".into(),
                context: vec![
                    format!("Unicode preview: {unicode}"),
                    format!("Nerd Font preview: {nerd_font}"),
                    format!("Font status: {font_status}"),
                    "Choose only after the preview is aligned. Missing glyphs or boxes mean keep Unicode.".into(),
                ],
                options,
                selected: selected.unwrap_or_else(|| GlyphSet::ALL
                    .into_iter()
                    .position(|glyph_set| glyph_set == current)
                    .unwrap_or(0)),
                confirmation: false,
            },
            Selection::GlyphSet { target, font_action },
        )
    }

    fn confirm_glyph_set(
        &mut self,
        console: &Console,
        target: ConfigTarget,
        glyph_set: GlyphSet,
    ) -> Result<(), String> {
        self.show(
            console,
            Menu {
                id: Uuid::new_v4(),
                title: "Confirm glyph set".into(),
                context: vec![format!(
                    "Save {} for future Tick Loop sessions?",
                    glyph_set.label()
                )],
                options: vec!["Confirm and save".into(), "Back to preview".into()],
                selected: 0,
                confirmation: false,
            },
            Selection::GlyphSetConfirmation { target, glyph_set },
        )
    }

    fn confirm_font_action(
        &mut self,
        console: &Console,
        target: ConfigTarget,
        action: FontAction,
    ) -> Result<(), String> {
        let (title, context) = match action {
            FontAction::Install => (
                "Confirm Nerd Font installation",
                "Tick Loop will use Homebrew when available; otherwise it downloads the fixed official v3.5.1 Symbols-only asset and verifies SHA-256. This does not enable Nerd Font.",
            ),
            FontAction::Uninstall => (
                "Confirm Nerd Font uninstall",
                "Tick Loop will switch to Unicode first and remove only the installation recorded in its ownership receipt.",
            ),
        };
        self.show(
            console,
            Menu {
                id: Uuid::new_v4(),
                title: title.into(),
                context: vec![context.into()],
                options: vec!["Confirm".into(), "Back to preview".into()],
                selected: 1,
                confirmation: false,
            },
            Selection::FontActionConfirmation { target, action },
        )
    }

    fn save_display_preferences(
        &self,
        console: &Console,
        preferences: DisplayPreferences,
    ) -> Result<(), String> {
        let settings = self.user_settings.as_ref().ok_or(
            "cannot save display preferences because the user settings directory is unavailable",
        )?;
        settings
            .save_display_preferences(preferences)
            .map_err(|error| error.to_string())?;
        console.set_display_preferences(preferences)
    }

    pub(super) fn themes(
        &mut self,
        console: &Console,
        settings_target: Option<ConfigTarget>,
    ) -> Result<(), String> {
        let current = console.theme()?;
        let options = ThemeName::ALL
            .into_iter()
            .map(|theme| {
                if theme == current {
                    format!("{} · current", theme.label())
                } else {
                    theme.label().to_owned()
                }
            })
            .collect();
        let selected = ThemeName::ALL
            .into_iter()
            .position(|theme| theme == current)
            .unwrap_or(0);
        self.show(
            console,
            Menu {
                id: Uuid::new_v4(),
                title: "Choose theme".into(),
                context: vec![
                    "Applies to this terminal session. Colours only; layout and text are unchanged."
                        .into(),
                ],
                options,
                selected,
                confirmation: false,
            },
            Selection::Theme { settings_target },
        )
    }

    async fn settings_info(
        &mut self,
        agent: &Agent,
        console: &Console,
        target: ConfigTarget,
        usage: bool,
    ) -> Result<(), String> {
        let ConfigTarget::Agent(chat) = target else {
            let configuration = agent
                .configuration(target)
                .await
                .map_err(|error| error.to_string())?;
            let context = if usage {
                vec!["No usage yet; the New Agent has not sent a request.".into()]
            } else {
                vec![
                    "New Agent · created when you send the first input".into(),
                    safe_text(&format!(
                        "Model: {}",
                        settings::model_label(&configuration.model)
                    )),
                    safe_text(&format!("Service: {}", configuration.model.base_url)),
                    format!("Execution mode: {}", configuration.execution_mode),
                ]
            };
            return self.show(
                console,
                Menu {
                    id: Uuid::new_v4(),
                    title: if usage { "Usage" } else { "Session status" }.into(),
                    context,
                    options: vec!["Back to settings".into()],
                    selected: 0,
                    confirmation: false,
                },
                Selection::SettingsInfo { target },
            );
        };
        let snapshot = agent
            .snapshot(chat)
            .await
            .map_err(|error| error.to_string())?;
        let context = if usage {
            recorded_usage_context(&snapshot.request_usage.summary())
        } else {
            vec![
                format!("Chat: {}", snapshot.id),
                safe_text(&format!(
                    "Model: {}",
                    settings::model_label(&snapshot.model)
                )),
                safe_text(&format!("Service: {}", snapshot.model.base_url)),
                safe_text(&format!(
                    "Working directory: {}",
                    snapshot.workdir.display()
                )),
                format!("Execution mode: {}", snapshot.policy.mode),
                format!(
                    "Goals: {} · pending permissions: {} · questions: {}",
                    snapshot.loops.len(),
                    snapshot.permissions.len(),
                    snapshot.questions.len()
                ),
                "Opening history never resumes work automatically.".into(),
            ]
        };
        self.show(
            console,
            Menu {
                id: Uuid::new_v4(),
                title: if usage { "Usage" } else { "Session status" }.into(),
                context,
                options: vec!["Back to settings".into()],
                selected: 0,
                confirmation: false,
            },
            Selection::SettingsInfo { target },
        )
    }

    pub(super) async fn modes(
        &mut self,
        agent: &Agent,
        console: &Console,
        target: ConfigTarget,
    ) -> Result<(), String> {
        let configuration = agent
            .configuration(target)
            .await
            .map_err(|error| error.to_string())?;
        let current = configuration.execution_mode;
        if let Some(reason) =
            configuration.unavailable_change(&ConfigurationChange::ExecutionMode { mode: current })
        {
            console.write(&format!(
                "Execution mode {current} is fixed here: {reason}.\n"
            ))?;
            return self.settings(agent, console, target).await;
        }
        let modes = settings::execution_modes();
        let selected = modes
            .iter()
            .position(|(mode, _)| *mode == current)
            .unwrap_or(0);
        let (choices, options) = modes.into_iter().unzip();
        self.show(
            console,
            Menu {
                id: Uuid::new_v4(),
                title: "Choose execution mode".into(),
                context: vec!["Applies to this Chat. Explicit deny rules remain effective.".into()],
                options,
                selected,
                confirmation: false,
            },
            Selection::Mode { target, choices },
        )
    }

    pub(super) async fn permission_modes(
        &mut self,
        agent: &Agent,
        console: &Console,
        target: ConfigTarget,
        return_to_settings: bool,
    ) -> Result<(), String> {
        let configuration = agent
            .configuration(target)
            .await
            .map_err(|error| error.to_string())?;
        let current = configuration.execution_mode;
        let fixed =
            configuration.unavailable_change(&ConfigurationChange::ExecutionMode { mode: current });
        let modes = settings::execution_modes()
            .into_iter()
            .filter(|(mode, _)| fixed.is_none() || *mode == current)
            .collect::<Vec<_>>();
        let selected = modes
            .iter()
            .position(|(mode, _)| *mode == current)
            .unwrap_or(0);
        let (choices, labels): (Vec<_>, Vec<_>) = modes.into_iter().unzip();
        let mut options: Vec<String> = labels
            .into_iter()
            .zip(&choices)
            .map(|(label, mode)| {
                if *mode == current {
                    format!("{label} · Current")
                } else {
                    label
                }
            })
            .collect();
        let fixed_context = fixed.map(|reason| {
            format!("Mode is fixed for this child: {reason}. Rules and activity are read-only.")
        });
        let context = if let Some(context) = fixed_context.as_deref() {
            context
        } else if current == ExecutionMode::Plan {
            "Current: Plan. Choose a permission mode to leave Plan, or view activity."
        } else if target == ConfigTarget::NewDraft {
            "Choose a mode for the New Agent, or view its permissions without changing it."
        } else {
            "Choose a mode for this Chat, or view activity without changing it."
        };
        options.push("↗ View recent activity · keep current mode".into());
        self.show(
            console,
            Menu {
                id: Uuid::new_v4(),
                title: "Permissions · Choose mode".into(),
                context: vec![context.into()],
                options,
                selected,
                confirmation: false,
            },
            Selection::PermissionMode {
                target,
                choices,
                current,
                return_to_settings,
            },
        )
    }

    pub(super) async fn refresh_family_permissions(
        &mut self,
        agent: &Agent,
        console: &Console,
        family: &[Uuid],
        pending: &HashMap<Uuid, super::PermissionRequestOwner>,
        questions: &HashMap<Uuid, QuestionRequest>,
    ) -> Result<(), String> {
        if !console.is_interactive() {
            return Ok(());
        }
        self.sync_dialog_chat(console)?;
        for chat in family {
            if let Some((execution, plans)) = agent.cached_pending_plan_approvals(*chat) {
                self.restored_plans.retain(|_, request| {
                    request.chat != *chat || plans.contains(&(request.loop_id, request.version))
                });
                if self.scanned_plan_executions.get(chat) == Some(&execution) {
                    continue;
                }
                self.scanned_plan_executions.insert(*chat, execution);
                self.restored_plans
                    .retain(|_, request| request.chat != *chat);
                for (goal, version) in plans {
                    let request = Uuid::new_v4();
                    self.restored_plans.insert(
                        request,
                        RestoredPlanApproval {
                            chat: *chat,
                            loop_id: goal,
                            version,
                        },
                    );
                    self.note_request(*chat, request);
                }
            }
        }
        self.request_order.retain(|(_, id)| {
            pending.contains_key(id)
                || questions.contains_key(id)
                || self.restored_plans.contains_key(id)
        });
        if let Some((
            _,
            Selection::Workflow(Workflow::Plan {
                chat,
                goal,
                version,
                ..
            }),
        )) = &self.active
            && family.contains(chat)
            && agent.cached_plan_is_pending(*chat, *goal, *version) == Some(false)
        {
            let request = self
                .active
                .as_ref()
                .and_then(|(_, selection)| selection.request_id());
            self.request_order.retain(|(_, id)| Some(*id) != request);
            self.dismiss(console)?;
        }
        let queued = self
            .request_order
            .iter()
            .filter(|(chat, id)| family.contains(chat) && !self.deferred_questions.contains(id))
            .count();
        let active_unqueued = usize::from(
            self.active
                .as_ref()
                .and_then(|(_, selection)| {
                    selection.request_id().filter(|id| {
                        pending.contains_key(id)
                            || questions.contains_key(id)
                            || self.restored_plans.contains_key(id)
                            || matches!(
                                selection,
                                Selection::Workflow(Workflow::Plan { request: None, .. })
                            )
                    })
                })
                .is_some_and(|id| !self.request_order.iter().any(|(_, queued)| *queued == id)),
        );
        console.set_request_queue_count(queued + active_unqueued)?;
        if let Some((_, selection)) = &self.active
            && selection.request_id().is_some()
        {
            if let Some(requester) = selection
                .chat()
                .filter(|requester| family.contains(requester))
            {
                return self
                    .refresh_permissions(agent, console, Some(requester), pending, questions)
                    .await;
            }
            self.dismiss(console)?;
        }
        // Only an interactive View presents requests, and only ones already tracked from their
        // events; otherwise every family snapshot below would be read for nothing, once per event.
        if !console.is_interactive()
            || pending.is_empty() && questions.is_empty() && self.restored_plans.is_empty()
            || !console.can_present_request()?
        {
            return Ok(());
        }
        for requester in family {
            let snapshot = agent
                .snapshot(*requester)
                .await
                .map_err(|error| error.to_string())?;
            for event in &snapshot.events {
                match &event.kind {
                    EventKind::PermissionRequested { request }
                        if pending.contains_key(&request.id) =>
                    {
                        self.note_request(*requester, request.id)
                    }
                    EventKind::QuestionAsked { request } if questions.contains_key(&request.id) => {
                        self.note_request(*requester, request.id)
                    }
                    _ => {}
                }
            }
        }
        let queued = self
            .request_order
            .iter()
            .filter(|(chat, id)| family.contains(chat) && !self.deferred_questions.contains(id))
            .copied()
            .collect::<Vec<_>>();
        console.set_request_queue_count(queued.len())?;
        if let Some((requester, _)) = queued.first() {
            return self
                .refresh_permissions(agent, console, Some(*requester), pending, questions)
                .await;
        }
        Ok(())
    }

    pub(super) async fn refresh_permissions(
        &mut self,
        agent: &Agent,
        console: &Console,
        chat: Option<Uuid>,
        pending: &HashMap<Uuid, super::PermissionRequestOwner>,
        questions: &HashMap<Uuid, QuestionRequest>,
    ) -> Result<(), String> {
        self.deferred_questions
            .retain(|id| questions.contains_key(id));
        self.typed_questions
            .retain(|id, _| questions.contains_key(id));
        if let Some((_, selection)) = &self.active {
            let stale = selection.request_id().is_some()
                && selection.chat().is_some()
                && selection.chat() != chat
                || matches!(selection, Selection::Permission { request, .. } | Selection::PermissionRule { request, .. } if !pending.contains_key(&request.id))
                || matches!(selection, Selection::Question(flow) if !questions.contains_key(&flow.request().id));
            let stale = stale
                || matches!(selection, Selection::Workflow(Workflow::Plan { request: Some(request), .. }) if !pending.contains_key(request) && !self.restored_plans.contains_key(request));
            if stale {
                self.dismiss(console)?;
            }
        }
        if !console.is_interactive()
            || self
                .active
                .as_ref()
                .is_some_and(|(_, selection)| selection.request_id().is_some())
            || pending.is_empty() && questions.is_empty() && self.restored_plans.is_empty()
        {
            return Ok(());
        }
        if let Some(chat) = chat {
            let snapshot = agent
                .snapshot(chat)
                .await
                .map_err(|error| error.to_string())?;
            let next_request = self
                .request_order
                .iter()
                .find(|(requester, id)| *requester == chat && !self.deferred_questions.contains(id))
                .map(|(_, id)| *id);
            let requester = requester_label(agent, console, &snapshot).await;
            let restored_request = self
                .restored_plans
                .iter()
                .find(|(id, request)| {
                    request.chat == chat && next_request.is_none_or(|next| next == **id)
                })
                .map(|(id, request)| (*id, request.loop_id));
            if let Some((request, goal)) = restored_request {
                let (mut menu, mut workflow) = Workflow::plan(agent, chat, goal).await?;
                if let Workflow::Plan {
                    request: current, ..
                } = &mut workflow
                {
                    *current = Some(request);
                }
                if console.dialog_chat()? != Some(chat) {
                    menu.title = format!("Plan approval · {requester}");
                }
                return self.workflow(console, (menu, workflow));
            }
            if let Some(request) = snapshot.permissions.iter().find(|request| {
                pending.contains_key(&request.id) && next_request.is_none_or(|id| id == request.id)
            }) {
                if request.name == tickloop_agent_tools::EXIT_PLAN_MODE_TOOL_NAME {
                    let (mut menu, workflow) = Workflow::plan(agent, chat, request.loop_id).await?;
                    if console.dialog_chat()? != Some(chat) {
                        menu.title = format!("Plan approval · {requester}");
                    }
                    return self.workflow(console, (menu, workflow));
                }
                let goal = snapshot
                    .loops
                    .iter()
                    .find(|goal| goal.id == request.loop_id);
                let context = safe_text(&format!(
                    "{requester} · Goal: {}",
                    goal.map(|goal| goal.goal.as_str()).unwrap_or("Task"),
                ));
                self.permission(
                    console,
                    chat,
                    request,
                    context,
                    snapshot.delegation.is_none(),
                )?;
            } else if let Some(request) = snapshot.questions.iter().find(|request| {
                !self.deferred_questions.contains(&request.id)
                    && questions.contains_key(&request.id)
                    && next_request.is_none_or(|id| id == request.id)
            }) {
                let goal = snapshot
                    .loops
                    .iter()
                    .find(|goal| goal.id == request.loop_id)
                    .map(|goal| goal.goal.as_str())
                    .unwrap_or("Task");
                self.show_question(
                    console,
                    QuestionFlow::new(request.clone(), format!("{requester} · {goal}")),
                )?;
            }
        }
        Ok(())
    }

    fn permission(
        &mut self,
        console: &Console,
        chat: Uuid,
        request: &PermissionRequest,
        goal: String,
        persistent: bool,
    ) -> Result<(), String> {
        if request.name == ENTER_PLAN_MODE_TOOL_NAME {
            let menu = enter_plan_mode_menu(request.id, goal.clone());
            let selection = Selection::Permission {
                chat,
                request: request.clone(),
                goal,
                persistent: false,
            };
            return self.show(console, menu, selection);
        }
        let review = request.file_change.as_deref().map(|prepared| {
            load_file_change_review(&prepared.artifact)
                .map_err(|error| error.to_string())
                .and_then(|review| {
                    if review.summary() == prepared.summary {
                        Ok(review)
                    } else {
                        Err("file change review does not match the permission request".into())
                    }
                })
        });
        let mut context = vec![goal.clone()];
        if review.is_none() {
            context.extend(permission_context(request));
        }
        let explicit_ask = request.approval_source == PermissionApprovalSource::ExplicitAsk;
        let rule_candidates = permission_rule_candidates(&request.name, &request.input);
        let persistent_option = rule_candidates.first().map_or_else(
            || "Yes, and don't ask again".into(),
            |candidate| format!("Yes, and don't ask again for: {}", candidate.label),
        );
        let options = if explicit_ask {
            vec![
                "Yes, once".into(),
                "No, and tell Claude what to do differently (esc)".into(),
            ]
        } else if persistent {
            vec![
                "Yes".into(),
                "Yes, for this session".into(),
                persistent_option,
                "No, and tell Claude what to do differently (esc)".into(),
            ]
        } else {
            vec![
                "Yes".into(),
                "Yes, for this session".into(),
                "No, and tell Claude what to do differently (esc)".into(),
            ]
        };
        let menu = Menu {
            id: request.id,
            title: format!(
                "Permission required · {}",
                super::presentation::tool_name(&request.name)
            ),
            context,
            options,
            selected: 0,
            confirmation: true,
        };
        let selection = Selection::Permission {
            chat,
            request: request.clone(),
            goal,
            persistent,
        };
        let denial_index = match (explicit_ask, persistent) {
            (true, _) => 1,
            (false, true) => 3,
            (false, false) => 2,
        };
        let has_prefix_choices = !explicit_ask && persistent && rule_candidates.len() > 1;
        match review {
            Some(review) => {
                self.show_reviewable(console, menu, selection, vec![0, denial_index], review)
            }
            None if has_prefix_choices => {
                self.show_expandable_commentable(console, menu, selection, vec![0, 3], vec![2])
            }
            None => self.show_commentable(console, menu, selection, vec![0, denial_index]),
        }
    }

    pub(super) fn expand(
        &mut self,
        console: &Console,
        menu_id: Uuid,
        index: usize,
    ) -> Result<(), String> {
        let Some((
            active_id,
            Selection::Permission {
                chat,
                request,
                goal,
                persistent,
            },
        )) = self.active.as_ref()
        else {
            return Err("this menu option cannot be expanded".into());
        };
        if *active_id != menu_id
            || index != 2
            || !*persistent
            || request.approval_source == PermissionApprovalSource::ExplicitAsk
        {
            return Err("this menu option cannot be expanded".into());
        }
        let chat = *chat;
        let request = request.clone();
        let goal = goal.clone();
        let candidates = permission_rule_candidates(&request.name, &request.input);
        if candidates.len() < 2 {
            return Err("this command has no broader prefix rules".into());
        }
        let mut options = candidates
            .iter()
            .map(|candidate| candidate.label.clone())
            .collect::<Vec<_>>();
        options.push("Back".into());
        self.show(
            console,
            Menu {
                id: Uuid::new_v4(),
                title: "Add permission rule".into(),
                context: vec![
                    "Choose the command rule to allow for this project.".into(),
                    "The first rule is exact; * matches the remaining command text, with broader rules lower down."
                        .into(),
                ],
                options,
                selected: 0,
                confirmation: true,
            },
            Selection::PermissionRule {
                chat,
                request,
                goal,
                candidates,
            },
        )
    }

    pub(super) async fn choose(
        &mut self,
        agent: &Agent,
        console: &Console,
        menu_id: Uuid,
        index: Option<usize>,
        comment: Option<String>,
    ) -> Result<MenuOutcome, String> {
        self.sync_dialog_chat(console)?;
        let menu_id = match self.provider.refreshed_picker_cancel {
            Some((previous, current))
                if index.is_none() && menu_id == previous && self.id() == Some(current) =>
            {
                self.provider.refreshed_picker_cancel = None;
                current
            }
            _ => menu_id,
        };
        if self.id() != Some(menu_id) {
            console.finish_menu_submission(menu_id)?;
            return Ok(MenuOutcome::default());
        }
        if !matches!(&self.active, Some((_, Selection::Model { choices, .. })) if index.is_some_and(|index| index < choices.len()))
        {
            self.cancel_model_discovery(menu_id);
        }
        let navigation_before = self.provider.provider_nav.clone();
        let model_provider_before = self.provider.model_provider.clone();
        let (_, selection) = self.active.take().unwrap();
        if let Selection::Settings { target } = &selection
            && let Some(index) = index
        {
            self.settings_positions.insert(
                target.agent(),
                SettingsPosition {
                    selected: index,
                    context_offset: console.picker_context_offset(menu_id)?.unwrap_or_default(),
                },
            );
        }
        let retry = if let Some(selection) = selection.configuration_retry() {
            console
                .menu_snapshot(menu_id)?
                .map(|menu| (selection, menu))
        } else {
            None
        };
        let outcome: Result<MenuOutcome, String> = async {
            match selection {
                Selection::Locations { generation, views } => {
                    if let Some(index) = index {
                        let view = views.get(index).ok_or("location no longer exists")?.view;
                        return Ok(MenuOutcome { command: Some(format!("/agent-location {generation} {} {}", view.session_id, view.view_id)), ..Default::default() });
                    }
                }
                Selection::HelpReference => self.help(console, None)?,
                Selection::Help { section } => {
                    if let Some(index) = index {
                        if let Some(section) = section {
                            let group = *COMMAND_GROUPS
                                .get(section)
                                .ok_or("command group does not exist")?;
                            let command = commands_in_group(group)
                                .nth(index)
                                .ok_or("command does not exist")?;
                            return Ok(MenuOutcome {
                                command: Some(command.command.into()),
                                ..Default::default()
                            });
                        }
                        self.help(console, Some(index))?;
                    } else if section.is_some() {
                        self.help(console, None)?;
                    }
                }
                Selection::Permission { chat, request, .. }
                    if request.name == ENTER_PLAN_MODE_TOOL_NAME =>
                {
                    let allow = match index {
                        Some(0) => true,
                        Some(1) | None => false,
                        _ => return Err("permission option does not exist".into()),
                    };
                    agent
                        .answer_permission_with_comment(
                            chat,
                            request.id,
                            allow,
                            PermissionScope::Once,
                            comment,
                        )
                        .await
                        .map_err(|error| error.to_string())?;
                    let message = if allow {
                        format!(
                            "{} Entering plan mode. Continuing…\n",
                            console.display_profile()?.glyph(Glyph::Success)
                        )
                    } else {
                        "Staying in the current mode. Add instructions to continue.\n".into()
                    };
                    console.write(&message)?;
                    return Ok(MenuOutcome {
                        permission: Some(request.id),
                        ..Default::default()
                    });
                }
                Selection::Permission {
                    chat,
                    request,
                    persistent,
                    ..
                } => {
                    let has_comment = comment
                        .as_ref()
                        .is_some_and(|comment| !comment.trim().is_empty());
                    let explicit_ask =
                        request.approval_source == PermissionApprovalSource::ExplicitAsk;
                    if !explicit_ask
                        && persistent
                        && index == Some(2)
                        && let Some(exact) =
                            permission_rule_candidates(&request.name, &request.input).first()
                    {
                        agent
                            .answer_permission_with_rule(
                                chat,
                                request.id,
                                exact.specifier.clone(),
                            )
                            .await
                            .map_err(|error| error.to_string())?;
                        console.write(&format!(
                            "{} Permission rule added: {}. Continuing…\n",
                            console.display_profile()?.glyph(Glyph::Success),
                            exact.label
                        ))?;
                        return Ok(MenuOutcome {
                            permission: Some(request.id),
                            ..Default::default()
                        });
                    }
                    let (allow, scope) = if explicit_ask {
                        match index {
                            Some(0) => (true, PermissionScope::Once),
                            Some(1) | None => (false, PermissionScope::Once),
                            _ => return Err("permission option does not exist".into()),
                        }
                    } else {
                        match (index, persistent) {
                            (Some(0), _) => (true, PermissionScope::Once),
                            (Some(1), _) => (true, PermissionScope::Session),
                            (Some(2), true) => (true, PermissionScope::Persistent),
                            (Some(3), true) | (Some(2), false) | (None, _) => {
                                (false, PermissionScope::Once)
                            }
                            _ => return Err("permission option does not exist".into()),
                        }
                    };
                    agent
                        .answer_permission_with_comment(chat, request.id, allow, scope, comment)
                        .await
                        .map_err(|error| error.to_string())?;
                    let message = match (allow, has_comment) {
                        (true, true) => format!(
                            "{} Permission accepted with your comment. Continuing…\n",
                            console.display_profile()?.glyph(Glyph::Success)
                        ),
                        (true, false) => format!(
                            "{} Permission accepted. Continuing…\n",
                            console.display_profile()?.glyph(Glyph::Success)
                        ),
                        (false, true) => "Operation denied with your comment. Continuing…\n".into(),
                        (false, false) => {
                            "Operation denied. Add instructions to continue.\n".into()
                        }
                    };
                    console.write(&message)?;
                    return Ok(MenuOutcome {
                        permission: Some(request.id),
                        ..Default::default()
                    });
                }
                Selection::PermissionRule {
                    chat,
                    request,
                    goal,
                    candidates,
                } => {
                    let Some(index) = index else {
                        self.permission(console, chat, &request, goal, true)?;
                        return Ok(MenuOutcome::default());
                    };
                    let Some(candidate) = candidates.get(index) else {
                        if index == candidates.len() {
                            self.permission(console, chat, &request, goal, true)?;
                            return Ok(MenuOutcome::default());
                        }
                        return Err("permission rule option does not exist".into());
                    };
                    agent
                        .answer_permission_with_rule(
                            chat,
                            request.id,
                            candidate.specifier.clone(),
                        )
                        .await
                        .map_err(|error| error.to_string())?;
                    console.write(&format!(
                        "{} Permission rule added: {}. Continuing…\n",
                        console.display_profile()?.glyph(Glyph::Success),
                        candidate.label
                    ))?;
                    return Ok(MenuOutcome {
                        permission: Some(request.id),
                        ..Default::default()
                    });
                }
                Selection::Question(mut flow) => match flow.choose(index, comment) {
                    FlowUpdate::Show => self.show_question(console, flow)?,
                    FlowUpdate::Submit(answers) => {
                        let request = flow.request();
                        agent
                            .answer_question(request.chat_id, request.id, answers)
                            .await
                            .map_err(|error| error.to_string())?;
                        console.write("Answer accepted. Continuing…\n")?;
                        return Ok(MenuOutcome {
                            question: Some(request.id),
                            ..Default::default()
                        });
                    }
                    FlowUpdate::Defer => {
                        let request = flow.request();
                        self.deferred_questions.insert(request.id);
                        let mut message =
                            "Enter your answer below. The question is still waiting.\n".to_owned();
                        for line in question_lines(&request.questions[0], 0, request.questions.len())
                        {
                            message.push_str(&line);
                            message.push('\n');
                        }
                        console.write(&message)?;
                        return Ok(MenuOutcome {
                            draft_question: Some(request.id),
                            ..Default::default()
                        });
                    }
                },
                Selection::Workflow(workflow) => {
                    let permission = match &workflow { Workflow::Plan { request, .. } => *request, _ => None };
                    let effect = workflow.choose_with_feedback(agent, console, index, comment).await?;
                    if let Some(request) = permission { self.restored_plans.remove(&request); }
                    if let Some(next) = effect.next {
                        self.workflow(console, next)?;
                    }
                    if let Some((chat, goal)) = effect.timer {
                        console.clear_menu(menu_id)?;
                        self.edit_loop_timer(agent, console, chat, goal).await?;
                    }
                    return Ok(MenuOutcome {
                        permission,
                        selection: effect.selection,
                        restored: effect.restored,
                        receipt: effect.receipt,
                        ..Default::default()
                    });
                }
                Selection::SettingsInfo { target } => self.settings(agent, console, target).await?,
                Selection::Rewind { chat, mut points } => {
                    match index {
                        None => console
                            .write("Rewind cancelled; conversation and files unchanged.\n")?,
                        Some(index) if index == points.len() => console
                            .write("Current point kept; conversation and files unchanged.\n")?,
                        Some(index) if index < points.len() => {
                            let message_id = points[index].message_id;
                            points[index].changed_files = agent
                                .rewind_file_changes(chat, message_id)
                                .await
                                .ok()
                                .flatten();
                            self.show_rewind_actions(console, chat, points, message_id)?
                        }
                        Some(_) => return Err("rewind point does not exist".into()),
                    }
                }
                Selection::RewindActions {
                    chat,
                    points,
                    message_id,
                    actions,
                } => {
                    let action = index
                        .map(|index| {
                            actions
                                .get(index)
                                .copied()
                                .ok_or("rewind action does not exist")
                        })
                        .transpose()?;
                    match action {
                        None | Some(RewindAction::Back) => {
                            self.show_rewind_points(console, chat, points)?
                        }
                        Some(action) if action.summary_direction().is_some() => {
                            let request = directed_summary_request(action, message_id, comment)
                                .expect("summary action checked above");
                            return Ok(MenuOutcome {
                                summary: Some((chat, request)),
                                status: Some("Working · Summarizing conversation".into()),
                                ..Default::default()
                            });
                        }
                        Some(action) => {
                            let mode = match action {
                                RewindAction::RestoreCodeAndConversation => {
                                    RewindMode::CodeAndConversation
                                }
                                RewindAction::RestoreConversation => RewindMode::Conversation,
                                RewindAction::RestoreCode => RewindMode::Code,
                                _ => unreachable!(),
                            };
                            match agent.rewind(chat, message_id, mode).await {
                                Ok(outcome) => {
                                    return finish_rewind(console, outcome);
                                }
                                Err(error) => {
                                    let status = format!(
                                        "Cannot restore checkpoint: {}",
                                        safe_line(&error.to_string())
                                    );
                                    console.set_status(&status)?;
                                    self.show_rewind_actions(console, chat, points, message_id)?;
                                    return Ok(MenuOutcome {
                                        status: Some(status),
                                        ..Default::default()
                                    });
                                }
                            }
                        }
                    }
                }
                Selection::Provider { target, choices, rows } => {
                    self.refresh_provider_scope(agent, target).await?;
                    if let Some(index) = index {
                        self.provider.provider_selected = index;
                        if let Some(ProviderRow::Provider(provider_index)) = rows.get(index) {
                            let provider = choices[*provider_index].clone();
                            self.provider.provider_choices = choices;
                            self.provider.provider_nav.focused = Some(provider.key());
                            self.provider.provider_nav.back_stack.push(self.directory_parent());
                            return self.enter_connection(agent, console, target, provider).await;
                        }
                        if rows.is_empty() && index == 0 {
                            self.provider.provider_nav.filter = providers::ConnectionFilter::All;
                            self.back_to_providers(console, target)?;
                        } else {
                            self.provider.provider_nav.focused = None;
                            self.judgment_settings(agent, console, JudgmentReturn::Providers { target }).await?;
                        }
                    } else {
                        self.provider_back(agent, console, target).await?;
                    }
                }
                Selection::ConnectionDetails { target, provider, actions } => {
                    let Some(index) = index else {
                        self.provider_back(agent, console, target).await?;
                        return Ok(MenuOutcome::default());
                    };
                    let action = *actions.get(index).ok_or("Connection action no longer exists")?;
                    let parent = providers::ProviderPage::Details { connection: provider.key(), selected: index };
                    match action {
                        providers::ConnectionAction::Browse => {
                            if matches!(self.provider.provider_nav.back_stack.last(), Some(providers::ProviderPage::Models { connection, .. }) if connection.as_deref().is_none_or(|key| key == provider.key())) {
                                self.provider_back(agent, console, target).await?;
                            } else {
                                self.provider.provider_nav.back_stack.push(parent);
                                return self.enter_connection(agent, console, target, provider).await;
                            }
                        }
                        providers::ConnectionAction::Configure => {
                            self.provider.provider_nav.back_stack.push(parent);
                            self.provider.provider_nav.credential_intent = Some(if self.provider_authenticated(&provider) == Ok(false) { providers::CredentialIntent::Setup } else { providers::CredentialIntent::Replace });
                            return self.provider_auth(console, target, provider);
                        }
                        providers::ConnectionAction::RemoveKey | providers::ConnectionAction::SignOut => {
                            self.provider.provider_nav.back_stack.push(parent);
                            let label = if matches!(action, providers::ConnectionAction::RemoveKey) { "Remove saved key" } else { "Sign out" };
                            let fallback = self.provider_environment_key(&provider.models[0].api_key_env);
                            let context = if matches!(action, providers::ConnectionAction::RemoveKey) {
                                vec!["This removes the saved value of this connection's credential reference. Other connections using that reference are also affected.".into(), if fallback { "An environment key will still be used.".into() } else { "No environment key is configured.".into() }]
                            } else { vec!["This signs out this provider's saved login. Connections and Chats using the same login also need sign-in.".into()] };
                            self.show(console, Menu { id: Uuid::new_v4(), title: format!("{label} · {}", provider.model_page_name()), context, options: vec!["Cancel".into(), label.into()], selected: 0, confirmation: false }, Selection::ConnectionConfirm { target, provider, action })?;
                        }
                        providers::ConnectionAction::CredentialDetails => {
                            self.provider.provider_nav.back_stack.push(parent);
                            let root = self.provider_root().map(|root| root.join(if provider.models[0].auth_source == tickloop_agent_protocol::ModelAuthSource::ApiKeyEnv || provider.models[0].auth_source == tickloop_agent_protocol::ModelAuthSource::GoogleCloud { ".env" } else { "auth" }).display().to_string()).unwrap_or_else(|| "No private store".into());
                            let reference = &provider.models[0].api_key_env;
                            let context = vec![self.provider_auth_label(&provider).into(), safe_text(&format!("Credential reference: {reference}")), safe_text(&format!("Private store: {root}")), "Saved values take precedence over the Agent's inherited environment. Removing saved values does not unset the environment.".into()];
                            self.show(console, Menu { id: Uuid::new_v4(), title: format!("Credential details · {}", provider.model_page_name()), context, options: vec!["Back to connection".into()], selected: 0, confirmation: false }, Selection::CredentialDetails { target })?;
                        }
                    }
                }
                Selection::ConnectionConfirm { target, provider, action } => {
                    if index != Some(1) {
                        self.provider_back(agent, console, target).await?;
                    } else if matches!(action, providers::ConnectionAction::RemoveKey) {
                        let root = self.provider_root().ok_or("Cannot locate the Agent credential directory")?;
                        if let Err(error) = tickloop_agent_model::credentials::remove_api_key(root, &provider.models[0].api_key_env) {
                            self.provider_back(agent, console, target).await?;
                            return Err(error.to_string());
                        }
                        self.model_discovery.invalidate(&provider.models[0].provider_id);
                        self.refresh_connections.insert((target.agent(), provider.key()), Uuid::new_v4());
                        self.provider_back(agent, console, target).await?;
                    } else {
                        let sign_in = super::login::SignInTarget::from_auth(provider.models[0].auth_source).unwrap_or(super::login::SignInTarget::Codex);
                        self.provider.login_return = Some((target, provider));
                        self.provider_back(agent, console, target).await?;
                        return Ok(MenuOutcome { provider_logout: true, logout_target: sign_in, ..Default::default() });
                    }
                }
                Selection::CredentialDetails { target } => self.provider_back(agent, console, target).await?,
                Selection::LoginMethods { target, provider } => {
                    if let Some(index) = index {
                        self.provider.provider_nav.back_stack.push(providers::ProviderPage::LoginMethods { connection: provider.key() });
                        return self.begin_provider_login(console, target, provider, if index == 0 { super::login::LoginMethod::Device } else { super::login::LoginMethod::Browser });
                    }
                    self.provider_back(agent, console, target).await?;
                }
                Selection::ServiceAccount { target, provider } => {
                    if index == Some(0) {
                        let root = self.provider_root().ok_or("Cannot locate the Agent state directory; set TICKLOOP_CONFIG_DIR.")?;
                        let path = comment.as_deref().unwrap_or("").trim();
                        if let Err(error) = tickloop_agent_model::auth::google::GoogleAuth::new(root).save_service_account_path(path) {
                            self.provider_service_account(console, target, provider, Some(path))?;
                            return Err(error.to_string());
                        }
                        self.model_discovery.invalidate(&provider.models[0].provider_id);
                        console.write("Service-account file configured.\n")?;
                        self.credential_saved(agent, console, target, &provider).await?;
                    } else {
                        self.provider_back(agent, console, target).await?;
                    }
                }
                Selection::ApiKey { target, provider } => {
                    if index == Some(0) {
                        let root = self.provider_root().ok_or("Cannot locate the Agent state directory; set TICKLOOP_CONFIG_DIR.")?;
                        let result = tickloop_agent_model::credentials::save_api_key(root, &provider.models[0].api_key_env, comment.as_deref().unwrap_or(""));
                        if let Err(error) = result {
                            self.provider_api_key(console, target, provider, comment.as_deref())?;
                            return Err(error.to_string());
                        }
                        self.model_discovery.invalidate(&provider.models[0].provider_id);
                        console.write("API key saved to Agent .env.\n")?;
                        self.credential_saved(agent, console, target, &provider).await?;
                    } else {
                        self.provider_back(agent, console, target).await?;
                    }
                }
                Selection::JudgmentSettings { return_to, has_key } => {
                    match index {
                        Some(0) if has_key && matches!(&return_to, JudgmentReturn::AutoSettings { select_reviewer: true, .. }) => {
                            console.clear_menu(menu_id)?;
                            self.finish_judgment_selection(agent, console, return_to, false).await?;
                        }
                        Some(option) if option == usize::from(has_key && matches!(&return_to, JudgmentReturn::AutoSettings { select_reviewer: true, .. })) => {
                            self.judgment_key(console, return_to)?;
                        }
                        _ => {
                            if matches!(&return_to, JudgmentReturn::AutoSettings { .. }) {
                                console.clear_menu(menu_id)?;
                            }
                            self.return_from_judgment(agent, console, return_to).await?;
                        }
                    }
                }
                Selection::JudgmentKey { return_to } => {
                    if index == Some(0) {
                        let value = comment.as_deref().unwrap_or("");
                        if let Err(error) = agent.save_jev_key(value).await {
                            self.judgment_key(console, return_to)?;
                            return Err(error.to_string());
                        }
                        match return_to {
                            JudgmentReturn::AutoSettings { select_reviewer: true, .. } => {
                                console.clear_menu(menu_id)?;
                                self.finish_judgment_selection(agent, console, return_to, true).await?;
                            }
                            JudgmentReturn::Providers { .. } => {
                                self.judgment_settings(agent, console, return_to).await?;
                                console.write("Jev (TypeSafe) key saved; authentication has not been verified.\n")?;
                            }
                            JudgmentReturn::AutoSettings { select_reviewer: false, .. } => {
                                self.judgment_settings(agent, console, return_to).await?;
                                console.write("Jev (TypeSafe) key saved; authentication has not been verified.\n")?;
                            }
                        }
                    } else {
                        self.judgment_settings(agent, console, return_to).await?;
                    }
                }
                Selection::Model { target, choices, actions } => {
                    let Some(index) = index else {
                        self.remember_model_drafts(&choices);
                        self.provider_back(agent, console, target).await?;
                        return Ok(MenuOutcome::default());
                    };
                    if let Some(action) = index.checked_sub(choices.len()).and_then(|index| actions.get(index)) {
                        match action {
                            providers::ModelAction::ConnectionDetails => self.open_model_providers(console, target, choices, index)?,
                            providers::ModelAction::RefreshAccountModels => {
                                self.remember_model_drafts(&choices);
                                self.refresh_model_choices(agent, console, target, None, Some(providers::ModelAction::RefreshAccountModels)).await?;
                            }
                            providers::ModelAction::Providers => {
                                self.provider.provider_nav.back_stack.push(providers::ProviderPage::Models {
                                    connection: self.provider.model_provider.clone(),
                                    focused: self.provider.focused_model.clone(),
                                    action: Some(providers::ModelAction::Providers),
                                });
                                self.provider.provider_nav.filter = providers::ConnectionFilter::All;
                                self.back_to_providers(console, target)?;
                            }
                        }
                        return Ok(MenuOutcome::default());
                    }
                    let choice = choices.get(index).ok_or("model option does not exist")?;
                    if !choice.availability.selectable() {
                        let error = choice.availability.label().to_owned();
                        self.show_model_choices(console, target, choices, index, Uuid::new_v4())?;
                        return Err(error);
                    }
                    let result = agent
                        .change_configuration(
                            target,
                            ConfigurationChange::Model {
                                config: choice.config.clone(),
                                thinking: Some(choice.thinking),
                            },
                        )
                        .await
                        .map_err(|error| error.to_string());
                    match result {
                        Ok(change) => console.write(&safe_text(&format!("Model selection: {change} · {} · Thinking {}\n", choice.config.name, choice.thinking.map_or("auto", ReasoningEffort::label))))?,
                        Err(error) => {
                            self.show_model_choices(console, target, choices, index, Uuid::new_v4())?;
                            return Err(error);
                        }
                    }
                    let snapshot = agent.configuration(target).await.map_err(|error| error.to_string())?;
                    self.provider.current_connection = self.key_for_model(&snapshot.model);
                    self.provider.current_model = Some((snapshot.model.provider_id, snapshot.model.model_id));
                    let updated = Uuid::new_v4();
                    self.show_model_choices(console, target, choices, index, updated)?;
                }
                Selection::PermissionMode {
                    target,
                    return_to_settings: true,
                    ..
                } if index.is_none() => self.settings(agent, console, target).await?,
                Selection::PermissionMode { .. } if index.is_none() => {},
                Selection::Settings { target } if index.is_none() => {
                    self.settings_positions.remove(&target.agent());
                }
                Selection::Effort { target, return_to_settings: true, .. }
                | Selection::Mode { target, .. }
                | Selection::OutputStyle { target }
                | Selection::GlyphSet { target, .. }
                | Selection::ArtifactSignOut { target }
                    if index.is_none() => self.settings(agent, console, target).await?,
                Selection::GlyphSetConfirmation { target, glyph_set }
                    if index.is_none() => self.glyph_sets_with_focus(console, target, GlyphSet::ALL.into_iter().position(|candidate| candidate == glyph_set)).await?,
                Selection::FontActionConfirmation { target, .. }
                    if index.is_none() => self.glyph_sets_with_focus(console, target, Some(GlyphSet::ALL.len())).await?,
                Selection::Theme { settings_target: Some(target) }
                    if index.is_none() => self.settings(agent, console, target).await?,
                _ if index.is_none() => {
                    console.write("Selection cancelled; configuration unchanged.\n")?
                }
                Selection::Effort { target, choices, return_to_settings } => {
                    let effort = *choices
                        .get(index.unwrap())
                        .ok_or("effort option does not exist")?;
                    let change = agent
                        .change_configuration(target, ConfigurationChange::ReasoningEffort { effort })
                        .await
                        .map_err(|error| error.to_string())?;
                    console.write(&format!(
                        "Thinking: {}{}\n",
                        effort.map_or("auto", ReasoningEffort::label),
                        change.staged_note()
                    ))?;
                    if return_to_settings {
                        self.settings(agent, console, target).await?;
                    } else {
                        self.show_efforts(agent, console, target, false).await?;
                    }
                }
                Selection::ArtifactSignOut { target } => {
                    if index == Some(1) {
                        let root = self.config_root.as_ref().ok_or("Artifact Service needs a state directory")?;
                        agent.disable_event_sources().await.map_err(|error| error.to_string())?;
                        let client = tickloop_agent_artifacts::ArtifactClient::new(root).map_err(|error| error.to_string())?;
                        client.logout().await.map_err(|error| error.to_string())?;
                        console.write("Artifact Service signed out.\n")?;
                    }
                    self.settings(agent, console, target).await?;
                },
                Selection::Settings { target } => match index {
                    Some(PROMPT_SUGGESTION_SETTING_INDEX) => {
                        let enabled = !console.prompt_suggestion_enabled()?;
                        self.user_settings.as_ref().ok_or("cannot save prompt suggestions because the user settings directory is unavailable")?
                            .save_prompt_suggestion_enabled(enabled).map_err(|error| error.to_string())?;
                        console.set_prompt_suggestion_enabled(enabled)?;
                        self.settings(agent, console, target).await?;
                    }
                    Some(0) => self.open_models(agent, console, target, true).await?,
                    Some(1) => {
                        let configuration = agent.configuration(target).await.map_err(|error| error.to_string())?;
                        if matches!(
                            tickloop_agent_model::cards::model_card(&configuration.model)
                                .map(|card| card.thinking),
                            Some(tickloop_agent_protocol::ThinkingControl::Toggle)
                        ) {
                            let enabled = !configuration.model.thinking_enabled(configuration.reasoning_effort, configuration.thinking_enabled).unwrap_or(configuration.thinking_enabled);
                            agent.change_configuration(target, ConfigurationChange::ThinkingEnabled { enabled }).await.map_err(|error| error.to_string())?;
                        } else {
                            console.write("Thinking toggle is unavailable for this model.\n")?;
                        }
                        self.settings(agent, console, target).await?;
                    }
                    Some(2) => self.show_efforts(agent, console, target, true).await?,
                    Some(3) => self.modes(agent, console, target).await?,
                    Some(4) => self.settings_info(agent, console, target, false).await?,
                    Some(5) => self.settings_info(agent, console, target, true).await?,
                    Some(6) => {
                        console.set_auto_scroll(!console.auto_scroll()?)?;
                        self.settings(agent, console, target).await?;
                    }
                    Some(7) => self.themes(console, Some(target))?,
                    Some(8) => self.glyph_sets(console, target).await?,
                    Some(REDUCE_MOTION_SETTING_INDEX) => {
                        let mut preferences = console.display_preferences()?;
                        preferences.prefers_reduced_motion = !preferences.prefers_reduced_motion;
                        self.save_display_preferences(console, preferences)?;
                        self.settings(agent, console, target).await?;
                    }
                    Some(12) => {
                        self.show(console, Menu { id: Uuid::new_v4(), title: "Sign out of Artifact Service?".into(), context: vec!["Stops feedback listening before revoking the credential.".into()], options: vec!["Cancel".into(), "Sign out".into()], selected: 0, confirmation: true }, Selection::ArtifactSignOut { target })?;
                    }
                    Some(OUTPUT_STYLE_SETTING_INDEX) => self.output_styles(console, target)?,
                    Some(PERMISSIONS_SETTING_INDEX) => {
                        match self.permission_modes(agent, console, target, true).await {
                            Ok(()) => {}
                            Err(error) => {
                                self.settings(agent, console, target).await?;
                                console.write(&format!(
                                    "Cannot open Permissions: {}.\n",
                                    super::safe_text(&error)
                                ))?;
                            },
                        }
                    }
                    _ => return Err("setting does not exist".into()),
                },
                Selection::Theme { settings_target } => {
                    let theme = *ThemeName::ALL
                        .get(index.unwrap())
                        .ok_or("theme option does not exist")?;
                    console.set_theme(theme)?;
                    match settings_target {
                        Some(target) => self.settings(agent, console, target).await?,
                        None => console.write(&format!("Theme: {}\n", theme.label()))?,
                    }
                }
                Selection::OutputStyle { target } => {
                    let style = *OutputStyle::ALL
                        .get(index.unwrap())
                        .ok_or("output style option does not exist")?;
                    self.user_settings
                        .as_ref()
                        .ok_or("cannot save the output style because the user settings directory is unavailable")?
                        .save_output_style(style)
                        .map_err(|error| error.to_string())?;
                    self.settings(agent, console, target).await?;
                }
                Selection::GlyphSet { target, font_action } => {
                    let selected = index.unwrap();
                    if let Some(glyph_set) = GlyphSet::ALL.get(selected).copied() {
                        self.confirm_glyph_set(console, target, glyph_set)?;
                    } else if selected == GlyphSet::ALL.len() {
                        self.confirm_font_action(
                            console,
                            target,
                            font_action.ok_or("font action does not exist")?,
                        )?;
                    } else {
                        return Err("Glyph Set option does not exist".into());
                    }
                }
                Selection::GlyphSetConfirmation { target, glyph_set } => match index {
                    Some(0) => {
                        let mut preferences = console.display_preferences()?;
                        preferences.glyph_set = glyph_set;
                        self.save_display_preferences(console, preferences)?;
                        self.settings(agent, console, target).await?;
                    }
                    Some(1) => self.glyph_sets_with_focus(console, target, GlyphSet::ALL.into_iter().position(|candidate| candidate == glyph_set)).await?,
                    _ => return Err("glyph confirmation option does not exist".into()),
                },
                Selection::FontActionConfirmation { target, action } => match index {
                    Some(0) => {
                        let mut preferences = console.display_preferences()?;
                        if preferences.glyph_set != GlyphSet::Unicode {
                            preferences.glyph_set = GlyphSet::Unicode;
                            self.save_display_preferences(console, preferences)?;
                        }
                        let outcome = self
                            .font_manager
                            .as_ref()
                            .ok_or("font manager is unavailable")?
                            .apply(action)
                            .await
                            .map_err(|error| error.to_string())?;
                        let message = match outcome {
                            FontOutcome::ReadyForPreview { message }
                            | FontOutcome::Removed { message } => message,
                        };
                        console.write(&format!("{message}\n"))?;
                        self.glyph_sets_with_focus(console, target, Some(GlyphSet::ALL.len())).await?;
                    }
                    Some(1) => self.glyph_sets_with_focus(console, target, Some(GlyphSet::ALL.len())).await?,
                    _ => return Err("font action confirmation does not exist".into()),
                },
                Selection::Mode { target, choices } => {
                    let mode = *choices
                        .get(index.unwrap())
                        .ok_or("execution mode does not exist")?;
                    let change = agent
                        .change_configuration(target, ConfigurationChange::ExecutionMode { mode })
                        .await
                        .map_err(|error| error.to_string())?;
                    console.write(&format!(
                        "Execution mode: {mode}. Explicit deny rules remain effective.{}\n",
                        change.staged_note()
                    ))?;
                    self.settings(agent, console, target).await?;
                }
                Selection::PermissionMode {
                    target: ConfigTarget::NewDraft,
                    choices,
                    current,
                    return_to_settings,
                } => {
                    let index = index.unwrap();
                    let is_mode_selection = index < choices.len();
                    let mode = if is_mode_selection {
                        choices[index]
                    } else if index == choices.len() {
                        current
                    } else {
                        return Err("permission mode does not exist".into());
                    };
                    let result = async {
                        if is_mode_selection && mode != current {
                            agent
                                .change_configuration(
                                    ConfigTarget::NewDraft,
                                    ConfigurationChange::ExecutionMode { mode },
                                )
                                .await
                                .map_err(|error| error.to_string())?;
                        }
                        console.clear_menu(menu_id)?;
                        self.open_draft_permissions(
                            agent,
                            console,
                            return_to_settings,
                            is_mode_selection && mode == ExecutionMode::Auto,
                        )
                        .await
                    }
                    .await;
                    if let Err(error) = result {
                        self.permission_modes(
                            agent,
                            console,
                            ConfigTarget::NewDraft,
                            return_to_settings,
                        )
                        .await?;
                        console.write(&format!(
                            "Cannot open Permissions: {}. Fix the private Auto settings and retry.\n",
                            super::safe_text(&error)
                        ))?;
                    }
                }
                Selection::PermissionMode {
                    target: ConfigTarget::Agent(chat),
                    choices,
                    current,
                    return_to_settings,
                } => {
                    let target = ConfigTarget::Agent(chat);
                    if let Some(index) = index {
                        let mode = if index == choices.len() {
                            current
                        } else {
                            *choices.get(index).ok_or("permission mode does not exist")?
                        };
                        let is_mode_selection = index < choices.len();
                        let result = async {
                            if is_mode_selection {
                                let actual = agent
                                    .configuration(target)
                                    .await
                                    .map_err(|error| error.to_string())?
                                    .execution_mode;
                                if mode != actual {
                                    agent
                                        .set_execution_mode(chat, mode)
                                        .await
                                        .map_err(|error| error.to_string())?;
                                }
                            }
                            let view = agent
                                .permissions_view(chat)
                                .await
                                .map_err(|error| error.to_string())?;
                            let history = agent
                                .approval_history_view(chat)
                                .await
                                .map_err(|error| super::safe_text(&error.to_string()));
                            Ok::<_, String>((view, history))
                        }
                        .await;
                        match result {
                            Ok((view, history)) => {
                                console.clear_menu(menu_id)?;
                                let auto_page = is_mode_selection
                                    && view.execution_mode == ExecutionMode::Auto;
                                console.show_permissions(
                                    view,
                                    history,
                                    return_to_settings,
                                    auto_page,
                                )?;
                            }
                            Err(error) => {
                                self.permission_modes(agent, console, target, return_to_settings)
                                    .await?;
                                console.write(&format!(
                                    "Cannot open Permissions: {}. Fix the private Auto settings and retry.\n",
                                    super::safe_text(&error)
                                ))?;
                            }
                        }
                    }
                }
            }
            Ok(MenuOutcome::default())
        }
        .await;
        if outcome.is_err()
            && self.active.is_none()
            && let Some((selection, mut menu)) = retry
        {
            menu.id = Uuid::new_v4();
            if let Some(index) = index {
                menu.selected = index;
            }
            let restored = menu.id;
            self.provider.provider_nav = navigation_before;
            self.provider.model_provider = model_provider_before;
            if console.recover_menu(menu_id, restored, index)? {
                self.active = Some((restored, selection));
            } else {
                self.show(console, menu, selection)?;
            }
            if let Some(flow) = &mut self.provider.discovery_flow
                && flow.id == menu_id
            {
                flow.id = restored;
            }
        }
        console.finish_menu_submission(menu_id)?;
        outcome
    }
}

/// Names a requester by its registered child handle or root Chat title, with a View reading
/// number for duplicate root titles. The request itself stays bound to its
/// execution.
async fn requester_label(agent: &Agent, console: &Console, snapshot: &ChatSnapshot) -> String {
    let title = super::safe_line(
        &agent
            .cached_title(snapshot.id)
            .map(|(_, title)| title)
            .unwrap_or_else(|| chat_display_title(snapshot, &snapshot.user_shells)),
    );
    agent
        .reference(snapshot.id)
        .await
        .ok()
        .and_then(|execution| console.agent_name(execution.agent_id, &title).ok())
        .unwrap_or_else(|| format!("Agent {title}"))
}

/// The confirmation for the Agent's `EnterPlanMode` request, following Claude Code's
/// `EnterPlanModePermissionRequest`.
fn enter_plan_mode_menu(request_id: Uuid, goal: String) -> Menu {
    Menu {
        id: request_id,
        title: "Enter plan mode?".into(),
        context: vec![
            goal,
            "The Agent wants to enter plan mode to explore and design an implementation approach."
                .into(),
            "No changes will be made until you approve the plan.".into(),
            "In plan mode, the Agent will:".into(),
            " · Explore the codebase thoroughly".into(),
            " · Identify existing patterns".into(),
            " · Design an implementation strategy".into(),
            " · Present a plan for your approval".into(),
        ],
        options: vec![
            "Yes, enter plan mode".into(),
            "No, start implementing now".into(),
        ],
        selected: 0,
        confirmation: true,
    }
}

fn recorded_usage_context(summary: &UsageSummary) -> Vec<String> {
    let mut context = vec![format!(
        "Requests in this process: {}",
        summary.request_count
    )];
    for (kind, count) in &summary.kind_counts {
        context.push(format!("{}: {count}", request_kind_label(*kind)));
    }
    for (outcome, count) in &summary.outcome_counts {
        context.push(format!("{}: {count}", request_outcome_label(outcome)));
    }
    if summary.request_count == 0 {
        context.push("This Chat has made no model requests in this process.".into());
    }
    for group in &summary.groups {
        context.push(String::new());
        let protocol = match group.model.protocol {
            Protocol::Messages => "Messages",
            Protocol::Responses => "Responses",
            Protocol::ChatCompletions => "Chat Completions",
            Protocol::Google => "Google API",
            Protocol::SystemOne => "System One",
        };
        for (label, value) in [
            ("Model", &group.model.requested_model),
            ("Profile", &group.model.profile),
        ] {
            context.push(
                safe_text(&format!("{label}: {value}"))
                    .replace('\n', " ↵ ")
                    .replace('\t', " ⇥ "),
            );
        }
        context.push(format!("Protocol: {protocol}"));
        context.push(format!("Request records: {}", group.request_count));
        for (kind, count) in &group.kind_counts {
            context.push(format!("{}: {count}", request_kind_label(*kind)));
        }
        for (outcome, count) in &group.outcome_counts {
            context.push(format!("{}: {count}", request_outcome_label(outcome)));
        }
        for (label, counter) in [
            ("Input tokens", &group.input_tokens),
            ("Output tokens", &group.output_tokens),
            ("Cache read tokens", &group.cache_read_input_tokens),
            (
                "Cache write/creation tokens",
                &group.cache_write_input_tokens,
            ),
        ] {
            context.push(format!("{label}: {}", usage_counter_label(counter)));
        }
    }
    context.push(String::new());
    context.extend([
        "Counts only requests made since this process started; usage is not restored after a restart.".into(),
        "Service-reported token counters; not billing, price or account quota.".into(),
        "Reported coverage counts requests, including unfinished or unsuccessful requests. Missing counters are not estimated.".into(),
        "Cache counters are shown separately; their inclusion in input depends on the service and protocol.".into(),
        "This view is a snapshot. Reopen Usage to see later updates.".into(),
    ]);
    context
}

fn request_kind_label(kind: ModelRequestKind) -> &'static str {
    match kind {
        ModelRequestKind::Conversation => "Conversation",
        ModelRequestKind::CompactionAutomatic => "Automatic compaction",
        ModelRequestKind::CompactionManual => "Manual compaction",
        ModelRequestKind::CompactionDirected => "Directed summary",
        ModelRequestKind::PermissionCheck => "Permission checks",
        ModelRequestKind::WebFetch => "WebFetch",
        ModelRequestKind::Title => "Chat titles",
        ModelRequestKind::AutoSetup => "Auto setup",
    }
}

fn request_outcome_label(outcome: &RequestOutcome) -> String {
    match outcome {
        RequestOutcome::InFlight => "In progress".into(),
        RequestOutcome::Accepted => "Accepted responses".into(),
        RequestOutcome::TimedOut => "Timed out".into(),
        RequestOutcome::Cancelled => "Cancelled".into(),
        RequestOutcome::HostRejected(reason) => {
            let reason = match reason {
                ResponseRejection::InvalidToolCall => "invalid tool call",
                ResponseRejection::EmptySummary => "empty summary",
                ResponseRejection::InvalidSummary => "invalid summary",
                ResponseRejection::InvalidVerdict => "invalid permission verdict",
                ResponseRejection::ContextChanged => "context changed",
                ResponseRejection::SummaryTooLarge => "summary too large",
                ResponseRejection::OutputLimit => "output limit",
                ResponseRejection::ContextLimit => "context limit",
                ResponseRejection::ContentFilter => "content filter",
                ResponseRejection::Refusal => "refusal",
                ResponseRejection::UnsupportedContinuation => "unsupported continuation",
            };
            format!("Host rejected ({reason})")
        }
        RequestOutcome::Failed(reason) => {
            let reason = match reason {
                RequestFailure::LocalConfiguration => "local configuration".into(),
                RequestFailure::Authentication => "authentication".into(),
                RequestFailure::RateLimited => "rate limited".into(),
                RequestFailure::Http { status } => format!("HTTP {status}"),
                RequestFailure::Connection => "connection".into(),
                RequestFailure::StreamInterrupted => "stream interrupted".into(),
                RequestFailure::ProviderFailure => "service failure".into(),
                RequestFailure::Protocol(ProtocolFailure::InvalidResponse) => "protocol".into(),
                RequestFailure::Protocol(ProtocolFailure::UsageRegression(counter)) => {
                    let counter = match counter {
                        UsageCounter::Input => "input",
                        UsageCounter::Output => "output",
                        UsageCounter::CacheRead => "cache read",
                        UsageCounter::CacheWrite => "cache write/creation",
                    };
                    format!("protocol: {counter} counter decreased")
                }
            };
            format!("Failed ({reason})")
        }
    }
}

fn usage_counter_label(counter: &CounterSummary) -> String {
    let value = match counter.total {
        CounterTotal::NotReported => "not reported".into(),
        CounterTotal::Value(value) => {
            if counter.coverage == UsageCoverage::Partial {
                format!("{value} reported subtotal")
            } else {
                value.to_string()
            }
        }
        CounterTotal::Overflow => "total exceeds the supported range".into(),
    };
    format!(
        "{value} · reported {}/{} requests",
        counter.reported_requests, counter.request_count
    )
}

fn finish_rewind(console: &Console, outcome: RewindOutcome) -> Result<MenuOutcome, String> {
    if outcome.conversation_restored {
        console.reset_chat(outcome.snapshot.id)?;
        workflows::write_public_history(console, &outcome.snapshot)?;
        console.write(
            "Conversation restored. The abandoned messages remain in the Transcript file.\n",
        )?;
    }
    for line in rewind_code_lines(&outcome) {
        console.write(&line)?;
    }
    let selection = if let Some(draft) = &outcome.draft {
        console.restore_rewind_draft(&draft.prompt, &draft.contexts)?;
        Some((outcome.snapshot.id, Some(draft.loop_id)))
    } else {
        None
    };
    let status = rewind_outcome_status(&outcome);
    console.set_status(&status)?;
    Ok(MenuOutcome {
        restored: Some(outcome.snapshot),
        selection,
        status: Some(status),
        ..Default::default()
    })
}

fn rewind_code_lines(outcome: &RewindOutcome) -> Vec<String> {
    let Some(code) = &outcome.code else {
        return Vec::new();
    };
    if let Some(error) = &code.error {
        return vec![format!("Code was not restored: {}\n", safe_line(error))];
    }
    let mut lines = code
        .changed_files
        .iter()
        .map(|path| format!("Restored {}\n", safe_line(&path.to_string_lossy())))
        .collect::<Vec<_>>();
    lines.extend(code.failed_files.iter().map(|(path, error)| {
        format!(
            "Failed to restore {}: {}\n",
            safe_line(&path.to_string_lossy()),
            safe_line(error)
        )
    }));
    lines
}

fn rewind_outcome_status(outcome: &RewindOutcome) -> String {
    let code = outcome.code.as_ref().map(|code| {
        if code.error.is_some() {
            "code was not restored".to_owned()
        } else if code.failed_files.is_empty() {
            format!(
                "code restored ({} file{} changed)",
                code.changed_files.len(),
                if code.changed_files.len() == 1 {
                    ""
                } else {
                    "s"
                }
            )
        } else {
            format!(
                "code partly restored ({} changed, {} failed)",
                code.changed_files.len(),
                code.failed_files.len()
            )
        }
    });
    let draft = outcome
        .draft
        .as_ref()
        .map(|draft| match draft.contexts.len() {
            0 => "prompt restored for editing".to_owned(),
            1 => "prompt restored with 1 saved attachment".to_owned(),
            count => format!("prompt restored with {count} saved attachments"),
        });
    let mut parts = Vec::new();
    if outcome.conversation_restored {
        parts.push("Conversation restored".to_owned());
    }
    parts.extend(code);
    parts.extend(draft);
    let mut status = parts.join(" · ");
    if let Some(first) = status.get(..1) {
        status = first.to_uppercase() + &status[1..];
    }
    status
}

fn rewind_points(snapshot: &ChatSnapshot) -> Vec<RewindPoint> {
    snapshot
        .rewind_points()
        .into_iter()
        .map(|point| RewindPoint {
            message_id: point.message_id,
            message_index: point.message_index,
            prompt: rewind_prompt_label(&point.prompt),
            changed_files: None,
        })
        .collect()
}

fn rewind_prompt_label(prompt: &str) -> String {
    let single_line = safe_text(prompt).replace('\n', " ↵ ");
    let mut characters = single_line.chars();
    let label = characters.by_ref().take(96).collect::<String>();
    if characters.next().is_some() {
        format!("{label}…")
    } else {
        label
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;
    use tickloop_agent_protocol::{CodeRewind, ModelUsageSummary, RewindDraft};
    use tickloop_agent_protocol::{Content, Message, ModelIdentity, Role};

    use super::*;

    #[tokio::test]
    async fn recovery_queues_pending_requests_even_after_their_events_are_evicted() {
        let directory = tempfile::tempdir().unwrap();
        let core = tickloop_agent_runtime::Agent::new_embedded(
            tickloop_agent_test_support::config(),
            std::sync::Arc::new(tickloop_agent_test_support::ScriptedModel::new(vec![
                tickloop_agent_test_support::call(
                    "snapshot-question",
                    tickloop_agent_tools::ASK_USER_QUESTION_TOOL_NAME,
                    tickloop_agent_test_support::single_question("Which fixture?"),
                ),
            ])),
        );
        let chat = core.create_chat(directory.path().into()).await.unwrap();
        let receipt = core
            .submit(chat, None, "Ask a controlled question".into())
            .await
            .unwrap();
        let mut snapshot = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let snapshot = core.snapshot(chat).await.unwrap();
                if !snapshot.questions.is_empty() {
                    break snapshot;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let question = snapshot.questions[0].id;
        let permission = Uuid::new_v4();
        snapshot
            .permissions
            .push(tickloop_agent_protocol::PermissionRequest {
                id: permission,
                loop_id: receipt.loop_id,
                execution: tickloop_agent_protocol::PermissionExecution::ToolCall {
                    run_id: receipt.run_id,
                    call_id: "snapshot-permission".into(),
                },
                name: "Bash".into(),
                input: json!({"command":"printf fixture"}),
                file_change: None,
                approval_reason: None,
                approval_source:
                    tickloop_agent_protocol::PermissionApprovalSource::PermissionPolicy,
            });
        snapshot.events.clear();
        let mut menus = Menus::default();
        menus.note_snapshot_requests(&snapshot);
        menus.note_snapshot_requests(&snapshot);
        assert_eq!(
            menus.request_order,
            VecDeque::from([(chat, permission), (chat, question)])
        );
        core.shutdown().await;
    }

    #[tokio::test]
    async fn connection_management_uses_the_captured_agents_store_from_a_different_view_root() {
        use tickloop_agent_daemon::EmbeddedAgentAccess;
        use tickloop_agent_protocol::{
            AgentAccess, AgentCommand, CommandEnvelope, CommandResult, LaunchSpec, ViewRef,
        };
        let directory = tempfile::tempdir().unwrap();
        let project = directory.path().canonicalize().unwrap();
        let root_a = project.join("a");
        let root_b = project.join("b");
        let reference = "PICKER_SCOPE_TEST_KEY";
        tickloop_agent_model::credentials::save_api_key(&root_a, reference, "root-a-fixture-key")
            .unwrap();
        tickloop_agent_model::credentials::save_api_key(&root_b, reference, "root-b-fixture-key")
            .unwrap();
        let mut model = tickloop_agent_test_support::config();
        model.api_key_env = reference.into();
        let core = tickloop_agent_runtime::Agent::new_embedded(
            model.clone(),
            std::sync::Arc::new(tickloop_agent_test_support::ScriptedModel::new(Vec::new())),
        );
        let access = std::sync::Arc::new(EmbeddedAgentAccess::new(core.clone()));
        let mut launch = LaunchSpec::embedded(project);
        launch.model = Some(model.clone());
        launch.config_root = Some(root_a.clone());
        let receipt = access
            .execute(CommandEnvelope {
                daemon_epoch: access.daemon_epoch(),
                command_id: Uuid::new_v4(),
                command: AgentCommand::Create {
                    launch: launch.clone(),
                    initial: None,
                    origin: None,
                },
            })
            .await
            .unwrap();
        let CommandResult::Created { snapshot, .. } = receipt.into_result().unwrap() else {
            panic!("Agent not created")
        };
        let target = ConfigTarget::Agent(snapshot.conversation.id);
        let before_model = snapshot.model.clone();
        launch.config_root = Some(root_b.clone());
        let view = Agent::from_access_for_test(
            access.clone(),
            access.daemon_epoch(),
            ViewRef {
                session_id: Uuid::new_v4(),
                view_id: Uuid::new_v4(),
            },
            launch,
            None,
        );
        let (console, _) = Console::start(false).unwrap();
        let mut menus = Menus::new(Some(UserSettings::in_root(&root_b)), Some(root_b.clone()));
        menus.models(&view, &console, target).await.unwrap();
        assert_eq!(menus.provider_root(), Some(root_a.as_path()));
        let (id, Selection::Model { choices, .. }) = menus.active.as_ref().unwrap() else {
            panic!("Expected configured Agent models")
        };
        let choice = choices[0].config.clone();
        let id = *id;
        menus
            .picker_action(&view, &console, id, 0, PickerAction::ConfigureProvider)
            .await
            .unwrap();
        let (_, Selection::ConnectionDetails { provider, .. }) = menus.active.as_ref().unwrap()
        else {
            panic!("Expected connection details")
        };
        assert_eq!(provider.models[0], choice);
        tickloop_agent_model::credentials::save_api_key(
            menus.provider_root().unwrap(),
            reference,
            "replacement-for-a",
        )
        .unwrap();
        menus.provider_back(&view, &console, target).await.unwrap();
        assert_eq!(
            tickloop_agent_model::credentials::load_api_key(&root_a, reference)
                .unwrap()
                .as_deref(),
            Some("replacement-for-a")
        );
        assert_eq!(
            tickloop_agent_model::credentials::load_api_key(&root_b, reference)
                .unwrap()
                .as_deref(),
            Some("root-b-fixture-key")
        );
        assert_eq!(
            view.snapshot(target.agent().unwrap()).await.unwrap().model,
            before_model
        );
        core.shutdown().await;
    }

    #[tokio::test]
    async fn dropping_an_account_directory_observation_cancels_the_pending_http_request() {
        use tickloop_agent_daemon::EmbeddedAgentAccess;
        use tickloop_agent_protocol::{
            AgentAccess, AgentCommand, CommandEnvelope, CommandResult, LaunchSpec,
        };
        use tokio::io::AsyncReadExt;
        let directory = tempfile::tempdir().unwrap();
        let project = directory.path().canonicalize().unwrap();
        let root = project.join("state");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut config = tickloop_agent_model::cards::ModelCatalog::defaults()
            .get("openai-api", "gpt-6.1-sol")
            .unwrap();
        config.base_url = format!("http://{}/v1", listener.local_addr().unwrap());
        config.api_key_env = "PICKER_OBSERVATION_KEY".into();
        tickloop_agent_model::credentials::save_api_key(
            &root,
            &config.api_key_env,
            "synthetic-observation-key",
        )
        .unwrap();
        let core = tickloop_agent_runtime::Agent::new_embedded(
            config.clone(),
            std::sync::Arc::new(tickloop_agent_test_support::ScriptedModel::new(Vec::new())),
        );
        let access = EmbeddedAgentAccess::new(core.clone());
        let mut launch = LaunchSpec::embedded(project);
        launch.config_root = Some(root);
        launch.model = Some(config.clone());
        let receipt = access
            .execute(CommandEnvelope {
                daemon_epoch: access.daemon_epoch(),
                command_id: Uuid::new_v4(),
                command: AgentCommand::Create {
                    launch,
                    initial: None,
                    origin: None,
                },
            })
            .await
            .unwrap();
        let CommandResult::Created { execution, .. } = receipt.into_result().unwrap() else {
            panic!("Agent not created")
        };
        let observation = access
            .observe(tickloop_agent_protocol::ObservationRequest::Agent {
                execution,
                detail: tickloop_agent_protocol::AgentDetailQuery::AccountModels {
                    config: Box::new(config),
                    refresh: false,
                },
                after: None,
            })
            .await
            .unwrap();
        let (mut socket, _) =
            tokio::time::timeout(std::time::Duration::from_secs(5), listener.accept())
                .await
                .unwrap()
                .unwrap();
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            socket.read_exact(&mut byte).await.unwrap();
            request.push(byte[0]);
        }
        assert!(
            String::from_utf8(request)
                .unwrap()
                .starts_with("GET /v1/models ")
        );
        drop(observation);
        let mut byte = [0];
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(5), socket.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        core.shutdown().await;
    }

    #[tokio::test]
    async fn execution_directory_over_ipc_preserves_more_than_eight_kib_of_models() {
        use tickloop_agent_client::LocalAgentClient;
        use tickloop_agent_daemon::EmbeddedAgentAccess;
        use tickloop_agent_protocol::{
            AgentAccess, AgentCommand, CommandEnvelope, CommandResult, LaunchSpec, ViewRef,
        };
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let directory = tempfile::Builder::new()
            .prefix("pmdir-")
            .tempdir_in("/tmp")
            .unwrap();
        let project = directory.path().canonicalize().unwrap();
        let root = project.join("state");
        let http = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut config = tickloop_agent_model::cards::ModelCatalog::defaults()
            .get("openai-api", "gpt-6.1-sol")
            .unwrap();
        config.base_url = format!("http://{}/v1", http.local_addr().unwrap());
        config.api_key_env = "REVIEW_LARGE_DIRECTORY_KEY".into();
        tickloop_agent_model::credentials::save_api_key(
            &root,
            &config.api_key_env,
            "synthetic-large-directory-key",
        )
        .unwrap();
        let response = serde_json::json!({"data": (0..400).map(|index| serde_json::json!({"id":format!("model-{index:04}")})).collect::<Vec<_>>()}).to_string();
        let server_http = tokio::spawn(async move {
            let (mut stream, _) = http.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).await.unwrap();
                request.push(byte[0]);
            }
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",response.len()).as_bytes()).await.unwrap();
        });
        let core = tickloop_agent_runtime::Agent::new_embedded(
            config.clone(),
            std::sync::Arc::new(tickloop_agent_test_support::ScriptedModel::new(Vec::new())),
        );
        let access = std::sync::Arc::new(EmbeddedAgentAccess::new(core.clone()));
        let mut launch = LaunchSpec::embedded(project.clone());
        launch.config_root = Some(root);
        launch.model = Some(config.clone());
        let created = access
            .execute(CommandEnvelope {
                daemon_epoch: access.daemon_epoch(),
                command_id: Uuid::new_v4(),
                command: AgentCommand::Create {
                    launch: launch.clone(),
                    initial: None,
                    origin: None,
                },
            })
            .await
            .unwrap()
            .into_result()
            .unwrap();
        let CommandResult::Created { snapshot, .. } = created else {
            panic!("Agent not created")
        };
        let target = ConfigTarget::Agent(snapshot.conversation.id);
        let endpoint = project.join("daemon.sock");
        let listener = tokio::net::UnixListener::bind(&endpoint).unwrap();
        let epoch = access.daemon_epoch();
        let ipc = tokio::spawn(tickloop_agent_daemon::serve_listener(
            listener,
            access,
            epoch,
            tickloop_agent_test_support::TRANSPORT_LIMITS,
        ));
        let client = std::sync::Arc::new(LocalAgentClient::connect(&endpoint).await.unwrap());
        let view = Agent::from_access_for_test(
            client,
            epoch,
            ViewRef {
                session_id: Uuid::new_v4(),
                view_id: Uuid::new_v4(),
            },
            launch,
            None,
        );
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(12),
            view.account_models(target, config, false),
        )
        .await
        .unwrap();
        server_http.await.unwrap();
        let models = result.expect("A valid model directory must survive the IPC delta limit");
        assert_eq!(models.models.len(), 400);
        assert!(serde_json::to_vec(&models).unwrap().len() > 8192);
        assert_eq!(
            models.availability("model-0399"),
            tickloop_agent_protocol::ModelAvailability::DirectoryListed
        );
        ipc.abort();
        core.shutdown().await;
    }

    #[tokio::test]
    async fn resumed_agent_warmup_and_first_model_page_share_one_execution_directory() {
        use tickloop_agent_daemon::EmbeddedAgentAccess;
        use tickloop_agent_protocol::{
            AgentAccess, AgentCommand, CommandEnvelope, CommandResult, LaunchSpec, ViewRef,
        };
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let directory = tempfile::tempdir().unwrap();
        let project = directory.path().canonicalize().unwrap();
        let root = project.join("state");
        let http = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut config = tickloop_agent_model::cards::ModelCatalog::defaults()
            .get("openai-api", "gpt-6.1-sol")
            .unwrap();
        config.base_url = format!("http://{}/v1", http.local_addr().unwrap());
        config.api_key_env = "REVIEW_WARMUP_KEY".into();
        tickloop_agent_model::credentials::save_api_key(
            &root,
            &config.api_key_env,
            "synthetic-warmup-key",
        )
        .unwrap();
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = calls.clone();
        let http_task = tokio::spawn(async move {
            loop {
                let (mut stream, _) = http.accept().await.unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    stream.read_exact(&mut byte).await.unwrap();
                    request.push(byte[0]);
                }
                observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let body = "{\"data\":[{\"id\":\"gpt-6.1-sol\"}]}";
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
            }
        });
        let core = tickloop_agent_runtime::Agent::new_embedded(
            config.clone(),
            std::sync::Arc::new(tickloop_agent_test_support::ScriptedModel::new(Vec::new())),
        );
        let access = std::sync::Arc::new(EmbeddedAgentAccess::new(core.clone()));
        let mut launch = LaunchSpec::embedded(project);
        launch.config_root = Some(root.clone());
        launch.model = Some(config.clone());
        let result = access
            .execute(CommandEnvelope {
                daemon_epoch: access.daemon_epoch(),
                command_id: Uuid::new_v4(),
                command: AgentCommand::Create {
                    launch: launch.clone(),
                    initial: None,
                    origin: None,
                },
            })
            .await
            .unwrap()
            .into_result()
            .unwrap();
        let CommandResult::Created { snapshot, .. } = result else {
            panic!("Agent not created")
        };
        let target = ConfigTarget::Agent(snapshot.conversation.id);
        let view = Agent::from_access_for_test(
            access.clone(),
            access.daemon_epoch(),
            ViewRef {
                session_id: Uuid::new_v4(),
                view_id: Uuid::new_v4(),
            },
            launch,
            None,
        );
        let (console, _) = Console::start(false).unwrap();
        let mut menus = Menus::new(Some(UserSettings::in_root(&root)), Some(root));
        menus.warm_model_directory(&view, target, config);
        menus.models(&view, &console, target).await.unwrap();
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(12), menus.next_discovery())
                .await
                .unwrap();
        menus
            .finish_discovery(&view, &console, result)
            .await
            .unwrap();
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "Resumed-Agent warmup and the model page must use the same cache"
        );
        http_task.abort();
        core.shutdown().await;
    }

    #[tokio::test]
    async fn model_levels_hide_account_denials_and_keep_unknown_models_selectable() {
        use tickloop_agent_protocol::{AccountModelDirectory, DirectorySemantics};
        let directory = tempfile::tempdir().unwrap();
        let agent = Agent::new_embedded(
            tickloop_agent_test_support::config(),
            std::sync::Arc::new(tickloop_agent_test_support::ScriptedModel::new(Vec::new())),
        );
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        let before = agent.snapshot(chat).await.unwrap().model;
        let (console, _) = Console::start(false).unwrap();
        let catalog = tickloop_agent_model::cards::ModelCatalog::defaults();
        let providers = ProviderModels::group(vec![
            catalog.get("deepseek-api", "deepseek-flash").unwrap(),
            catalog.get("openai-api", "gpt-6-astra").unwrap(),
        ]);
        let mut menus = Menus::default();
        menus
            .configured_model_choices(
                &agent,
                &console,
                ConfigTarget::Agent(chat),
                vec![
                    (
                        providers[0].clone(),
                        Err("Model discovery is not provided".into()),
                    ),
                    (
                        providers[1].clone(),
                        Ok(AccountModelDirectory {
                            models: BTreeMap::new(),
                            semantics: DirectorySemantics::ExclusiveAuthorization,
                            complete: true,
                        }),
                    ),
                ],
            )
            .await
            .unwrap();
        assert_eq!(menus.provider.model_choices.len(), 1);
        assert!(menus.provider.model_choices[0].availability.selectable());
        assert_eq!(
            menus.provider.model_choices[0].config.provider_id,
            "deepseek-api"
        );
        menus
            .back_to_providers(&console, ConfigTarget::Agent(chat))
            .unwrap();
        menus.provider.provider_choices = menus.provider.model_providers.clone();
        menus
            .provider
            .provider_nav
            .back_stack
            .push(menus.directory_parent());
        let openai = menus
            .provider
            .model_providers
            .iter()
            .find(|provider| provider.models[0].provider_id == "openai-api")
            .unwrap()
            .key();
        menus
            .show_provider_models(&console, ConfigTarget::Agent(chat), &openai, None, None)
            .unwrap();
        let (id, Selection::Model { choices, .. }) = menus.active.as_ref().unwrap() else {
            panic!("expected empty model selection for denied Provider");
        };
        assert!(choices.is_empty());
        assert_eq!(
            menus.provider.model_provider.as_deref(),
            Some(openai.as_str())
        );
        let id = *id;
        menus
            .choose(&agent, &console, id, None, None)
            .await
            .unwrap();
        assert!(matches!(
            menus.active.as_ref(),
            Some((_, Selection::Provider { .. }))
        ));
        assert_eq!(agent.snapshot(chat).await.unwrap().model, before);
        agent.shutdown().await;
    }

    struct CountingDirectory(std::sync::atomic::AtomicUsize);

    #[async_trait::async_trait]
    impl tickloop_agent_model::availability::AccountDirectoryAdapter for CountingDirectory {
        async fn identity(
            &self,
            _: &tokio_util::sync::CancellationToken,
        ) -> Result<tickloop_agent_model::availability::DiscoveryIdentity, String> {
            Ok(tickloop_agent_model::availability::DiscoveryIdentity {
                provider: "test-provider".into(),
                origin: "https://api.example.com".into(),
                resource: "models".into(),
                account: "account".into(),
            })
        }
        async fn fetch(
            &self,
            _: &tokio_util::sync::CancellationToken,
        ) -> Result<tickloop_agent_protocol::AccountModelDirectory, String> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(tickloop_agent_protocol::AccountModelDirectory {
                models: BTreeMap::new(),
                semantics: tickloop_agent_protocol::DirectorySemantics::Directory,
                complete: true,
            })
        }
    }

    #[tokio::test]
    async fn access_failures_invalidate_directory_but_transient_failures_reuse_it() {
        use tickloop_agent_protocol::{
            ModelRequestKind, RequestFailure, RequestOutcome, RequestOwner, RequestUsageLedger,
        };
        let directory = tempfile::tempdir().unwrap();
        let agent = Agent::new_embedded(
            tickloop_agent_test_support::config(),
            std::sync::Arc::new(tickloop_agent_test_support::ScriptedModel::new(Vec::new())),
        );
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        let mut snapshot = agent.snapshot(chat).await.unwrap();
        snapshot.model.provider_id = "test-provider".into();
        for (failure, expected_calls) in [
            (RequestFailure::Authentication, 2),
            (RequestFailure::Http { status: 402 }, 2),
            (RequestFailure::Http { status: 403 }, 2),
            (RequestFailure::Http { status: 404 }, 2),
            (RequestFailure::Http { status: 429 }, 2),
            (RequestFailure::RateLimited, 1),
            (RequestFailure::Connection, 1),
            (RequestFailure::Http { status: 503 }, 1),
        ] {
            let mut menus = Menus::default();
            let adapter = CountingDirectory(std::sync::atomic::AtomicUsize::new(0));
            let cancel = tokio_util::sync::CancellationToken::new();
            menus
                .model_discovery
                .discover_with(&adapter, false, &cancel)
                .await
                .unwrap();
            let run = Uuid::new_v4();
            snapshot.request_usage = RequestUsageLedger::new();
            let request = snapshot
                .request_usage
                .start(
                    RequestOwner {
                        chat_id: chat,
                        loop_id: Some(Uuid::new_v4()),
                        run_id: Some(run),
                    },
                    ModelRequestKind::Conversation,
                    snapshot.model.identity(),
                )
                .unwrap();
            snapshot
                .request_usage
                .finish(request, RequestOutcome::Failed(failure))
                .unwrap();
            menus.invalidate_failed_directory(&snapshot, run);
            menus
                .model_discovery
                .discover_with(&adapter, false, &cancel)
                .await
                .unwrap();
            assert_eq!(
                adapter.0.load(std::sync::atomic::Ordering::SeqCst),
                expected_calls,
                "{failure:?}"
            );
        }
        agent.shutdown().await;
    }

    fn unreported(request_count: usize) -> CounterSummary {
        CounterSummary {
            total: CounterTotal::NotReported,
            reported_requests: 0,
            request_count,
            coverage: UsageCoverage::NotReported,
        }
    }

    fn identity(requested_model: &str) -> ModelIdentity {
        ModelIdentity {
            provider: tickloop_agent_protocol::ModelProvider::Api,
            profile: "fixture".into(),
            protocol: Protocol::Messages,
            requested_model: requested_model.into(),
        }
    }

    fn group(model: ModelIdentity, request_count: usize) -> ModelUsageSummary {
        ModelUsageSummary {
            model,
            request_count,
            kind_counts: BTreeMap::new(),
            outcome_counts: BTreeMap::new(),
            input_tokens: unreported(request_count),
            output_tokens: unreported(request_count),
            cache_read_input_tokens: unreported(request_count),
            cache_write_input_tokens: unreported(request_count),
        }
    }

    fn empty_summary() -> UsageSummary {
        UsageSummary {
            request_count: 0,
            kind_counts: BTreeMap::new(),
            outcome_counts: BTreeMap::new(),
            groups: Vec::new(),
        }
    }

    fn prompt_snapshot(prompts: &[&str]) -> ChatSnapshot {
        let mut snapshot: ChatSnapshot = serde_json::from_value(json!({
            "id": Uuid::from_u128(1),
            "workdir": "/workspace",
            "model": {"name":"fixture","protocol":"messages","base_url":"http://127.0.0.1","model":"fixture","api_key_env":"","max_output_tokens":1024},
            "messages": [],
            "loops": [],
            "last_sequence": 0
        }))
        .unwrap();
        for (index, prompt) in prompts.iter().enumerate() {
            snapshot.messages.push(Message {
                role: Role::User,
                content: vec![Content::Text {
                    text: (*prompt).into(),
                }],
            });
            if !prompt.starts_with("Scheduled") {
                snapshot
                    .message_ids
                    .insert(index, Uuid::from_u128(100 + index as u128));
                snapshot
                    .prompt_execution_modes
                    .insert(index, ExecutionMode::AcceptEdits);
            }
        }
        snapshot
    }

    #[test]
    fn rewind_points_list_recorded_prompts_with_single_line_labels() {
        let snapshot = prompt_snapshot(&[
            "真实问题\n第二行\u{1b}[31m",
            "Scheduled trigger: continue checking",
            "Prompt 2",
        ]);
        assert_eq!(
            rewind_points(&snapshot),
            vec![
                RewindPoint {
                    message_id: Uuid::from_u128(100),
                    message_index: 0,
                    prompt: "真实问题 ↵ 第二行[31m".into(),
                    changed_files: None,
                },
                RewindPoint {
                    message_id: Uuid::from_u128(102),
                    message_index: 2,
                    prompt: "Prompt 2".into(),
                    changed_files: None,
                },
            ]
        );
    }

    #[test]
    fn code_actions_are_offered_only_when_restoring_code_changes_files() {
        let mut point = rewind_points(&prompt_snapshot(&["Prompt"])).remove(0);
        let conversation_only = [
            RewindAction::RestoreConversation,
            RewindAction::SummarizeFromHere,
            RewindAction::SummarizeUpToHere,
            RewindAction::Back,
        ];
        assert_eq!(point.actions(), conversation_only);
        assert!(
            point
                .file_history_description()
                .contains("cannot be restored")
        );
        point.changed_files = Some(Vec::new());
        assert_eq!(point.actions(), conversation_only);
        point.changed_files = Some(vec!["/workspace/a.txt".into(), "/workspace/b\n.txt".into()]);
        assert_eq!(
            point.actions()[..3],
            [
                RewindAction::RestoreCodeAndConversation,
                RewindAction::RestoreConversation,
                RewindAction::RestoreCode,
            ]
        );
        let description = point.file_history_description();
        assert!(description.contains("2 tracked files"));
        assert!(!description.contains('\n'));
    }

    #[test]
    fn rewind_status_reports_code_failures_and_the_restored_draft() {
        let snapshot = prompt_snapshot(&["Prompt"]);
        let draft = RewindDraft {
            loop_id: Uuid::from_u128(7),
            prompt: "Prompt".into(),
            contexts: Vec::new(),
        };
        let both = RewindOutcome {
            snapshot: snapshot.clone(),
            mode: RewindMode::CodeAndConversation,
            code: Some(CodeRewind {
                changed_files: vec!["/workspace/a.txt".into()],
                failed_files: Vec::new(),
                error: None,
            }),
            conversation_restored: true,
            draft: Some(draft),
        };
        assert_eq!(
            rewind_outcome_status(&both),
            "Conversation restored · code restored (1 file changed) · prompt restored for editing"
        );
        let failed = RewindOutcome {
            snapshot,
            mode: RewindMode::Code,
            code: Some(CodeRewind {
                changed_files: Vec::new(),
                failed_files: vec![("/workspace/x\u{1b}.txt".into(), "denied\nforged".into())],
                error: None,
            }),
            conversation_restored: false,
            draft: None,
        };
        assert_eq!(
            rewind_outcome_status(&failed),
            "Code partly restored (0 changed, 1 failed)"
        );
        let lines = rewind_code_lines(&failed).join("");
        assert_eq!(lines.lines().count(), 1);
        assert!(!lines.contains('\x1b'));
    }

    #[test]
    fn summary_actions_build_directional_requests_and_drop_blank_guidance() {
        let message_id = Uuid::from_u128(900);
        assert_eq!(
            directed_summary_request(
                RewindAction::SummarizeFromHere,
                message_id,
                Some("focus on API boundaries".into()),
            ),
            Some(DirectedSummaryRequest {
                message_id,
                direction: SummaryDirection::FromHere,
                guidance: Some("focus on API boundaries".into()),
            })
        );
        assert_eq!(
            directed_summary_request(
                RewindAction::SummarizeUpToHere,
                message_id,
                Some("  ".into()),
            ),
            Some(DirectedSummaryRequest {
                message_id,
                direction: SummaryDirection::UpToHere,
                guidance: None,
            })
        );
        assert!(
            directed_summary_request(RewindAction::RestoreConversation, message_id, None).is_none()
        );
    }

    #[test]
    fn usage_projection_preserves_zero_partial_unknown_and_large_counter_facts() {
        let mut recorded = group(identity("fixture"), 3);
        recorded.input_tokens = CounterSummary {
            total: CounterTotal::Value(u128::from(u64::MAX) + 9),
            reported_requests: 3,
            request_count: 3,
            coverage: UsageCoverage::Complete,
        };
        recorded.output_tokens = CounterSummary {
            total: CounterTotal::Value(7),
            reported_requests: 1,
            request_count: 3,
            coverage: UsageCoverage::Partial,
        };
        recorded.cache_read_input_tokens = CounterSummary {
            total: CounterTotal::Value(0),
            reported_requests: 3,
            request_count: 3,
            coverage: UsageCoverage::Complete,
        };
        let summary = UsageSummary {
            request_count: 3,
            groups: vec![recorded],
            ..empty_summary()
        };
        let context = recorded_usage_context(&summary);
        assert!(
            context.contains(&"Input tokens: 18446744073709551624 · reported 3/3 requests".into())
        );
        assert!(
            context.contains(&"Output tokens: 7 reported subtotal · reported 1/3 requests".into())
        );
        assert!(context.contains(&"Cache read tokens: 0 · reported 3/3 requests".into()));
        assert!(
            context.contains(
                &"Cache write/creation tokens: not reported · reported 0/3 requests".into()
            )
        );
        assert!(!context.iter().any(|line| line.contains("Total input")));
    }

    #[test]
    fn usage_projection_keeps_profile_protocol_and_requested_model_groups_separate() {
        let identities = [
            ("DeepSeek", Protocol::Messages, "same-model"),
            ("DeepSeek", Protocol::Responses, "same-model"),
            ("Private gateway", Protocol::ChatCompletions, "same-model"),
            ("Private gateway", Protocol::ChatCompletions, "other-model"),
        ];
        let summary = UsageSummary {
            request_count: identities.len(),
            groups: identities
                .into_iter()
                .map(|(profile, protocol, requested_model)| {
                    group(
                        ModelIdentity {
                            provider: tickloop_agent_protocol::ModelProvider::Api,
                            profile: profile.into(),
                            protocol,
                            requested_model: requested_model.into(),
                        },
                        1,
                    )
                })
                .collect(),
            ..empty_summary()
        };
        let context = recorded_usage_context(&summary);
        assert_eq!(
            context
                .iter()
                .filter(|line| *line == "Model: same-model")
                .count(),
            3
        );
        assert!(context.contains(&"Model: other-model".into()));
        assert_eq!(
            context
                .iter()
                .filter(|line| *line == "Profile: DeepSeek")
                .count(),
            2
        );
        for protocol in ["Messages", "Responses", "Chat Completions"] {
            assert!(context.contains(&format!("Protocol: {protocol}")));
        }
        assert!(
            !context
                .iter()
                .any(|line| line.contains("Anthropic") || line.contains("OpenAI"))
        );
    }

    #[test]
    fn usage_projection_places_chat_counts_before_model_groups_and_preserves_outcomes() {
        let outcomes = BTreeMap::from([
            (RequestOutcome::InFlight, 1),
            (RequestOutcome::Accepted, 2),
            (
                RequestOutcome::HostRejected(ResponseRejection::EmptySummary),
                1,
            ),
            (
                RequestOutcome::Failed(RequestFailure::Http { status: 503 }),
                1,
            ),
            (
                RequestOutcome::Failed(RequestFailure::Protocol(ProtocolFailure::UsageRegression(
                    UsageCounter::CacheRead,
                ))),
                1,
            ),
            (RequestOutcome::TimedOut, 1),
            (RequestOutcome::Cancelled, 1),
        ]);
        let kinds = BTreeMap::from([
            (ModelRequestKind::Conversation, 5),
            (ModelRequestKind::CompactionAutomatic, 1),
            (ModelRequestKind::CompactionManual, 1),
            (ModelRequestKind::CompactionDirected, 1),
            (ModelRequestKind::PermissionCheck, 1),
        ]);
        let summary = UsageSummary {
            request_count: 9,
            kind_counts: kinds.clone(),
            outcome_counts: outcomes.clone(),
            groups: vec![ModelUsageSummary {
                kind_counts: kinds,
                outcome_counts: outcomes,
                ..group(identity("fixture"), 9)
            }],
        };
        let context = recorded_usage_context(&summary);
        let first_model = context
            .iter()
            .position(|line| line.starts_with("Model:"))
            .unwrap();
        assert_eq!(context[0], "Requests in this process: 9");
        for expected in [
            "Conversation: 5",
            "Automatic compaction: 1",
            "Manual compaction: 1",
            "Directed summary: 1",
            "Permission checks: 1",
            "In progress: 1",
            "Accepted responses: 2",
            "Host rejected (empty summary): 1",
            "Failed (HTTP 503): 1",
            "Failed (protocol: cache read counter decreased): 1",
            "Timed out: 1",
            "Cancelled: 1",
        ] {
            assert!(
                context[..first_model].iter().any(|line| line == expected),
                "missing Chat count {expected}"
            );
            assert!(
                context[first_model..].iter().any(|line| line == expected),
                "missing model count {expected}"
            );
        }
    }

    #[test]
    fn usage_projection_sanitizes_model_labels_without_truncating_unicode() {
        let long_name = "深度模型🦀 café ".repeat(30);
        let summary = UsageSummary {
            request_count: 1,
            groups: vec![group(
                ModelIdentity {
                    provider: tickloop_agent_protocol::ModelProvider::Api,
                    profile: "custom\u{1b}[31m\r\nprofile\tname\u{07}".into(),
                    protocol: Protocol::Messages,
                    requested_model: long_name.clone(),
                },
                1,
            )],
            ..empty_summary()
        };
        let context = recorded_usage_context(&summary);
        assert!(context.contains(&format!("Model: {long_name}")));
        assert!(context.contains(&"Profile: custom[31m ↵ profile ⇥ name".into()));
        assert!(
            !context
                .iter()
                .flat_map(|line| line.chars())
                .any(char::is_control)
        );
    }

    #[test]
    fn usage_projection_explains_non_billing_scope_and_typed_overflow() {
        let context = recorded_usage_context(&empty_summary());
        assert!(context.contains(&"Requests in this process: 0".into()));
        assert!(context.contains(&"This Chat has made no model requests in this process.".into()));
        assert!(
            context
                .iter()
                .any(|line| line.contains("not restored after a restart"))
        );
        assert!(context.contains(
            &"Service-reported token counters; not billing, price or account quota.".into()
        ));
        assert!(!context.iter().any(|line| line.contains('$')));
        assert_eq!(
            usage_counter_label(&CounterSummary {
                total: CounterTotal::Overflow,
                reported_requests: 2,
                request_count: 3,
                coverage: UsageCoverage::Partial,
            }),
            "total exceeds the supported range · reported 2/3 requests"
        );
    }
}

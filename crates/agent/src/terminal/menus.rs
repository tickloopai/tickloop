mod judgment;
mod providers;
use providers::{ModelChoice, ProviderModels, ProviderRow};

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::PathBuf,
};

use tickloop_agent::{
    file_change::load_file_change_review,
    model::{ModelConfig, Protocol, ReasoningEffort, cards::ProviderPatch, model_client},
    output_style::OutputStyle,
    permissions::{
        ExecutionMode, PermissionRuleCandidate, PermissionScope, permission_rule_candidates,
    },
    runtime::{
        Agent, AutoReviewerPreference, ChatSnapshot, CounterSummary, CounterTotal,
        DirectedSummaryRequest, ENTER_PLAN_MODE_TOOL_NAME, PermissionApprovalSource,
        PermissionChange, PermissionChangeError, PermissionRequest, PermissionsRevision,
        ProtocolFailure, QuestionRequest, Receipt, RequestFailure, RequestKind, RequestOutcome,
        ResponseRejection, RewindMode, RewindOutcome, SummaryDirection, UsageCounter,
        UsageCoverage, UsageSummary,
    },
    settings::{DisplayPreferences, GlyphSet, UserSettings},
};
use uuid::Uuid;

use super::{
    completion::{CommandGroup, CommandSpec, builtin_commands},
    console::{Console, Menu, PickerAction, PickerKind},
    diff_view::{DiffNavigation, DiffViewport, render_diff, render_error},
    display::{DisplayProfile, Glyph},
    font_manager::{FontAction, FontOutcome, NerdFontManager, SystemFontAdapter},
    presentation::permission_context,
    questions::{FlowUpdate, QuestionFlow, question_lines},
    safe_line, safe_text, settings,
    style::ThemeName,
    workflows::{self, Workflow},
};

enum Selection {
    Permission {
        chat: Uuid,
        request: PermissionRequest,
        goal: String,
    },
    PermissionRule {
        chat: Uuid,
        request: PermissionRequest,
        goal: String,
        candidates: Vec<PermissionRuleCandidate>,
    },
    Provider {
        chat: Uuid,
        choices: Vec<ProviderModels>,
        rows: Vec<ProviderRow>,
    },
    ModelProviders {
        chat: Uuid,
    },
    Model {
        chat: Uuid,
        choices: Vec<ModelChoice>,
    },
    Discovery {
        chat: Uuid,
    },
    ApiKey {
        chat: Uuid,
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
        chat: Uuid,
        choices: Vec<Option<ReasoningEffort>>,
    },
    Settings {
        chat: Uuid,
    },
    Theme {
        /// Present when opened from Settings, so confirming returns there.
        settings_chat: Option<Uuid>,
    },
    GlyphSet {
        chat: Uuid,
        font_action: Option<FontAction>,
    },
    GlyphSetConfirmation {
        chat: Uuid,
        glyph_set: GlyphSet,
    },
    FontActionConfirmation {
        chat: Uuid,
        action: FontAction,
    },
    SettingsInfo {
        chat: Uuid,
    },
    OutputStyle {
        chat: Uuid,
    },
    Mode {
        chat: Uuid,
        choices: Vec<ExecutionMode>,
    },
    PermissionMode {
        chat: Uuid,
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
        chat: Uuid,
    },
    AutoSettings {
        chat: Uuid,
        select_reviewer: bool,
        revision: PermissionsRevision,
    },
}

impl JudgmentReturn {
    fn chat(&self) -> Uuid {
        match self {
            Self::Providers { chat } | Self::AutoSettings { chat, .. } => *chat,
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
    fn chat(&self) -> Option<Uuid> {
        match self {
            Self::Permission { chat, .. }
            | Self::PermissionRule { chat, .. }
            | Self::Provider { chat, .. }
            | Self::ModelProviders { chat }
            | Self::Discovery { chat }
            | Self::ApiKey { chat, .. }
            | Self::Model { chat, .. }
            | Self::Effort { chat, .. }
            | Self::Settings { chat }
            | Self::SettingsInfo { chat }
            | Self::OutputStyle { chat }
            | Self::GlyphSet { chat, .. }
            | Self::GlyphSetConfirmation { chat, .. }
            | Self::FontActionConfirmation { chat, .. }
            | Self::Mode { chat, .. }
            | Self::PermissionMode { chat, .. }
            | Self::Rewind { chat, .. }
            | Self::RewindActions { chat, .. } => Some(*chat),
            Self::JudgmentSettings { return_to, .. } | Self::JudgmentKey { return_to } => {
                Some(return_to.chat())
            }
            Self::Theme { settings_chat } => *settings_chat,
            Self::Question(flow) => Some(flow.request().chat_id),
            Self::Workflow(workflow) => workflow.chat(),
            Self::Help { .. } | Self::HelpReference => None,
        }
    }
}

pub(super) struct Menus {
    active: Option<(Uuid, Selection)>,
    /// Questions whose dialog was closed; they wait for typed answers instead.
    deferred_questions: HashSet<Uuid>,
    /// Answers typed so far, one line per question.
    typed_questions: HashMap<Uuid, QuestionFlow>,
    user_settings: Option<UserSettings>,
    transient_providers: Vec<ProviderPatch>,
    login_return: Option<(Uuid, ProviderModels)>,
    provider_choices: Vec<ProviderModels>,
    provider_selected: usize,
    provider_group: Option<(String, usize)>,
    model_return: Option<providers::ModelPickerDraft>,
    model_choices: Vec<ModelChoice>,
    model_providers: Vec<ProviderModels>,
    model_provider: Option<String>,
    current_model: Option<(String, String)>,
    config_root: Option<PathBuf>,
    model_discovery: tickloop_agent::model::availability::AccountModelDiscovery,
    model_warmup: Option<providers::ModelWarmup>,
    discovery_flow: Option<providers::DiscoveryFlow>,
    model_directory_status: BTreeMap<String, String>,
    font_manager: Option<NerdFontManager<SystemFontAdapter>>,
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
            deferred_questions: HashSet::new(),
            typed_questions: HashMap::new(),
            user_settings,
            transient_providers: Vec::new(),
            login_return: None,
            provider_choices: Vec::new(),
            provider_selected: 0,
            provider_group: None,
            model_return: None,
            model_choices: Vec::new(),
            model_providers: Vec::new(),
            model_provider: None,
            current_model: None,
            config_root: config_root.clone(),
            model_discovery: Default::default(),
            model_warmup: None,
            discovery_flow: None,
            model_directory_status: BTreeMap::new(),
            font_manager: config_root.map(NerdFontManager::production),
        }
    }

    pub(super) fn add_transient_provider(&mut self, provider: ProviderPatch) {
        self.transient_providers
            .retain(|existing| existing.id != provider.id);
        self.transient_providers.push(provider);
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
                        .map(|command| format!("{} — {}", command.syntax(), command.description))
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
                    .map(|command| format!("{} · {}", command.description, command.syntax()))
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
    pub(super) fn id(&self) -> Option<Uuid> {
        self.active.as_ref().map(|(id, _)| *id)
    }

    pub(super) fn dismiss(&mut self, console: &Console) -> Result<(), String> {
        self.discovery_flow.take();
        if let Some((id, _)) = self.active.take() {
            console.clear_menu(id)?;
        }
        Ok(())
    }

    fn show(&mut self, console: &Console, menu: Menu, selection: Selection) -> Result<(), String> {
        self.dismiss(console)?;
        if !console.is_interactive() {
            console.write(&format!("{}\n{}\n", menu.title, menu.context.join("\n")))?;
            for (index, option) in menu.options.iter().enumerate() {
                console.write(&format!("  {}. {}\n", index + 1, option))?;
            }
            console.write("Enter an option number, or cancel.\n")?;
        }
        let id = menu.id;
        if console.is_interactive() {
            if matches!(&selection, Selection::Provider { .. }) {
                console.show_picker_menu(menu, PickerKind::Provider)?;
            } else if matches!(&selection, Selection::ModelProviders { .. }) {
                console.show_picker_menu(menu, PickerKind::ModelProvider)?;
            } else if matches!(&selection, Selection::Model { .. }) {
                console.show_picker_menu(menu, PickerKind::Model)?;
            } else if matches!(
                &selection,
                Selection::Rewind { .. }
                    | Selection::RewindActions { .. }
                    | Selection::PermissionMode { .. }
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
        self.dismiss(console)?;
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
        self.dismiss(console)?;
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
        self.dismiss(console)?;
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
        self.dismiss(console)?;
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
        review: Result<tickloop_agent::file_change::FileChangeReview, String>,
    ) -> Result<(), String> {
        self.dismiss(console)?;
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

    async fn load_providers(&mut self, agent: &Agent, chat: Uuid) -> Result<(), String> {
        let snapshot = agent
            .snapshot(chat)
            .await
            .map_err(|error| error.to_string())?;
        let catalog = self
            .user_settings
            .as_ref()
            .map(UserSettings::model_catalog)
            .transpose()?
            .unwrap_or_else(tickloop_agent::model::cards::ModelCatalog::defaults)
            .with_providers(self.transient_providers.clone())?;
        let defaults = tickloop_agent::model::cards::ModelCatalog::defaults();
        let access = tickloop_agent::model::access::AccessCatalog::defaults();
        access.validate(&catalog)?;
        let profiles = catalog
            .profiles()
            .into_iter()
            .filter(|profile| {
                let explicitly_loaded = self.transient_providers.iter().any(|provider| {
                    provider.id == profile.provider_id
                        && provider.models.as_ref().is_some_and(|models| {
                            models.iter().any(|model| model.id == profile.model_id)
                        })
                });
                access
                    .ordinary_candidate(profile, &defaults)
                    .unwrap_or_else(|| {
                        explicitly_loaded
                            || defaults
                                .get(&profile.provider_id, &profile.model_id)
                                .as_ref()
                                != Some(profile)
                            || tickloop_agent::model::cards::is_default_visible(profile)
                    })
            })
            .collect();
        let mut choices = settings::model_choices_with_profiles(&snapshot.model, profiles);
        if let Some(pending) = &snapshot.pending_model
            && !choices.contains(pending)
            && tickloop_agent::model::access::AccessCatalog::defaults().ordinary_candidate(
                pending,
                &tickloop_agent::model::cards::ModelCatalog::defaults(),
            ) != Some(false)
        {
            choices.push(pending.clone());
        }
        self.current_model = Some((snapshot.model.provider_id, snapshot.model.model_id));
        self.provider_choices = ProviderModels::group(choices);
        self.provider_group = None;
        let rows = providers::provider_rows(&self.provider_choices, None);
        self.provider_selected = rows
            .iter()
            .position(|row| match row {
                ProviderRow::Provider(index) => {
                    self.current_model.as_ref().is_some_and(|(provider, _)| {
                        self.provider_choices[*index].models[0].provider_id == *provider
                    })
                }
                ProviderRow::Group(group) => self.provider_choices.iter().any(|provider| {
                    self.current_model
                        .as_ref()
                        .is_some_and(|(id, _)| provider.models[0].provider_id == *id)
                        && providers::provider_group(provider) == Some(group.as_str())
                }),
            })
            .unwrap_or(0);
        Ok(())
    }

    pub(super) async fn models(
        &mut self,
        agent: &Agent,
        console: &Console,
        chat: Uuid,
    ) -> Result<(), String> {
        self.model_return = None;
        self.load_providers(agent, chat).await?;
        self.configured_models(agent, console, chat).await
    }

    pub(super) async fn providers(
        &mut self,
        agent: &Agent,
        console: &Console,
        chat: Uuid,
    ) -> Result<(), String> {
        self.model_return = None;
        self.load_providers(agent, chat).await?;
        self.back_to_providers(console, chat)
    }

    pub(super) async fn efforts(
        &mut self,
        agent: &Agent,
        console: &Console,
        chat: Uuid,
    ) -> Result<(), String> {
        let snapshot = agent
            .snapshot(chat)
            .await
            .map_err(|error| error.to_string())?;
        let choices = if tickloop_agent::model::validate_reasoning_effort(
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
            Selection::Effort { chat, choices },
        )
    }

    pub(super) async fn settings(
        &mut self,
        agent: &Agent,
        console: &Console,
        chat: Uuid,
    ) -> Result<(), String> {
        let snapshot = agent
            .snapshot(chat)
            .await
            .map_err(|error| error.to_string())?;
        let mut context = vec![
            "Model, Thinking, effort, persistent rules, context window, output style, Glyph Set and reduced motion are saved in user settings; execution mode applies to this process.".into(),
            "Auto-scroll and Theme apply to this terminal session.".into(),
            safe_text(&format!(
                "Current model: {}",
                settings::model_label(&snapshot.model)
            )),
            format!("Execution mode: {}", snapshot.policy.mode),
        ];
        if let Some(pending) = snapshot.pending_model {
            context.push(safe_text(&format!(
                "Pending model: {}",
                settings::model_label(&pending)
            )));
        }
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
                        match tickloop_agent::model::cards::model_card(&snapshot.model)
                            .map(|card| card.thinking)
                        {
                            Some(tickloop_agent::model::cards::ThinkingControl::Toggle) => {
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
                            Some(tickloop_agent::model::cards::ThinkingControl::AlwaysOn) =>
                                "Always on",
                            Some(tickloop_agent::model::cards::ThinkingControl::Unsupported)
                            | None => "Model default",
                        }
                    ),
                    format!(
                        "Thinking: {}",
                        snapshot
                            .reasoning_effort
                            .map_or("auto", ReasoningEffort::label)
                    ),
                    "Execution mode".into(),
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
                ],
                selected: 0,
                confirmation: false,
            },
            Selection::Settings { chat },
        )
    }

    fn saved_output_style(&self) -> OutputStyle {
        self.user_settings
            .as_ref()
            .map_or_else(OutputStyle::default, UserSettings::output_style)
    }

    fn output_styles(&mut self, console: &Console, chat: Uuid) -> Result<(), String> {
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
            Selection::OutputStyle { chat },
        )
    }

    pub(super) async fn glyph_sets(&mut self, console: &Console, chat: Uuid) -> Result<(), String> {
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
                selected: GlyphSet::ALL
                    .into_iter()
                    .position(|glyph_set| glyph_set == current)
                    .unwrap_or(0),
                confirmation: false,
            },
            Selection::GlyphSet { chat, font_action },
        )
    }

    fn confirm_glyph_set(
        &mut self,
        console: &Console,
        chat: Uuid,
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
            Selection::GlyphSetConfirmation { chat, glyph_set },
        )
    }

    fn confirm_font_action(
        &mut self,
        console: &Console,
        chat: Uuid,
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
            Selection::FontActionConfirmation { chat, action },
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
        settings_chat: Option<Uuid>,
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
            Selection::Theme { settings_chat },
        )
    }

    async fn settings_info(
        &mut self,
        agent: &Agent,
        console: &Console,
        chat: Uuid,
        usage: bool,
    ) -> Result<(), String> {
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
            Selection::SettingsInfo { chat },
        )
    }

    pub(super) async fn modes(
        &mut self,
        agent: &Agent,
        console: &Console,
        chat: Uuid,
    ) -> Result<(), String> {
        let current = agent
            .snapshot(chat)
            .await
            .map_err(|error| error.to_string())?
            .policy
            .mode;
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
            Selection::Mode { chat, choices },
        )
    }

    pub(super) async fn permission_modes(
        &mut self,
        agent: &Agent,
        console: &Console,
        chat: Uuid,
        return_to_settings: bool,
    ) -> Result<(), String> {
        let current = agent
            .snapshot(chat)
            .await
            .map_err(|error| error.to_string())?
            .policy
            .mode;
        let modes = settings::execution_modes();
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
        options.push("↗ View recent activity · keep current mode".into());
        let context = if current == ExecutionMode::Plan {
            "Current: Plan. Choose a permission mode to leave Plan, or view activity."
        } else {
            "Choose a mode for this Chat, or view activity without changing it."
        };
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
                chat,
                choices,
                current,
                return_to_settings,
            },
        )
    }

    pub(super) async fn refresh_permissions(
        &mut self,
        agent: &Agent,
        console: &Console,
        chat: Option<Uuid>,
        pending: &HashMap<Uuid, Uuid>,
        questions: &HashMap<Uuid, QuestionRequest>,
    ) -> Result<(), String> {
        self.deferred_questions
            .retain(|id| questions.contains_key(id));
        self.typed_questions
            .retain(|id, _| questions.contains_key(id));
        if let Some((_, selection)) = &self.active {
            let stale = selection.chat().is_some() && selection.chat() != chat
                || matches!(selection, Selection::Permission { request, .. } | Selection::PermissionRule { request, .. } if !pending.contains_key(&request.id))
                || matches!(selection, Selection::Question(flow) if !questions.contains_key(&flow.request().id));
            if stale {
                self.dismiss(console)?;
            }
        }
        if !console.is_interactive()
            || self.active.is_some()
            || pending.is_empty() && questions.is_empty()
        {
            return Ok(());
        }
        if let Some(chat) = chat {
            let snapshot = agent
                .snapshot(chat)
                .await
                .map_err(|error| error.to_string())?;
            if let Some(request) = snapshot
                .permissions
                .iter()
                .find(|request| pending.contains_key(&request.id))
            {
                let goal = snapshot
                    .loops
                    .iter()
                    .find(|goal| goal.id == request.loop_id);
                let context = safe_text(&format!(
                    "Goal: {} · {}",
                    goal.map(|goal| goal.goal.as_str()).unwrap_or("Task"),
                    &request.loop_id.to_string()[..8],
                ));
                self.permission(console, chat, request, context)?;
            } else if let Some(request) = snapshot.questions.iter().find(|request| {
                !self.deferred_questions.contains(&request.id)
                    && questions.contains_key(&request.id)
            }) {
                let goal = snapshot
                    .loops
                    .iter()
                    .find(|goal| goal.id == request.loop_id)
                    .map(|goal| goal.goal.as_str())
                    .unwrap_or("Task");
                self.show_question(console, QuestionFlow::new(request.clone(), goal.into()))?;
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
    ) -> Result<(), String> {
        if request.name == ENTER_PLAN_MODE_TOOL_NAME {
            let menu = enter_plan_mode_menu(request.id, goal.clone());
            let selection = Selection::Permission {
                chat,
                request: request.clone(),
                goal,
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
        } else {
            vec![
                "Yes".into(),
                "Yes, for this session".into(),
                persistent_option,
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
        };
        let denial_index = if explicit_ask { 1 } else { 3 };
        let has_prefix_choices = !explicit_ask && rule_candidates.len() > 1;
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
            },
        )) = self.active.as_ref()
        else {
            return Err("this menu option cannot be expanded".into());
        };
        if *active_id != menu_id
            || index != 2
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
        if self.id() != Some(menu_id) {
            console.finish_menu_submission(menu_id)?;
            return Ok(MenuOutcome::default());
        }
        let (_, selection) = self.active.take().unwrap();
        let outcome: Result<MenuOutcome, String> = async {
            match selection {
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
                Selection::Permission {
                    chat,
                    request,
                    goal: _,
                } if request.name == ENTER_PLAN_MODE_TOOL_NAME => {
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
                    goal: _,
                } => {
                    let has_comment = comment
                        .as_ref()
                        .is_some_and(|comment| !comment.trim().is_empty());
                    let explicit_ask =
                        request.approval_source == PermissionApprovalSource::ExplicitAsk;
                    if !explicit_ask
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
                        match index {
                            Some(0) => (true, PermissionScope::Once),
                            Some(1) => (true, PermissionScope::Session),
                            Some(2) => (true, PermissionScope::Persistent),
                            Some(3) | None => (false, PermissionScope::Once),
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
                        self.permission(console, chat, &request, goal)?;
                        return Ok(MenuOutcome::default());
                    };
                    let Some(candidate) = candidates.get(index) else {
                        if index == candidates.len() {
                            self.permission(console, chat, &request, goal)?;
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
                    let effect = workflow.choose(agent, console, index).await?;
                    if let Some(next) = effect.next {
                        self.workflow(console, next)?;
                    }
                    if let Some((chat, goal)) = effect.timer {
                        console.clear_menu(menu_id)?;
                        self.edit_loop_timer(agent, console, chat, goal).await?;
                    }
                    return Ok(MenuOutcome {
                        selection: effect.selection,
                        restored: effect.restored,
                        receipt: effect.receipt,
                        ..Default::default()
                    });
                }
                Selection::SettingsInfo { chat } => self.settings(agent, console, chat).await?,
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
                Selection::Provider { chat, choices, rows } => {
                    if let Some(index) = index {
                        if self.provider_group.is_none() && index == rows.len() + 1 {
                            let provider = choices.iter().find(|provider| provider.models[0].auth_source == tickloop_agent::model::ModelAuthSource::CodexSubscription).cloned().ok_or("Codex subscription Provider is not configured.")?;
                            self.provider_choices = choices;
                            self.provider_selected = index;
                            self.dismiss(console)?;
                            self.login_return = Some((chat, provider));
                            return Ok(MenuOutcome { provider_logout: true, logout_target: super::login::SignInTarget::Codex, ..Default::default() });
                        }
                        if self.provider_group.is_none() && index == rows.len() {
                            self.provider_choices = choices;
                            self.provider_selected = index;
                            if let Err(error) = self.judgment_settings(
                                agent,
                                console,
                                JudgmentReturn::Providers { chat },
                            )
                            .await {
                                self.back_to_providers(console, chat)?;
                                return Err(error);
                            }
                            return Ok(MenuOutcome::default());
                        }
                        let row = rows.get(index).ok_or("provider option does not exist")?;
                        if let Some((_, selected)) = &mut self.provider_group { *selected = index; } else { self.provider_selected = index; }
                        let provider_index = match row {
                            ProviderRow::Group(name) => {
                                self.provider_group = Some((name.clone(), 0));
                                self.provider_choices = choices.clone();
                                self.show_providers(console, chat, choices, 0, Uuid::new_v4())?;
                                return Ok(MenuOutcome::default());
                            }
                            ProviderRow::Provider(provider_index) => *provider_index,
                        };
                        let provider = choices.get(provider_index).ok_or("provider option does not exist")?.clone();
                        self.provider_choices = choices;
                        match self.provider_authenticated(&provider) {
                            Ok(true) | Ok(false) => return self.provider_auth(console, chat, provider),
                            Err(error) => {
                                self.back_to_providers(console, chat)?;
                                return Err(error);
                            }
                        }
                    } else if self.provider_group.take().is_some() {
                        self.back_to_providers(console, chat)?;
                    } else if self.model_return.is_some() {
                        self.configured_models(agent, console, chat).await?;
                    } else {
                        console.write("Selection cancelled; configuration unchanged.\n")?;
                    }
                }
                Selection::ApiKey { chat, provider } => {
                    if index == Some(0) {
                        let root = self.config_root.as_deref().ok_or("Cannot locate the Agent state directory; set TICKLOOP_CONFIG_DIR.")?;
                        let result = tickloop_agent::model::credentials::save_api_key(root, &provider.models[0].api_key_env, comment.as_deref().unwrap_or(""));
                        if let Err(error) = result {
                            self.provider_api_key(console, chat, provider)?;
                            return Err(error.to_string());
                        }
                        self.model_discovery.invalidate(&provider.models[0].provider_id);
                        console.write("API key saved to Agent .env.\n")?;
                        self.finish_provider_configuration(agent, console, chat).await?;
                    } else {
                        self.finish_provider_configuration(agent, console, chat).await?;
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
                Selection::Discovery { chat } => {
                    if let Some(flow) = self.discovery_flow.take() {
                        let providers = flow.providers.clone();
                        drop(flow);
                        let directories = providers.into_iter().map(|provider| (provider, Err("Account model check cancelled".into()))).collect();
                        self.configured_model_choices(agent, console, chat, directories).await?;
                    }
                }
                Selection::ModelProviders { chat } => {
                    if let Some(index) = index {
                        if index == self.model_providers.len() {
                            self.open_model_provider_configuration(console, chat, None)?;
                        } else {
                            let provider = self.model_providers.get(index).ok_or("provider option does not exist")?.models[0].provider_id.clone();
                            self.model_return = Some(providers::ModelPickerDraft {
                                selected: None,
                                choices: self.model_choices.clone(),
                                provider: Some(provider),
                            });
                            self.configured_models(agent, console, chat).await?;
                        }
                    }
                }
                Selection::Model { chat, choices } => {
                    let Some(index) = index else {
                        self.remember_model_drafts(&choices);
                        let provider = self.model_provider.clone();
                        self.show_model_providers(console, chat, provider.as_deref())?;
                        return Ok(MenuOutcome::default());
                    };
                    if index == choices.len() {
                        self.open_model_providers(console, chat, choices, index)?;
                        return Ok(MenuOutcome::default());
                    }
                    if index == choices.len() + 1 {
                        self.remember_model_drafts(&choices);
                        self.refresh_model_choices(agent, console, chat).await?;
                        return Ok(MenuOutcome::default());
                    }
                    let choice = choices.get(index).ok_or("model option does not exist")?;
                    if !choice.availability.selectable() {
                        let error = choice.availability.label().to_owned();
                        self.show_model_choices(console, chat, choices, index, Uuid::new_v4())?;
                        return Err(error);
                    }
                    let result: Result<_, String> = async {
                        let client = model_client(choice.config.clone(), self.config_root.as_deref())
                            .map_err(|error| error.to_string())?;
                        agent.set_model_with_thinking(chat, choice.config.clone(), client, choice.thinking)
                            .await.map_err(|error| error.to_string())
                    }.await;
                    match result {
                        Ok(change) => console.write(&safe_text(&format!("Model selection: {change:?} · {} · Thinking {}\n", choice.config.name, choice.thinking.map_or("auto", ReasoningEffort::label))))?,
                        Err(error) => {
                            self.show_model_choices(console, chat, choices, index, Uuid::new_v4())?;
                            return Err(error);
                        }
                    }
                }
                Selection::PermissionMode {
                    chat,
                    return_to_settings: true,
                    ..
                } if index.is_none() => self.settings(agent, console, chat).await?,
                Selection::PermissionMode { .. } if index.is_none() => {},
                _ if index.is_none() => {
                    console.write("Selection cancelled; configuration unchanged.\n")?
                }
                Selection::Effort { chat, choices } => {
                    let effort = *choices
                        .get(index.unwrap())
                        .ok_or("effort option does not exist")?;
                    agent
                        .set_reasoning_effort(chat, effort)
                        .await
                        .map_err(|error| error.to_string())?;
                    console.write(&format!(
                        "Thinking: {}\n",
                        effort.map_or("auto", ReasoningEffort::label)
                    ))?;
                }
                Selection::Settings { chat } => match index {
                    Some(0) => self.models(agent, console, chat).await?,
                    Some(1) => {
                        let snapshot = agent.snapshot(chat).await.map_err(|error| error.to_string())?;
                        if matches!(
                            tickloop_agent::model::cards::model_card(&snapshot.model)
                                .map(|card| card.thinking),
                            Some(tickloop_agent::model::cards::ThinkingControl::Toggle)
                        ) {
                            agent.set_thinking_enabled(chat, !snapshot.model.thinking_enabled(snapshot.reasoning_effort, snapshot.thinking_enabled).unwrap_or(snapshot.thinking_enabled)).await.map_err(|error| error.to_string())?;
                        } else {
                            console.write("Thinking toggle is unavailable for this model.\n")?;
                        }
                        self.settings(agent, console, chat).await?;
                    }
                    Some(2) => self.efforts(agent, console, chat).await?,
                    Some(3) => self.modes(agent, console, chat).await?,
                    Some(4) => self.settings_info(agent, console, chat, false).await?,
                    Some(5) => self.settings_info(agent, console, chat, true).await?,
                    Some(6) => {
                        console.set_auto_scroll(!console.auto_scroll()?)?;
                        self.settings(agent, console, chat).await?;
                    }
                    Some(7) => self.themes(console, Some(chat))?,
                    Some(8) => self.glyph_sets(console, chat).await?,
                    Some(9) => {
                        let mut preferences = console.display_preferences()?;
                        preferences.prefers_reduced_motion = !preferences.prefers_reduced_motion;
                        self.save_display_preferences(console, preferences)?;
                        self.settings(agent, console, chat).await?;
                    }
                    Some(10) => self.output_styles(console, chat)?,
                    Some(11) => {
                        match self.permission_modes(agent, console, chat, true).await {
                            Ok(()) => {}
                            Err(error) => {
                                self.settings(agent, console, chat).await?;
                                console.write(&format!(
                                    "Cannot open Permissions: {}.\n",
                                    super::safe_text(&error)
                                ))?;
                            },
                        }
                    }
                    _ => return Err("setting does not exist".into()),
                },
                Selection::Theme { settings_chat } => {
                    let theme = *ThemeName::ALL
                        .get(index.unwrap())
                        .ok_or("theme option does not exist")?;
                    console.set_theme(theme)?;
                    match settings_chat {
                        Some(chat) => self.settings(agent, console, chat).await?,
                        None => console.write(&format!("Theme: {}\n", theme.label()))?,
                    }
                }
                Selection::OutputStyle { chat } => {
                    let style = *OutputStyle::ALL
                        .get(index.unwrap())
                        .ok_or("output style option does not exist")?;
                    self.user_settings
                        .as_ref()
                        .ok_or("cannot save the output style because the user settings directory is unavailable")?
                        .save_output_style(style)
                        .map_err(|error| error.to_string())?;
                    self.settings(agent, console, chat).await?;
                }
                Selection::GlyphSet { chat, font_action } => {
                    let selected = index.unwrap();
                    if let Some(glyph_set) = GlyphSet::ALL.get(selected).copied() {
                        self.confirm_glyph_set(console, chat, glyph_set)?;
                    } else if selected == GlyphSet::ALL.len() {
                        self.confirm_font_action(
                            console,
                            chat,
                            font_action.ok_or("font action does not exist")?,
                        )?;
                    } else {
                        return Err("Glyph Set option does not exist".into());
                    }
                }
                Selection::GlyphSetConfirmation { chat, glyph_set } => match index {
                    Some(0) => {
                        let mut preferences = console.display_preferences()?;
                        preferences.glyph_set = glyph_set;
                        self.save_display_preferences(console, preferences)?;
                        self.settings(agent, console, chat).await?;
                    }
                    Some(1) => self.glyph_sets(console, chat).await?,
                    _ => return Err("glyph confirmation option does not exist".into()),
                },
                Selection::FontActionConfirmation { chat, action } => match index {
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
                        self.glyph_sets(console, chat).await?;
                    }
                    Some(1) => self.glyph_sets(console, chat).await?,
                    _ => return Err("font action confirmation does not exist".into()),
                },
                Selection::Mode { chat, choices } => {
                    let mode = *choices
                        .get(index.unwrap())
                        .ok_or("execution mode does not exist")?;
                    agent
                        .set_execution_mode(chat, mode)
                        .await
                        .map_err(|error| error.to_string())?;
                    console.write(&format!(
                        "Execution mode: {mode}. Explicit deny rules remain effective.\n"
                    ))?;
                }
                Selection::PermissionMode {
                    chat,
                    choices,
                    current,
                    return_to_settings,
                } => {
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
                                    .snapshot(chat)
                                    .await
                                    .map_err(|error| error.to_string())?
                                    .policy
                                    .mode;
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
                                self.permission_modes(agent, console, chat, return_to_settings)
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
        console.finish_menu_submission(menu_id)?;
        outcome
    }
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
            "In plan mode, the Agent will:".into(),
            " · Explore the codebase thoroughly".into(),
            " · Identify existing patterns".into(),
            " · Design an implementation strategy".into(),
            " · Present a plan for your approval".into(),
            "No changes will be made until you approve the plan.".into(),
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

fn request_kind_label(kind: RequestKind) -> &'static str {
    match kind {
        RequestKind::Conversation => "Conversation",
        RequestKind::CompactionAutomatic => "Automatic compaction",
        RequestKind::CompactionManual => "Manual compaction",
        RequestKind::CompactionDirected => "Directed summary",
        RequestKind::PermissionCheck => "Permission checks",
        RequestKind::WebFetch => "WebFetch",
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
    use tickloop_agent::{
        model::{Content, Message, ModelIdentity, Role},
        runtime::{CodeRewind, ModelUsageSummary, RewindDraft},
    };

    use super::*;

    #[tokio::test]
    async fn model_levels_hide_account_denials_and_keep_unknown_models_selectable() {
        use crate::terminal::test_support as support;
        use tickloop_agent::model::availability::{AccountModelDirectory, DirectorySemantics};
        let directory = tempfile::tempdir().unwrap();
        let agent = Agent::new_embedded(
            support::config(),
            std::sync::Arc::new(support::ScriptedModel::new(Vec::new())),
        );
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        let before = agent.snapshot(chat).await.unwrap().model;
        let (console, _) = Console::start(false).unwrap();
        let catalog = tickloop_agent::model::cards::ModelCatalog::defaults();
        let providers = ProviderModels::group(vec![
            catalog.get("deepseek-api", "deepseek-flash").unwrap(),
            catalog.get("openai-api", "gpt-6-astra").unwrap(),
        ]);
        let mut menus = Menus::default();
        menus
            .configured_model_choices(
                &agent,
                &console,
                chat,
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
        assert_eq!(menus.model_choices.len(), 1);
        assert!(menus.model_choices[0].availability.selectable());
        assert_eq!(menus.model_choices[0].config.provider_id, "deepseek-api");
        menus.show_model_providers(&console, chat, None).unwrap();
        menus
            .show_provider_models(&console, chat, "openai-api", None)
            .unwrap();
        let (id, Selection::Model { choices, .. }) = menus.active.as_ref().unwrap() else {
            panic!("expected empty model selection for denied Provider");
        };
        assert!(choices.is_empty());
        assert_eq!(menus.model_provider.as_deref(), Some("openai-api"));
        let id = *id;
        menus
            .choose(&agent, &console, id, None, None)
            .await
            .unwrap();
        assert!(matches!(
            menus.active.as_ref(),
            Some((_, Selection::ModelProviders { .. }))
        ));
        assert_eq!(agent.snapshot(chat).await.unwrap().model, before);
        agent.shutdown().await;
    }

    struct CountingDirectory(std::sync::atomic::AtomicUsize);

    #[async_trait::async_trait]
    impl tickloop_agent::model::availability::AccountDirectoryAdapter for CountingDirectory {
        async fn identity(
            &self,
            _: &tokio_util::sync::CancellationToken,
        ) -> Result<tickloop_agent::model::availability::DiscoveryIdentity, String> {
            Ok(tickloop_agent::model::availability::DiscoveryIdentity {
                provider: "test-provider".into(),
                origin: "https://api.example.com".into(),
                resource: "models".into(),
                account: "account".into(),
            })
        }
        async fn fetch(
            &self,
            _: &tokio_util::sync::CancellationToken,
        ) -> Result<tickloop_agent::model::availability::AccountModelDirectory, String> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(tickloop_agent::model::availability::AccountModelDirectory {
                models: BTreeMap::new(),
                semantics: tickloop_agent::model::availability::DirectorySemantics::Directory,
                complete: true,
            })
        }
    }

    #[tokio::test]
    async fn access_failures_invalidate_directory_but_transient_failures_reuse_it() {
        use crate::terminal::test_support as support;
        use tickloop_agent::runtime::{
            RequestFailure, RequestKind, RequestOutcome, RequestOwner, RequestUsageLedger,
        };
        let directory = tempfile::tempdir().unwrap();
        let agent = Agent::new_embedded(
            support::config(),
            std::sync::Arc::new(support::ScriptedModel::new(Vec::new())),
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
            let menus = Menus::default();
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
                    RequestKind::Conversation,
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
            provider: crate::model::ModelProvider::Api,
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
                            provider: crate::model::ModelProvider::Api,
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
            (RequestKind::Conversation, 5),
            (RequestKind::CompactionAutomatic, 1),
            (RequestKind::CompactionManual, 1),
            (RequestKind::CompactionDirected, 1),
            (RequestKind::PermissionCheck, 1),
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
                    provider: crate::model::ModelProvider::Api,
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

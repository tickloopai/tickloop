use super::super::{
    artifact_ui, safe_text,
    task_access::ShellTaskCenter,
    task_surface::{TaskOutputUpdate, TaskSurfaceEffect},
    tick_shell::{TickShell, TickShellKey},
};
use super::*;
use crate::monitor_rows::{HOSTED_CLOSE_KEY, HOSTED_REOPEN_KEY, TickIntent};

/// The Console executor that carries out a Monitor row's intent. A notice has none: the Console
/// only shows it.
pub(in crate::terminal) fn executor(intent: &TickIntent) -> Option<Arc<dyn TickActionExecutor>> {
    let executor: Arc<dyn TickActionExecutor> = match intent.clone() {
        TickIntent::Notify(_) => return None,
        TickIntent::ShowDetails(instance) => Arc::new(DetailsOpen { instance }),
        TickIntent::RemoveFromMonitor(instance) => Arc::new(RemoveFromMonitor { instance }),
        TickIntent::OpenHosted(instance) => Arc::new(OpenHosted { instance }),
        TickIntent::ShowHostedSnapshot(instance) => Arc::new(SnapshotHosted { instance }),
        TickIntent::CloseHosted(instance) => Arc::new(CloseHosted {
            instance,
            is_confirmed: false,
        }),
        TickIntent::ReopenHosted(instance) => Arc::new(ReopenHosted {
            instance,
            is_confirmed: false,
        }),
        TickIntent::OpenShell { task } => Arc::new(ShellOpen { task }),
        TickIntent::Artifact { id, key } => artifact(&id, key),
        TickIntent::RefreshArtifacts => artifact("", 'r'),
        TickIntent::OpenSubscription { id } => Arc::new(SubscriptionAction { id }),
    };
    Some(executor)
}
pub(in crate::terminal) fn artifact(id: &str, key: char) -> Arc<dyn TickActionExecutor> {
    Arc::new(ArtifactAction { id: id.into(), key })
}
pub(in super::super) fn load_hosted_snapshot(
    instance: tick_protocol::InstanceRef,
    panel: Uuid,
) -> PreparedTickAction {
    PreparedTickAction::new(
        instance.to_string(),
        super::super::hosted_detail::REFRESH_KEY,
        Arc::new(LoadHostedSnapshot { instance, panel }),
    )
}
/// Reloads everything the registered rows' refresh intents name, in one action.
pub(in crate::terminal) fn refresh(intents: &[TickIntent]) -> Arc<dyn TickActionExecutor> {
    Arc::new(Refresh {
        actions: intents.iter().filter_map(executor).collect(),
    })
}
struct ArtifactAction {
    id: String,
    key: char,
}
impl TickActionExecutor for ArtifactAction {
    fn prepare(&self, _ui: &mut TickActionUi<'_>) -> Result<PreparedTickAction, String> {
        Ok(PreparedTickAction::new(
            &self.id,
            self.key,
            Arc::new(ArtifactAction {
                id: self.id.clone(),
                key: self.key,
            }),
        ))
    }
}
#[async_trait]
impl TickActionBackend for ArtifactAction {
    async fn execute(&self, ctx: TickActionContext<'_>) -> Result<TickActionOutcome, String> {
        match artifact_ui::tick_action(
            ctx.agent,
            ctx.console,
            ctx.root,
            ctx.chat,
            self.id.clone(),
            self.key,
        )
        .await?
        {
            Some(InputEvent::Prompt { text, .. }) => Ok(TickActionOutcome::Prompt(text)),
            _ => Ok(TickActionOutcome::Done),
        }
    }
}
struct SubscriptionAction {
    id: Uuid,
}
impl TickActionExecutor for SubscriptionAction {
    fn prepare(&self, _ui: &mut TickActionUi<'_>) -> Result<PreparedTickAction, String> {
        Ok(PreparedTickAction::new(
            self.id.to_string(),
            'o',
            Arc::new(SubscriptionAction { id: self.id }),
        ))
    }
}
#[async_trait]
impl TickActionBackend for SubscriptionAction {
    async fn execute(&self, ctx: TickActionContext<'_>) -> Result<TickActionOutcome, String> {
        let rows = ctx
            .agent
            .tick_subscriptions(ctx.chat)
            .await
            .map_err(|error| error.to_string())?;
        ctx.console
            .show_subscriptions_at(ctx.chat, rows, self.id, false)?;
        Ok(TickActionOutcome::Done)
    }
}
struct DetailsOpen {
    instance: tick_protocol::InstanceRef,
}
impl TickActionExecutor for DetailsOpen {
    fn prepare(&self, ui: &mut TickActionUi<'_>) -> Result<PreparedTickAction, String> {
        let chat = ui.chat().ok_or("No selected Chat")?;
        let entry = ui
            .monitor_entry(&self.instance)
            .ok_or("This Tick is no longer in Monitor")?;
        ui.open_detail(Box::new(super::super::monitor::MonitorDetail::new(
            chat, entry,
        )));
        Ok(PreparedTickAction::new(
            self.instance.to_string(),
            'o',
            Arc::new(NoBackend),
        ))
    }
}
/// Enter on a hosted Tick opens it at once: inside Herdr in Tick Loop's Herdr popup, otherwise
/// full screen in this View's terminal. Either view shows the same process and Esc closes it.
struct OpenHosted {
    instance: tick_protocol::InstanceRef,
}
impl TickActionExecutor for OpenHosted {
    fn prepare(&self, ui: &mut TickActionUi<'_>) -> Result<PreparedTickAction, String> {
        ui.monitor_entry(&self.instance)
            .ok_or("This Tick is no longer in Monitor")?;
        Ok(PreparedTickAction::new(
            self.instance.to_string(),
            crate::monitor_rows::MONITOR_ENTER_KEY,
            Arc::new(OpenHosted {
                instance: self.instance.clone(),
            }),
        ))
    }
}
#[async_trait]
impl TickActionBackend for OpenHosted {
    async fn execute(&self, ctx: TickActionContext<'_>) -> Result<TickActionOutcome, String> {
        let floated = match HerdrPopup::reach(&ctx).await {
            Ok(None) => return open_full_screen(&ctx, &self.instance).await,
            Ok(Some(popup)) => match popup.check(&ctx).await {
                Ok(()) => popup.float(&ctx, &self.instance).await,
                Err(reason)
                    if reason.offers_registration()
                        && ctx.console.take_popup_registration_offer()? =>
                {
                    ctx.console.confirm_tick_action(
                        ctx.chat,
                        self.instance.to_string(),
                        Arc::new(RegisterPopup {
                            instance: self.instance.clone(),
                        }),
                        reason.registration_prompt(),
                    )?;
                    return Ok(TickActionOutcome::Done);
                }
                Err(reason) => Err(reason),
            },
            Err(reason) => Err(reason),
        };
        settle_float(&ctx, &self.instance, floated).await
    }
}

const REGISTER_POPUP_PROMPT: &str =
    "Register Tick Loop's Herdr popup plugin? No other plugin is touched.";
const TURN_ON_POPUP_PROMPT: &str = "Turn Tick Loop's Herdr popup on? No other plugin is touched.";
const REGISTER_THIS_TICKLOOP_PROMPT: &str =
    "Tick Loop's Herdr popup runs another tickloop. Register this one instead?";
const HERDR_BUSY: &str = "Herdr is showing another view; close it and press Enter again";

/// Why a hosted Tick did not float in the Herdr popup.
enum NotFloated {
    NotRegistered,
    /// Our plugin is disabled in Herdr, or `monitorPopup` is off.
    TurnedOff,
    /// Our plugin runs another tickloop build's attach client.
    Outdated,
    /// The popup cannot show it here, such as when another plugin holds our id; nothing opened.
    Unavailable,
    /// The user confirmed registering our plugin, but it could not be registered.
    RegistrationFailed(String),
    /// Herdr is showing something other than its workspace view, such as another popup.
    Busy,
    /// Herdr may be showing the popup; opening again could show the Tick twice.
    Unknown(String),
}

impl From<crate::terminal::popup::FloatError> for NotFloated {
    fn from(error: crate::terminal::popup::FloatError) -> Self {
        use crate::terminal::popup::FloatError;
        match error {
            FloatError::Busy => Self::Busy,
            FloatError::Refused => Self::Unavailable,
            FloatError::Unknown(reason) => Self::Unknown(reason),
        }
    }
}

impl NotFloated {
    fn offers_registration(&self) -> bool {
        matches!(self, Self::NotRegistered | Self::TurnedOff | Self::Outdated)
    }

    fn registration_prompt(&self) -> &'static str {
        match self {
            Self::TurnedOff => TURN_ON_POPUP_PROMPT,
            Self::Outdated => REGISTER_THIS_TICKLOOP_PROMPT,
            _ => REGISTER_POPUP_PROMPT,
        }
    }
}

/// This View's Herdr session together with Tick Loop's popup plugin for it.
struct HerdrPopup {
    herdr: crate::terminal::popup::HerdrSession,
    plugin: crate::terminal::popup::PopupPlugin,
    settings: tickloop_agent_config::UserSettings,
}

impl HerdrPopup {
    /// Verifies the Herdr session this View runs inside; `None` outside Herdr.
    async fn reach(ctx: &TickActionContext<'_>) -> Result<Option<Self>, NotFloated> {
        use crate::terminal::popup::{HerdrSession, PopupPlugin};
        let Some(socket) = ctx
            .console
            .herdr_socket()
            .map_err(|_| NotFloated::Unavailable)?
        else {
            return Ok(None);
        };
        let root = ctx.root.ok_or(NotFloated::Unavailable)?;
        Ok(Some(Self {
            herdr: HerdrSession::verify(&socket)
                .await
                .map_err(|_| NotFloated::Unavailable)?,
            plugin: PopupPlugin::for_this_executable(root).map_err(|_| NotFloated::Unavailable)?,
            settings: tickloop_agent_config::UserSettings::in_root(root),
        }))
    }

    /// Whether our plugin is registered and turned on in this session.
    async fn check(&self, ctx: &TickActionContext<'_>) -> Result<(), NotFloated> {
        use crate::terminal::popup::PopupReadiness;
        match self
            .herdr
            .readiness(&self.plugin)
            .await
            .map_err(|_| NotFloated::Unavailable)?
        {
            PopupReadiness::Ready => {}
            PopupReadiness::NotInstalled => return Err(NotFloated::NotRegistered),
            PopupReadiness::Disabled => return Err(NotFloated::TurnedOff),
            PopupReadiness::Outdated => return Err(NotFloated::Outdated),
            PopupReadiness::Foreign { .. } => return Err(NotFloated::Unavailable),
        }
        if ctx
            .console
            .monitor_popup()
            .map_err(|_| NotFloated::Unavailable)?
            == tickloop_agent_config::MonitorPopup::Disabled
        {
            return Err(NotFloated::TurnedOff);
        }
        Ok(())
    }

    /// Links our plugin enabled and turns `monitorPopup` on; the user has just confirmed.
    async fn register(&self, ctx: &TickActionContext<'_>) -> Result<(), NotFloated> {
        self.herdr
            .install(&self.plugin, &self.settings)
            .await
            .map_err(NotFloated::RegistrationFailed)?;
        ctx.console
            .set_monitor_popup(tickloop_agent_config::MonitorPopup::Enabled)
            .map_err(NotFloated::RegistrationFailed)
    }

    /// Opens the popup on `instance` with a one-time view grant.
    async fn float(
        &self,
        ctx: &TickActionContext<'_>,
        instance: &tick_protocol::InstanceRef,
    ) -> Result<(), NotFloated> {
        let grant = ctx
            .agent
            .attach_hosted_tick(ctx.chat, instance.clone())
            .await
            .map_err(|_| NotFloated::Unavailable)?;
        Ok(self.herdr.float(&grant).await?)
    }
}

/// Registers, or turns on, Tick Loop's popup plugin in this View's Herdr session once the user
/// confirmed, then floats the Tick.
struct RegisterPopup {
    instance: tick_protocol::InstanceRef,
}
impl TickActionExecutor for RegisterPopup {
    fn prepare(&self, _ui: &mut TickActionUi<'_>) -> Result<PreparedTickAction, String> {
        Ok(PreparedTickAction::new(
            self.instance.to_string(),
            crate::monitor_rows::MONITOR_ENTER_KEY,
            Arc::new(RegisterPopup {
                instance: self.instance.clone(),
            }),
        ))
    }
}
#[async_trait]
impl TickActionBackend for RegisterPopup {
    async fn execute(&self, ctx: TickActionContext<'_>) -> Result<TickActionOutcome, String> {
        let floated = async {
            let popup = HerdrPopup::reach(&ctx)
                .await?
                .ok_or(NotFloated::Unavailable)?;
            popup.register(&ctx).await?;
            popup.float(&ctx, &self.instance).await
        }
        .await;
        settle_float(&ctx, &self.instance, floated).await
    }
}

/// Leaves a floated Tick to its popup and opens it full screen when nothing can be showing it;
/// otherwise only says what keeps it from opening.
async fn settle_float(
    ctx: &TickActionContext<'_>,
    instance: &tick_protocol::InstanceRef,
    floated: Result<(), NotFloated>,
) -> Result<TickActionOutcome, String> {
    match floated {
        Ok(()) => {}
        Err(NotFloated::Busy) => ctx.console.hold_status(HERDR_BUSY)?,
        Err(NotFloated::Unknown(reason)) => ctx.console.hold_status(&format!(
            "The Herdr popup may have opened ({reason}); look in Herdr before opening again"
        ))?,
        Err(NotFloated::RegistrationFailed(reason)) => ctx.console.hold_status(&format!(
            "Cannot register Tick Loop's Herdr popup: {reason}"
        ))?,
        Err(_) => return open_full_screen(ctx, instance).await,
    }
    Ok(TickActionOutcome::Done)
}

/// Attaches the hosted Tick full screen in this View's terminal with a one-time view grant.
async fn open_full_screen(
    ctx: &TickActionContext<'_>,
    instance: &tick_protocol::InstanceRef,
) -> Result<TickActionOutcome, String> {
    let grant = ctx
        .agent
        .attach_hosted_tick(ctx.chat, instance.clone())
        .await
        .map_err(|error| format!("Cannot open {instance}: {error}"))?;
    ctx.console.begin_full_screen_attach(ctx.chat, grant)?;
    Ok(TickActionOutcome::Done)
}

/// Opens the text snapshot detail of a hosted Tick.
struct SnapshotHosted {
    instance: tick_protocol::InstanceRef,
}
impl TickActionExecutor for SnapshotHosted {
    fn prepare(&self, ui: &mut TickActionUi<'_>) -> Result<PreparedTickAction, String> {
        let chat = ui.chat().ok_or("No selected Chat")?;
        let entry = ui
            .monitor_entry(&self.instance)
            .ok_or("This Tick is no longer in Monitor")?;
        let detail = super::super::hosted_detail::HostedDetail::new(chat, entry);
        let panel = detail.id();
        ui.open_detail(Box::new(detail));
        Ok(load_hosted_snapshot(self.instance.clone(), panel))
    }
}

struct LoadHostedSnapshot {
    instance: tick_protocol::InstanceRef,
    panel: Uuid,
}
#[async_trait]
impl TickActionBackend for LoadHostedSnapshot {
    async fn execute(&self, ctx: TickActionContext<'_>) -> Result<TickActionOutcome, String> {
        let snapshot = ctx
            .agent
            .hosted_tick_snapshot(ctx.chat, self.instance.clone())
            .await
            .map_err(|error| error.to_string());
        ctx.update_detail(
            self.panel,
            |detail: &mut super::super::hosted_detail::HostedDetail| {
                detail.show(snapshot);
                Some(())
            },
        )?;
        Ok(TickActionOutcome::Done)
    }
}
struct NoBackend;
#[async_trait]
impl TickActionBackend for NoBackend {
    async fn execute(&self, _: TickActionContext<'_>) -> Result<TickActionOutcome, String> {
        Ok(TickActionOutcome::Done)
    }
}
struct RemoveFromMonitor {
    instance: tick_protocol::InstanceRef,
}
impl TickActionExecutor for RemoveFromMonitor {
    fn prepare(&self, _ui: &mut TickActionUi<'_>) -> Result<PreparedTickAction, String> {
        Ok(PreparedTickAction::new(
            self.instance.to_string(),
            'x',
            Arc::new(RemoveFromMonitor {
                instance: self.instance.clone(),
            }),
        ))
    }
}
#[async_trait]
impl TickActionBackend for RemoveFromMonitor {
    async fn execute(&self, ctx: TickActionContext<'_>) -> Result<TickActionOutcome, String> {
        let membership = ctx
            .agent
            .remove_monitor_member(ctx.chat, self.instance.clone())
            .await
            .map_err(|error| format!("Cannot remove from Monitor: {error}"))?;
        ctx.console
            .update_monitor_membership(ctx.chat, membership)?;
        Ok(TickActionOutcome::Done)
    }
}
const CLOSE_HOSTED_PROMPT: &str =
    "Close this hosted Tick and end its process? Enter confirm · Esc cancel";
const REOPEN_HOSTED_PROMPT: &str = "Start a new instance of this lost Tick? Its subscriptions do not follow. Enter confirm · Esc cancel";

/// Ends the hosted Tick at the user's request once confirmed; it stays in the Monitor, shown
/// gone, until removed.
struct CloseHosted {
    instance: tick_protocol::InstanceRef,
    is_confirmed: bool,
}
impl TickActionExecutor for CloseHosted {
    fn prepare(&self, _ui: &mut TickActionUi<'_>) -> Result<PreparedTickAction, String> {
        Ok(PreparedTickAction::new(
            self.instance.to_string(),
            HOSTED_CLOSE_KEY,
            Arc::new(CloseHosted {
                instance: self.instance.clone(),
                is_confirmed: self.is_confirmed,
            }),
        ))
    }
}
#[async_trait]
impl TickActionBackend for CloseHosted {
    async fn execute(&self, ctx: TickActionContext<'_>) -> Result<TickActionOutcome, String> {
        if !self.is_confirmed {
            ctx.console.confirm_tick_action(
                ctx.chat,
                self.instance.to_string(),
                Arc::new(CloseHosted {
                    instance: self.instance.clone(),
                    is_confirmed: true,
                }),
                CLOSE_HOSTED_PROMPT,
            )?;
            return Ok(TickActionOutcome::Done);
        }
        ctx.agent
            .close_hosted_tick(ctx.chat, self.instance.clone())
            .await
            .map_err(|error| format!("Cannot close {}: {error}", self.instance))?;
        ctx.notify(&format!("Closed {}", self.instance))?;
        Ok(TickActionOutcome::Done)
    }
}

/// Starts a new instance in place of a hosted Tick lost with its host once confirmed; it joins
/// the Monitor beside the lost row, and nothing the lost one was subscribed to follows it.
struct ReopenHosted {
    instance: tick_protocol::InstanceRef,
    is_confirmed: bool,
}
impl TickActionExecutor for ReopenHosted {
    fn prepare(&self, _ui: &mut TickActionUi<'_>) -> Result<PreparedTickAction, String> {
        Ok(PreparedTickAction::new(
            self.instance.to_string(),
            HOSTED_REOPEN_KEY,
            Arc::new(ReopenHosted {
                instance: self.instance.clone(),
                is_confirmed: self.is_confirmed,
            }),
        ))
    }
}
#[async_trait]
impl TickActionBackend for ReopenHosted {
    async fn execute(&self, ctx: TickActionContext<'_>) -> Result<TickActionOutcome, String> {
        if !self.is_confirmed {
            ctx.console.confirm_tick_action(
                ctx.chat,
                self.instance.to_string(),
                Arc::new(ReopenHosted {
                    instance: self.instance.clone(),
                    is_confirmed: true,
                }),
                REOPEN_HOSTED_PROMPT,
            )?;
            return Ok(TickActionOutcome::Done);
        }
        let (reopened, membership) = ctx
            .agent
            .reopen_hosted_tick(ctx.chat, self.instance.clone())
            .await
            .map_err(|error| format!("Cannot reopen {}: {error}", self.instance))?;
        ctx.console
            .update_monitor_membership(ctx.chat, membership)?;
        ctx.notify(&format!("Reopened {} as {reopened}", self.instance))?;
        Ok(TickActionOutcome::Done)
    }
}
struct Refresh {
    actions: Vec<Arc<dyn TickActionExecutor>>,
}
struct RefreshBackend {
    actions: Vec<PreparedTickAction>,
}
impl TickActionExecutor for Refresh {
    fn prepare(&self, ui: &mut TickActionUi<'_>) -> Result<PreparedTickAction, String> {
        let actions = self
            .actions
            .iter()
            .map(|action| action.prepare(ui))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(PreparedTickAction::new(
            "",
            'r',
            Arc::new(RefreshBackend { actions }),
        ))
    }
}
#[async_trait]
impl TickActionBackend for RefreshBackend {
    async fn execute(&self, ctx: TickActionContext<'_>) -> Result<TickActionOutcome, String> {
        for action in &self.actions {
            action.backend.execute(ctx).await?;
        }
        Ok(TickActionOutcome::Done)
    }
}
struct ShellOpen {
    task: Uuid,
}
struct ShellRefresh {
    panel: Uuid,
    task: Uuid,
}
impl TickActionExecutor for ShellOpen {
    fn prepare(&self, ui: &mut TickActionUi<'_>) -> Result<PreparedTickAction, String> {
        let chat = ui.chat().ok_or("No selected Chat")?;
        let detail = TickShell::loading(chat, self.task);
        let panel = detail.id;
        ui.open_detail(Box::new(detail));
        Ok(PreparedTickAction::new(
            tick_protocol::InstanceRef::shell(self.task).to_string(),
            'o',
            Arc::new(ShellRefresh {
                panel,
                task: self.task,
            }),
        ))
    }
}
#[async_trait]
impl TickActionBackend for ShellRefresh {
    async fn execute(&self, ctx: TickActionContext<'_>) -> Result<TickActionOutcome, String> {
        let center = ShellTaskCenter::new(ctx.agent.clone());
        let snapshot = center
            .snapshot(ctx.chat)
            .await
            .map_err(|error| error.to_string())?;
        let started = center.started_at_ms(ctx.chat, self.task).await;
        let effect = ctx
            .update_detail::<TickShell, _>(self.panel, |detail| {
                if detail.is_loaded() {
                    detail.update(snapshot)
                } else {
                    detail.load(snapshot, started)
                }
            })?
            .unwrap_or(TaskSurfaceEffect::None);
        apply_shell_effect(
            &center,
            ctx.console,
            ctx.chat,
            ShellEffectTarget::TickPanel(self.panel),
            effect,
        )
        .await?;
        Ok(TickActionOutcome::Done)
    }
}
struct ShellEffect {
    panel: Uuid,
    effect: TaskSurfaceEffect,
}
#[async_trait]
impl TickActionBackend for ShellEffect {
    async fn execute(&self, ctx: TickActionContext<'_>) -> Result<TickActionOutcome, String> {
        let center = ShellTaskCenter::new(ctx.agent.clone());
        apply_shell_effect(
            &center,
            ctx.console,
            ctx.chat,
            ShellEffectTarget::TickPanel(self.panel),
            self.effect.clone(),
        )
        .await?;
        Ok(TickActionOutcome::Done)
    }
}
#[derive(Clone, Copy)]
enum ShellEffectTarget {
    TaskCenter,
    TickPanel(uuid::Uuid),
}

impl ShellEffectTarget {
    fn snapshot(
        self,
        console: &Console,
        chat: uuid::Uuid,
        snapshot: tickloop_agent_protocol::ShellTaskSnapshot,
    ) -> Result<TaskSurfaceEffect, String> {
        match self {
            Self::TaskCenter => console.update_tasks(snapshot),
            Self::TickPanel(panel) => console.update_tick_shell(chat, panel, snapshot),
        }
    }
    fn output(
        self,
        console: &Console,
        chat: uuid::Uuid,
        update: TaskOutputUpdate,
    ) -> Result<TaskSurfaceEffect, String> {
        match self {
            Self::TaskCenter => console.update_task_output(chat, update),
            Self::TickPanel(panel) => console.update_tick_shell_output(chat, panel, update),
        }
    }
    fn action_failed(
        self,
        console: &Console,
        chat: uuid::Uuid,
        task: uuid::Uuid,
        error: String,
    ) -> Result<(), String> {
        match self {
            Self::TaskCenter => console.task_action_failed(chat, task, error),
            Self::TickPanel(panel) => console.tick_shell_control_failed(chat, panel, error),
        }
    }
    fn clear_action_error(self, console: &Console, chat: uuid::Uuid) -> Result<(), String> {
        match self {
            Self::TaskCenter => console.clear_task_action_error(chat),
            Self::TickPanel(panel) => console.clear_tick_shell_control_error(chat, panel),
        }
    }
    fn updates_failed(
        self,
        console: &Console,
        chat: uuid::Uuid,
        error: String,
    ) -> Result<(), String> {
        match self {
            Self::TaskCenter => console.task_updates_failed(chat, error),
            Self::TickPanel(panel) => console.tick_shell_failed(chat, panel, error),
        }
    }
}

pub(in crate::terminal) async fn apply_task_surface_effect(
    task_center: &ShellTaskCenter,
    console: &Console,
    chat: uuid::Uuid,
    effect: TaskSurfaceEffect,
) -> Result<(), String> {
    apply_shell_effect(
        task_center,
        console,
        chat,
        ShellEffectTarget::TaskCenter,
        effect,
    )
    .await
}

async fn apply_shell_effect(
    task_center: &ShellTaskCenter,
    console: &Console,
    chat: uuid::Uuid,
    target: ShellEffectTarget,
    mut effect: TaskSurfaceEffect,
) -> Result<(), String> {
    loop {
        if matches!(target, ShellEffectTarget::TickPanel(_))
            && let TaskSurfaceEffect::Control { task_id, .. }
            | TaskSurfaceEffect::ReadOutput { task_id, .. } = &effect
        {
            match task_center.snapshot(chat).await {
                Ok(snapshot)
                    if !snapshot
                        .tasks
                        .iter()
                        .any(|task| task.id == *task_id && task.chat_id == chat) =>
                {
                    target.snapshot(console, chat, snapshot)?;
                    return Ok(());
                }
                Ok(_) => {}
                Err(error) => {
                    target.updates_failed(console, chat, safe_text(&error.to_string()))?;
                    return Ok(());
                }
            }
        }
        match effect {
            TaskSurfaceEffect::None | TaskSurfaceEffect::Close => return Ok(()),
            TaskSurfaceEffect::Control { task_id, action } => {
                match task_center.dispatch(chat, task_id, action).await {
                    Ok(_) => target.clear_action_error(console, chat)?,
                    Err(error) => target.action_failed(
                        console,
                        chat,
                        task_id,
                        format!(
                            "Cannot update Shell task: {}",
                            safe_text(&error.to_string())
                        ),
                    )?,
                }
                match task_center.snapshot(chat).await {
                    Ok(snapshot) => effect = target.snapshot(console, chat, snapshot)?,
                    Err(error) => {
                        target.updates_failed(console, chat, safe_text(&error.to_string()))?;
                        return Ok(());
                    }
                }
            }
            TaskSurfaceEffect::ReadOutput {
                task_id,
                cursor,
                limit,
                generation,
            } => match task_center.read_output(chat, task_id, cursor, limit).await {
                Ok(chunk) => {
                    effect = target.output(
                        console,
                        chat,
                        TaskOutputUpdate::Chunk { generation, chunk },
                    )?
                }
                Err(error) => {
                    let total_bytes = task_center
                        .snapshot(chat)
                        .await
                        .ok()
                        .and_then(|snapshot| {
                            snapshot.tasks.into_iter().find(|task| task.id == task_id)
                        })
                        .map_or(0, |task| task.output_bytes);
                    target.output(
                        console,
                        chat,
                        TaskOutputUpdate::Unavailable {
                            generation,
                            task_id,
                            total_bytes,
                            message: safe_text(&error.to_string()),
                        },
                    )?;
                    return Ok(());
                }
            },
        }
    }
}
impl TickDetailView for TickShell {
    fn confirmation(&self) -> Option<TickConfirmationView> {
        self.is_confirming_stop().then(|| TickConfirmationView {
            title: "Stop Shell".into(),
            message: "Stop this Shell?".into(),
            label: "Stop".into(),
        })
    }

    fn id(&self) -> Uuid {
        self.id
    }
    fn chat(&self) -> Uuid {
        self.chat
    }
    fn lines(&mut self, width: usize, height: usize, now: u64) -> Vec<TickDetailLine> {
        TickShell::lines(self, width, height, now)
            .into_iter()
            .map(|line| TickDetailLine { line })
            .collect()
    }
    fn hint(&self) -> String {
        TickShell::hint(self)
    }
    fn key(&mut self, key: KeyEvent, width: usize, height: usize) -> TickDetailKey {
        match TickShell::key(self, key, width, height) {
            TickShellKey::None => TickDetailKey::None,
            TickShellKey::Back => TickDetailKey::Back,
            TickShellKey::Effect(effect) => TickDetailKey::Action(PreparedTickAction::new(
                tick_protocol::InstanceRef::shell(self.task_id).to_string(),
                's',
                Arc::new(ShellEffect {
                    panel: self.id,
                    effect,
                }),
            )),
        }
    }
    fn poll(&mut self, now: u64) -> Option<PreparedTickAction> {
        if !self.should_refresh(now) {
            return None;
        }
        Some(PreparedTickAction::new(
            tick_protocol::InstanceRef::shell(self.task_id).to_string(),
            'r',
            Arc::new(ShellRefresh {
                panel: self.id,
                task: self.task_id,
            }),
        ))
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

impl Console {
    pub(in crate::terminal) fn confirm_tick_removal(
        &self,
        chat: Uuid,
        id: String,
    ) -> Result<(), String> {
        self.confirm_tick_action(
            chat,
            id.clone(),
            artifact(&id, 'X'),
            "Remove artifact and cancel its Tick subscriptions? Enter confirm · Esc cancel",
        )
    }
}
impl Console {
    pub(in crate::terminal) fn update_tick_shell(
        &self,
        chat: Uuid,
        panel: Uuid,
        snapshot: tickloop_agent_protocol::ShellTaskSnapshot,
    ) -> Result<TaskSurfaceEffect, String> {
        Ok(self
            .update_tick_detail::<TickShell, _>(chat, panel, |detail| detail.update(snapshot))?
            .unwrap_or(TaskSurfaceEffect::None))
    }
    pub(in crate::terminal) fn update_tick_shell_output(
        &self,
        chat: Uuid,
        panel: Uuid,
        update: TaskOutputUpdate,
    ) -> Result<TaskSurfaceEffect, String> {
        Ok(self
            .update_tick_detail::<TickShell, _>(chat, panel, |detail| Some(detail.output(update)))?
            .unwrap_or(TaskSurfaceEffect::None))
    }
    pub(in crate::terminal) fn tick_shell_failed(
        &self,
        chat: Uuid,
        panel: Uuid,
        error: String,
    ) -> Result<(), String> {
        self.update_tick_detail::<TickShell, _>(chat, panel, |detail| {
            detail.report(error);
            Some(())
        })?;
        Ok(())
    }
    pub(in crate::terminal) fn tick_shell_control_failed(
        &self,
        chat: Uuid,
        panel: Uuid,
        error: String,
    ) -> Result<(), String> {
        self.update_tick_detail::<TickShell, _>(chat, panel, |detail| {
            detail.report_control(error);
            Some(())
        })?;
        Ok(())
    }
    pub(in crate::terminal) fn clear_tick_shell_control_error(
        &self,
        chat: Uuid,
        panel: Uuid,
    ) -> Result<(), String> {
        self.update_tick_detail::<TickShell, _>(chat, panel, |detail| {
            detail.clear_control_error();
            Some(())
        })?;
        Ok(())
    }
}

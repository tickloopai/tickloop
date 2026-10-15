use super::super::{
    artifact_ui, safe_text,
    subscription_detail::{SubscriptionDetail, SubscriptionRequest},
    task_access::ShellTaskCenter,
};
use super::*;
use crate::monitor_rows::{HOSTED_CLOSE_KEY, HOSTED_REOPEN_KEY, SHELL_STOP_KEY, TickIntent};

/// The Console executor that carries out a Monitor row's intent. A notice has none: the Console
/// only shows it.
pub(in crate::terminal) fn executor(intent: &TickIntent) -> Option<Arc<dyn TickActionExecutor>> {
    let executor: Arc<dyn TickActionExecutor> = match intent.clone() {
        TickIntent::Notify(_) => return None,
        TickIntent::ShowDetails(instance) => Arc::new(DetailsOpen { instance }),
        TickIntent::RemoveFromMonitor(instance) => Arc::new(RemoveFromMonitor {
            instance,
            is_confirmed: false,
        }),
        TickIntent::OpenView(instance) => Arc::new(OpenView { instance }),
        TickIntent::CloseHosted(instance) => Arc::new(CloseHosted {
            instance,
            is_confirmed: false,
        }),
        TickIntent::ReopenHosted(instance) => Arc::new(ReopenHosted {
            instance,
            is_confirmed: false,
        }),
        TickIntent::StopShell { task } => Arc::new(StopShell {
            task,
            is_confirmed: false,
        }),
        TickIntent::Artifact { id, key } => artifact(&id, key),
        TickIntent::RefreshArtifacts => artifact("", 'r'),
        TickIntent::ManageSubscriptions {
            instance,
            operation,
        } => Arc::new(OpenSubscriptions {
            instance,
            operation,
        }),
    };
    Some(executor)
}
pub(in crate::terminal) fn artifact(id: &str, key: char) -> Arc<dyn TickActionExecutor> {
    Arc::new(ArtifactAction { id: id.into(), key })
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
    async fn execute(&self, ctx: TickActionContext<'_>) -> Result<(), String> {
        artifact_ui::tick_action(
            ctx.agent,
            ctx.console,
            ctx.root,
            ctx.chat,
            self.id.clone(),
            self.key,
        )
        .await?;
        Ok(())
    }
}
/// Opens the row's subscriptions inside Monitor, then reads them fresh from the Agent.
struct OpenSubscriptions {
    instance: tick_protocol::InstanceRef,
    operation: crate::monitor_rows::SubscriptionOperation,
}
impl TickActionExecutor for OpenSubscriptions {
    fn prepare(&self, ui: &mut TickActionUi<'_>) -> Result<PreparedTickAction, String> {
        let detail = ui.subscription_detail(&self.instance, self.operation)?;
        ui.open_detail(Box::new(detail));
        let key = match self.operation {
            crate::monitor_rows::SubscriptionOperation::View => {
                crate::monitor_rows::SUBSCRIPTIONS_KEY
            }
            crate::monitor_rows::SubscriptionOperation::EditFrequency => {
                crate::monitor_rows::SUBSCRIPTION_FREQUENCY_KEY
            }
            crate::monitor_rows::SubscriptionOperation::Cancel => {
                crate::monitor_rows::UNSUBSCRIBE_KEY
            }
        };
        Ok(PreparedTickAction::new(
            self.instance.to_string(),
            key,
            Arc::new(RefreshSubscriptions),
        ))
    }
}
/// Reads the Chat's subscriptions again so Monitor and an open subscription detail show them.
struct RefreshSubscriptions;
#[async_trait]
impl TickActionBackend for RefreshSubscriptions {
    async fn execute(&self, ctx: TickActionContext<'_>) -> Result<(), String> {
        refresh_subscriptions(&ctx).await
    }
}
async fn refresh_subscriptions(ctx: &TickActionContext<'_>) -> Result<(), String> {
    let rows = ctx
        .agent
        .tick_subscriptions(ctx.chat)
        .await
        .map_err(|error| format!("Cannot read Tick Subscriptions: {error}"))?;
    ctx.console.update_subscribed_ticks(ctx.chat, &rows)
}
/// Changes or cancels one subscription for the subscription detail `panel`, which reports a
/// refusal; either way Monitor then shows the subscriptions as the Agent holds them.
pub(in crate::terminal) fn change_subscription(
    panel: Uuid,
    request: SubscriptionRequest,
) -> Arc<dyn TickActionBackend> {
    Arc::new(ChangeSubscription { panel, request })
}
struct ChangeSubscription {
    panel: Uuid,
    request: SubscriptionRequest,
}
#[async_trait]
impl TickActionBackend for ChangeSubscription {
    async fn execute(&self, ctx: TickActionContext<'_>) -> Result<(), String> {
        if let Err(error) = ctx
            .agent
            .update_tick_subscription(ctx.chat, self.request.subscription, self.request.change)
            .await
        {
            ctx.update_detail(self.panel, |detail: &mut SubscriptionDetail| {
                detail.report_error(format!(
                    "Cannot update the Tick Subscription: {}",
                    safe_text(&error.to_string())
                ));
                Some(())
            })?;
        }
        refresh_subscriptions(&ctx).await
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
/// Enter on a local Tick, a hosted Tick or a Shell, opens its view at once: inside Herdr in
/// Tickloop's Herdr popup, otherwise full screen in this View's terminal. Either view shows the
/// same Tick and Esc closes it.
struct OpenView {
    instance: tick_protocol::InstanceRef,
}
impl TickActionExecutor for OpenView {
    fn prepare(&self, ui: &mut TickActionUi<'_>) -> Result<PreparedTickAction, String> {
        ui.monitor_entry(&self.instance)
            .ok_or("This Tick is no longer in Monitor")?;
        Ok(PreparedTickAction::new(
            self.instance.to_string(),
            crate::monitor_rows::MONITOR_ENTER_KEY,
            Arc::new(OpenView {
                instance: self.instance.clone(),
            }),
        ))
    }
}
#[async_trait]
impl TickActionBackend for OpenView {
    async fn execute(&self, ctx: TickActionContext<'_>) -> Result<(), String> {
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
                    return Ok(());
                }
                Err(reason) => Err(reason),
            },
            Err(reason) => Err(reason),
        };
        settle_float(&ctx, &self.instance, floated).await
    }
}

const REGISTER_POPUP_PROMPT: &str =
    "Register Tickloop's Herdr popup plugin? No other plugin is touched.";
const TURN_ON_POPUP_PROMPT: &str = "Turn Tickloop's Herdr popup on? No other plugin is touched.";
const REGISTER_THIS_TICKLOOP_PROMPT: &str =
    "Tickloop's Herdr popup runs another tickloop. Register this one instead?";
const HERDR_BUSY: &str = "Herdr is showing another view; close it and press enter again";

/// Why a local Tick did not float in the Herdr popup.
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

/// This View's Herdr session together with Tickloop's popup plugin for it.
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

/// Registers, or turns on, Tickloop's popup plugin in this View's Herdr session once the user
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
    async fn execute(&self, ctx: TickActionContext<'_>) -> Result<(), String> {
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
) -> Result<(), String> {
    match floated {
        Ok(()) => {}
        Err(NotFloated::Busy) => ctx.console.hold_status(HERDR_BUSY)?,
        Err(NotFloated::Unknown(reason)) => ctx.console.hold_status(&format!(
            "The Herdr popup may have opened ({reason}); look in Herdr before opening again"
        ))?,
        Err(NotFloated::RegistrationFailed(reason)) => ctx
            .console
            .hold_status(&format!("Cannot register Tickloop's Herdr popup: {reason}"))?,
        Err(_) => return open_full_screen(ctx, instance).await,
    }
    Ok(())
}

/// Attaches the local Tick full screen in this View's terminal with a one-time view grant.
async fn open_full_screen(
    ctx: &TickActionContext<'_>,
    instance: &tick_protocol::InstanceRef,
) -> Result<(), String> {
    let grant = ctx
        .agent
        .attach_hosted_tick(ctx.chat, instance.clone())
        .await
        .map_err(|error| format!("Cannot open {instance}: {error}"))?;
    ctx.console.begin_full_screen_attach(ctx.chat, grant)?;
    Ok(())
}

struct NoBackend;
#[async_trait]
impl TickActionBackend for NoBackend {
    async fn execute(&self, _: TickActionContext<'_>) -> Result<(), String> {
        Ok(())
    }
}
/// The user has seen `row` up to now, such as by closing its open detail.
pub(in crate::terminal) fn seen(row: tick_protocol::InstanceRef) -> PreparedTickAction {
    PreparedTickAction::new(
        row.to_string(),
        crate::monitor_rows::MONITOR_ENTER_KEY,
        Arc::new(NoBackend),
    )
    .opening(row)
}
/// Hides a row in this Chat. A row its Loops subscribe to asks first, then cancels those
/// subscriptions, so nothing keeps delivering from a Tick the user no longer sees.
struct RemoveFromMonitor {
    instance: tick_protocol::InstanceRef,
    is_confirmed: bool,
}
impl TickActionExecutor for RemoveFromMonitor {
    fn prepare(&self, _ui: &mut TickActionUi<'_>) -> Result<PreparedTickAction, String> {
        Ok(PreparedTickAction::new(
            self.instance.to_string(),
            'x',
            Arc::new(RemoveFromMonitor {
                instance: self.instance.clone(),
                is_confirmed: self.is_confirmed,
            }),
        ))
    }
}
#[async_trait]
impl TickActionBackend for RemoveFromMonitor {
    async fn execute(&self, ctx: TickActionContext<'_>) -> Result<(), String> {
        let subscriptions = crate::monitor_rows::subscriptions_on(
            &ctx.agent
                .tick_subscriptions(ctx.chat)
                .await
                .map_err(|error| format!("Cannot read Tick Subscriptions: {error}"))?,
            &self.instance,
        );
        if !subscriptions.is_empty() && !self.is_confirmed {
            ctx.console.confirm_tick_action(
                ctx.chat,
                self.instance.to_string(),
                Arc::new(RemoveFromMonitor {
                    instance: self.instance.clone(),
                    is_confirmed: true,
                }),
                &remove_subscribed_prompt(subscriptions.len()),
            )?;
            return Ok(());
        }
        for subscription in &subscriptions {
            ctx.agent
                .update_tick_subscription(
                    ctx.chat,
                    subscription.id,
                    tickloop_agent_protocol::TickSubscriptionChange::Cancel,
                )
                .await
                .map_err(|error| format!("Cannot cancel the Tick Subscription: {error}"))?;
        }
        if !subscriptions.is_empty() {
            refresh_subscriptions(&ctx).await?;
        }
        let membership = ctx
            .agent
            .remove_monitor_member(ctx.chat, self.instance.clone())
            .await
            .map_err(|error| format!("Cannot remove from Monitor: {error}"))?;
        ctx.console
            .update_monitor_membership(ctx.chat, membership)?;
        Ok(())
    }
}
fn remove_subscribed_prompt(subscriptions: usize) -> String {
    format!(
        "Remove from Monitor and cancel its {}? enter confirm · esc cancel",
        tick_subscriptions(subscriptions)
    )
}
const ARTIFACT_UNLINK_PROMPT: &str =
    "Unlink this artifact and stop its @tickloop requests here? enter confirm · esc cancel";
fn tick_subscriptions(count: usize) -> String {
    format!(
        "{count} Tick {}",
        if count == 1 {
            "Subscription"
        } else {
            "Subscriptions"
        }
    )
}
const CLOSE_HOSTED_PROMPT: &str =
    "Close this hosted Tick and end its process? enter confirm · esc cancel";
const STOP_SHELL_PROMPT: &str = "Stop this Shell? enter confirm · esc cancel";
const REOPEN_HOSTED_PROMPT: &str = "Start a new instance of this lost Tick? Its subscriptions do not follow. enter confirm · esc cancel";

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
    async fn execute(&self, ctx: TickActionContext<'_>) -> Result<(), String> {
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
            return Ok(());
        }
        ctx.agent
            .close_hosted_tick(ctx.chat, self.instance.clone())
            .await
            .map_err(|error| format!("Cannot close {}: {error}", self.instance))?;
        ctx.notify(&format!("Closed {}", self.instance))?;
        Ok(())
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
    async fn execute(&self, ctx: TickActionContext<'_>) -> Result<(), String> {
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
            return Ok(());
        }
        let (reopened, membership) = ctx
            .agent
            .reopen_hosted_tick(ctx.chat, self.instance.clone())
            .await
            .map_err(|error| format!("Cannot reopen {}: {error}", self.instance))?;
        ctx.console
            .update_monitor_membership(ctx.chat, membership)?;
        ctx.notify(&format!("Reopened {} as {reopened}", self.instance))?;
        Ok(())
    }
}
/// Stops a running Shell from its Monitor row once confirmed, under the Shell's own controls:
/// only a Shell its owner lets this View stop. Its row shows how it ended.
struct StopShell {
    task: Uuid,
    is_confirmed: bool,
}
impl TickActionExecutor for StopShell {
    fn prepare(&self, _ui: &mut TickActionUi<'_>) -> Result<PreparedTickAction, String> {
        Ok(PreparedTickAction::new(
            tick_protocol::InstanceRef::shell(self.task).to_string(),
            SHELL_STOP_KEY,
            Arc::new(StopShell {
                task: self.task,
                is_confirmed: self.is_confirmed,
            }),
        ))
    }
}
#[async_trait]
impl TickActionBackend for StopShell {
    async fn execute(&self, ctx: TickActionContext<'_>) -> Result<(), String> {
        if !self.is_confirmed {
            ctx.console.confirm_tick_action(
                ctx.chat,
                tick_protocol::InstanceRef::shell(self.task).to_string(),
                Arc::new(StopShell {
                    task: self.task,
                    is_confirmed: true,
                }),
                STOP_SHELL_PROMPT,
            )?;
            return Ok(());
        }
        let center = ShellTaskCenter::new(ctx.agent.clone());
        center
            .dispatch(
                ctx.chat,
                self.task,
                tickloop_agent_protocol::ShellTaskAction::Stop,
            )
            .await
            .map_err(|error| format!("Cannot stop the Shell: {}", safe_text(&error.to_string())))?;
        let snapshot = center
            .snapshot(ctx.chat)
            .await
            .map_err(|error| error.to_string())?;
        ctx.console.update_shell_ticks(&snapshot)
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
    async fn execute(&self, ctx: TickActionContext<'_>) -> Result<(), String> {
        for action in &self.actions {
            action.backend.execute(ctx).await?;
        }
        Ok(())
    }
}
impl Console {
    /// Asks before `x` takes out an artifact whose association still delivers here; confirming
    /// unlinks it.
    pub(in crate::terminal) fn confirm_artifact_unlink(
        &self,
        chat: Uuid,
        id: String,
    ) -> Result<(), String> {
        self.confirm_tick_action(chat, id.clone(), artifact(&id, 'X'), ARTIFACT_UNLINK_PROMPT)
    }
}

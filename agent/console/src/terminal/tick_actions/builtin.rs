use super::super::{
    artifact_ui, safe_text,
    subscription_detail::{SubscriptionDetail, SubscriptionRequest},
    task_access::ShellTaskCenter,
};
use super::*;
use crate::monitor_rows::{REMOVE_KEY, SHELL_STOP_KEY, SUBSCRIBE_KEY, TickIntent};

/// The Console executor that carries out a Monitor row's intent. A notice has none: the Console
/// only shows it.
pub(in crate::terminal) fn executor(intent: &TickIntent) -> Option<Arc<dyn TickActionExecutor>> {
    let executor: Arc<dyn TickActionExecutor> = match intent.clone() {
        TickIntent::Notify(_) => return None,
        TickIntent::ShowDetails(instance) => Arc::new(DetailsOpen { instance }),
        TickIntent::RemoveFromMonitor(instance) => Arc::new(RemoveFromMonitor { instance }),
        TickIntent::OpenView(instance) => Arc::new(OpenView { instance }),
        TickIntent::CloseHosted {
            instance,
            ends_process,
        } => Arc::new(CloseHosted {
            instance,
            ends_process,
        }),
        TickIntent::ReopenHosted(instance) => Arc::new(ReopenHosted { instance }),
        TickIntent::Subscribe(instance) => Arc::new(Subscribe { instance }),
        TickIntent::SendUpdate(instance) => Arc::new(SendUpdate { instance }),
        TickIntent::StopShell { task } => Arc::new(StopShell { task }),
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
    ctx.console.update_monitor_chat(
        ctx.agent
            .current_status(ctx.chat)
            .await
            .map_err(|error| error.to_string())?,
    )?;
    ctx.console.update_subscribed_ticks(ctx.chat, &rows)
}
/// Changes or cancels one subscription. A refusal shows in the subscription detail `panel` while
/// it is open, else on the status line, and Monitor stops showing a frequency the Agent refused;
/// either way Monitor then shows the subscriptions as the Agent holds them.
pub(in crate::terminal) fn change_subscription(
    panel: Option<Uuid>,
    request: SubscriptionRequest,
) -> Arc<dyn TickActionBackend> {
    Arc::new(ChangeSubscription { panel, request })
}
struct ChangeSubscription {
    panel: Option<Uuid>,
    request: SubscriptionRequest,
}
#[async_trait]
impl TickActionBackend for ChangeSubscription {
    async fn execute(&self, ctx: TickActionContext<'_>) -> Result<(), String> {
        let rows = ctx
            .agent
            .tick_subscriptions(ctx.chat)
            .await
            .map_err(|error| error.to_string())?;
        let targets = rows
            .iter()
            .find(|row| row.id == self.request.subscription)
            .filter(|_| self.panel.is_none())
            .map(|selected| {
                rows.iter()
                    .filter(|row| {
                        row.instance == selected.instance && row.frequency.period().is_some()
                    })
                    .map(|row| row.id)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(|| vec![self.request.subscription]);
        let mut failure = None;
        for id in &targets {
            if let Err(error) = ctx
                .agent
                .update_tick_subscription(ctx.chat, *id, self.request.change)
                .await
            {
                failure = Some(error);
                break;
            }
        }
        if let Some(error) = failure {
            for id in targets {
                ctx.console.forget_requested_subscription(ctx.chat, id)?;
            }
            let message = format!(
                "Cannot update the Tick Subscription: {}",
                safe_text(&error.to_string())
            );
            let reported = match self.panel {
                Some(panel) => ctx.update_detail(panel, |detail: &mut SubscriptionDetail| {
                    detail.report(message.clone());
                    Some(())
                })?,
                None => None,
            };
            if reported.is_none() {
                ctx.notify(&message)?;
            }
        }
        refresh_subscriptions(&ctx).await
    }
}
/// Moves one periodic subscription on to its next frequency, off after low, at once: Monitor
/// shows the new frequency before the Agent confirms it, without asking. A refusal shows in the
/// subscription detail `panel` while it is open.
pub(in crate::terminal) struct CycleSubscription {
    pub(in crate::terminal) instance: tick_protocol::InstanceRef,
    pub(in crate::terminal) subscription: Uuid,
    pub(in crate::terminal) panel: Option<Uuid>,
}
impl TickActionExecutor for CycleSubscription {
    fn prepare(&self, ui: &mut TickActionUi<'_>) -> Result<PreparedTickAction, String> {
        let change = ui
            .cycle_subscription(self.subscription, self.panel.is_none())
            .ok_or("This Tick Subscription has no frequency to change")?;
        Ok(PreparedTickAction::new(
            self.instance.to_string(),
            SUBSCRIBE_KEY,
            change_subscription(
                self.panel,
                SubscriptionRequest {
                    subscription: self.subscription,
                    change,
                },
            ),
        ))
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
                Err(reason) if reason.needs_registration() => {
                    return RegisterPopup {
                        instance: self.instance.clone(),
                    }
                    .execute(ctx)
                    .await;
                }
                Err(reason) => Err(reason),
            },
            Err(reason) => Err(reason),
        };
        settle_float(&ctx, &self.instance, floated).await
    }
}

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
    /// Opening the Tick needed our plugin, but registration failed.
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
    fn needs_registration(&self) -> bool {
        matches!(self, Self::NotRegistered | Self::Outdated)
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
        if ctx
            .console
            .monitor_popup()
            .map_err(|_| NotFloated::Unavailable)?
            == tickloop_agent_config::MonitorPopup::Disabled
        {
            return Err(NotFloated::TurnedOff);
        }

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
        Ok(())
    }

    /// Links our own popup plugin and enables the popup requested by opening the Tick.
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

/// Registers, or turns on, Tickloop's popup plugin in this View's Herdr session when the user opens a Tick, then floats it.
struct RegisterPopup {
    instance: tick_protocol::InstanceRef,
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
/// Hides a row in this Chat and cancels its Tick Subscriptions.
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
    async fn execute(&self, ctx: TickActionContext<'_>) -> Result<(), String> {
        let subscriptions = crate::monitor_rows::subscriptions_on(
            &ctx.agent
                .tick_subscriptions(ctx.chat)
                .await
                .map_err(|error| format!("Cannot read Tick Subscriptions: {error}"))?,
            &self.instance,
        );
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
        ctx.notify(&removed_message(subscriptions.len()))
    }
}
fn removed_message(subscriptions: usize) -> String {
    if subscriptions == 0 {
        "Removed from Monitor".into()
    } else {
        format!(
            "Removed from Monitor · {} cancelled",
            tick_subscriptions(subscriptions)
        )
    }
}
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
/// Ends a hosted Tick this Chat opened and removes its row and Tick Subscriptions.
struct CloseHosted {
    instance: tick_protocol::InstanceRef,
    ends_process: bool,
}
impl TickActionExecutor for CloseHosted {
    fn prepare(&self, _ui: &mut TickActionUi<'_>) -> Result<PreparedTickAction, String> {
        Ok(PreparedTickAction::new(
            self.instance.to_string(),
            REMOVE_KEY,
            Arc::new(CloseHosted {
                instance: self.instance.clone(),
                ends_process: self.ends_process,
            }),
        ))
    }
}
#[async_trait]
impl TickActionBackend for CloseHosted {
    async fn execute(&self, ctx: TickActionContext<'_>) -> Result<(), String> {
        let subscriptions = crate::monitor_rows::subscriptions_on(
            &ctx.agent
                .tick_subscriptions(ctx.chat)
                .await
                .map_err(|error| format!("Cannot read Tick Subscriptions: {error}"))?,
            &self.instance,
        );
        ctx.agent
            .close_hosted_tick(ctx.chat, self.instance.clone())
            .await
            .map_err(|error| format!("Cannot close {}: {error}", self.instance))?;
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
        ctx.notify(&if self.ends_process {
            format!("Tick closed · {}", removed_message(subscriptions.len()))
        } else {
            removed_message(subscriptions.len())
        })
    }
}

/// Starts a new instance with the recorded command and restores its Tick Subscriptions.
struct ReopenHosted {
    instance: tick_protocol::InstanceRef,
}
impl TickActionExecutor for ReopenHosted {
    fn prepare(&self, _ui: &mut TickActionUi<'_>) -> Result<PreparedTickAction, String> {
        Ok(PreparedTickAction::new(
            self.instance.to_string(),
            crate::monitor_rows::MONITOR_ENTER_KEY,
            Arc::new(ReopenHosted {
                instance: self.instance.clone(),
            }),
        ))
    }
}
#[async_trait]
impl TickActionBackend for ReopenHosted {
    async fn execute(&self, ctx: TickActionContext<'_>) -> Result<(), String> {
        let (reopened, membership, restore_error) = ctx
            .agent
            .reopen_hosted_tick(ctx.chat, self.instance.clone(), true)
            .await
            .map_err(|error| format!("Cannot reopen {}: {error}", self.instance))?;
        ctx.console
            .update_monitor_membership(ctx.chat, membership)?;
        refresh_subscriptions(&ctx).await?;
        ctx.notify(&match restore_error {
            Some(error) => format!(
                "Reopened {} as {reopened}; {}",
                self.instance,
                safe_text(&error)
            ),
            None => format!("Reopened {} as {reopened}", self.instance),
        })?;
        Ok(())
    }
}

struct SendUpdate {
    instance: tick_protocol::InstanceRef,
}
impl TickActionExecutor for SendUpdate {
    fn prepare(&self, _ui: &mut TickActionUi<'_>) -> Result<PreparedTickAction, String> {
        Ok(PreparedTickAction::new(
            self.instance.to_string(),
            'u',
            Arc::new(Self {
                instance: self.instance.clone(),
            }),
        ))
    }
}
#[async_trait]
impl TickActionBackend for SendUpdate {
    async fn execute(&self, ctx: TickActionContext<'_>) -> Result<(), String> {
        ctx.agent
            .tick_send_update(ctx.chat, self.instance.clone())
            .await
            .map_err(|error| format!("Cannot send update: {}", safe_text(&error.to_string())))?;
        refresh_subscriptions(&ctx).await
    }
}

/// The row's `s` on an extension or terminal Tick: subscribes the Chat at the default frequency
/// on its user's word, or, once a Loop of the Chat subscribes to it, cycles that subscription's
/// frequency.
struct Subscribe {
    instance: tick_protocol::InstanceRef,
}
impl TickActionExecutor for Subscribe {
    fn prepare(&self, ui: &mut TickActionUi<'_>) -> Result<PreparedTickAction, String> {
        if let Some(subscription) = ui.subscription_target(&self.instance) {
            ui.cycle_subscription(subscription, true);
        }
        // Keep queued row presses relative, including those prepared before the first subscribe
        // completes. Mixing these with absolute frequency requests would lose a rapid press.
        Ok(PreparedTickAction::new(
            self.instance.to_string(),
            SUBSCRIBE_KEY,
            Arc::new(Subscribe {
                instance: self.instance.clone(),
            }),
        ))
    }
}
#[async_trait]
impl TickActionBackend for Subscribe {
    async fn execute(&self, ctx: TickActionContext<'_>) -> Result<(), String> {
        let subscriptions = ctx
            .agent
            .tick_subscriptions(ctx.chat)
            .await
            .map_err(|error| format!("Cannot read Tick Subscriptions: {error}"))?;
        if let Some(row) = crate::monitor_rows::subscriptions_on(&subscriptions, &self.instance)
            .into_iter()
            .filter(|row| row.frequency.period().is_some())
            .min_by_key(|row| row.frequency.period())
        {
            let change = match row.frequency {
                tickloop_agent_protocol::TickSubscriptionFrequency::Low => {
                    tickloop_agent_protocol::TickSubscriptionChange::Cancel
                }
                frequency => tickloop_agent_protocol::TickSubscriptionChange::Frequency(
                    frequency.next_periodic().expect("periodic subscription"),
                ),
            };
            return ChangeSubscription {
                panel: None,
                request: SubscriptionRequest {
                    subscription: row.id,
                    change,
                },
            }
            .execute(ctx)
            .await;
        }
        ctx.agent
            .tick_subscribe(ctx.chat, self.instance.clone())
            .await
            .map_err(|error| format!("Cannot subscribe: {}", safe_text(&error.to_string())))?;
        refresh_subscriptions(&ctx).await?;
        Ok(())
    }
}
/// Stops a running Shell from its Monitor row under the Shell's own controls:
/// only a Shell its owner lets this View stop. Its row shows how it ended.
struct StopShell {
    task: Uuid,
}
impl TickActionExecutor for StopShell {
    fn prepare(&self, _ui: &mut TickActionUi<'_>) -> Result<PreparedTickAction, String> {
        Ok(PreparedTickAction::new(
            tick_protocol::InstanceRef::shell(self.task).to_string(),
            SHELL_STOP_KEY,
            Arc::new(StopShell { task: self.task }),
        ))
    }
}
#[async_trait]
impl TickActionBackend for StopShell {
    async fn execute(&self, ctx: TickActionContext<'_>) -> Result<(), String> {
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
        ctx.console.refresh_monitor_ticks(snapshot, None)?;
        ctx.notify("Shell stopped")
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

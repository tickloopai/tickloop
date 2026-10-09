use super::super::{
    artifact_ui, safe_text,
    task_access::ShellTaskCenter,
    task_surface::{self, TaskOutputUpdate, TaskSurfaceEffect},
    tick_shell::{TickShell, TickShellKey},
};
use super::*;

pub(crate) fn artifact(id: &str, key: char) -> Arc<dyn TickActionExecutor> {
    Arc::new(ArtifactAction { id: id.into(), key })
}
pub(crate) fn subscription(id: &str) -> Arc<dyn TickActionExecutor> {
    Arc::new(SubscriptionAction { id: id.into() })
}
pub(crate) fn details(instance: crate::tick_stream::InstanceRef) -> Arc<dyn TickActionExecutor> {
    Arc::new(DetailsOpen { instance })
}
pub(crate) fn remove(instance: crate::tick_stream::InstanceRef) -> Arc<dyn TickActionExecutor> {
    Arc::new(RemoveFromMonitor { instance })
}
pub(crate) fn shell(task: Uuid) -> Arc<dyn TickActionExecutor> {
    Arc::new(ShellOpen { task })
}
pub(crate) fn refresh(actions: Vec<Arc<dyn TickActionExecutor>>) -> Arc<dyn TickActionExecutor> {
    Arc::new(Refresh { actions })
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
    id: String,
}
impl TickActionExecutor for SubscriptionAction {
    fn prepare(&self, _ui: &mut TickActionUi<'_>) -> Result<PreparedTickAction, String> {
        Ok(PreparedTickAction::new(
            &self.id,
            'o',
            Arc::new(SubscriptionAction {
                id: self.id.clone(),
            }),
        ))
    }
}
#[async_trait]
impl TickActionBackend for SubscriptionAction {
    async fn execute(&self, ctx: TickActionContext<'_>) -> Result<TickActionOutcome, String> {
        let id = Uuid::parse_str(&self.id).map_err(|error| error.to_string())?;
        let rows = ctx
            .agent
            .tick_subscriptions(ctx.chat)
            .await
            .map_err(|error| error.to_string())?;
        ctx.console
            .show_subscriptions_at(ctx.chat, rows, id, false)?;
        Ok(TickActionOutcome::Done)
    }
}
struct DetailsOpen {
    instance: crate::tick_stream::InstanceRef,
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
struct NoBackend;
#[async_trait]
impl TickActionBackend for NoBackend {
    async fn execute(&self, _: TickActionContext<'_>) -> Result<TickActionOutcome, String> {
        Ok(TickActionOutcome::Done)
    }
}
struct RemoveFromMonitor {
    instance: crate::tick_stream::InstanceRef,
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
            crate::tick_stream::InstanceRef::shell(self.task).to_string(),
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
        snapshot: task_surface::TaskSurfaceSnapshot,
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
                crate::tick_stream::InstanceRef::shell(self.task_id).to_string(),
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
            crate::tick_stream::InstanceRef::shell(self.task_id).to_string(),
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
        snapshot: super::super::task_surface::TaskSurfaceSnapshot,
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

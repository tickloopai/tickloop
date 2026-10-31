//! Adapter-owned list actions and details, with a shared terminal presentation boundary.
pub(crate) mod builtin;

use super::{
    console::{Console, State},
    execution::ConsoleExecution,
    style::{Role, RoleLine},
};
use async_trait::async_trait;
use crossterm::event::KeyEvent;
use std::{
    any::Any,
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::Arc,
};
use uuid::Uuid;

pub struct TickDetailLine {
    pub(super) line: RoleLine,
}
impl TickDetailLine {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            line: RoleLine::role(text.into(), Role::Text),
        }
    }
}

pub enum TickDetailKey {
    None,
    Back,
    Action(PreparedTickAction),
    /// An action the Console prepares with its own state, as it prepares a row key's.
    Prepare(Arc<dyn TickActionExecutor>),
}
pub trait TickDetailView: Send + Any {
    fn id(&self) -> Uuid;
    fn chat(&self) -> Uuid;
    /// The Monitor row this view shows; that row stays listed while the view is open.
    fn instance(&self) -> Option<tick_protocol::InstanceRef> {
        None
    }
    fn lines(&mut self, width: usize, height: usize, now: u64) -> Vec<TickDetailLine>;
    fn hint(&self) -> String;
    fn key(&mut self, key: KeyEvent, width: usize, height: usize) -> TickDetailKey;
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

pub struct TickActionUi<'a> {
    pub(super) state: &'a mut State,
}
impl TickActionUi<'_> {
    pub fn chat(&self) -> Option<Uuid> {
        self.state.chat
    }
    pub fn open_detail(&mut self, detail: Box<dyn TickDetailView>) {
        self.state.monitor.detail = Some(detail);
        self.state.rendered_region = None;
    }
    pub fn notify(&mut self, message: &str) {
        self.state.monitor.action_error = Some(super::safe_line(message));
    }
    /// The subscriptions on `instance` as a Monitor detail starting with `operation`.
    pub(super) fn subscription_detail(
        &self,
        instance: &tick_protocol::InstanceRef,
        operation: crate::monitor_rows::SubscriptionOperation,
    ) -> Result<super::subscription_detail::SubscriptionDetail, String> {
        let chat = self.chat().ok_or("No selected Chat")?;
        let monitor = &self.state.monitor;
        let rows = crate::monitor_rows::subscriptions_on(&monitor.shown_subscriptions(), instance);
        if rows.is_empty() {
            return Err("The Loop does not subscribe to this Tick".into());
        }
        Ok(super::subscription_detail::SubscriptionDetail::open(
            chat,
            instance.clone(),
            monitor.title_of(instance),
            rows,
            operation,
            self.state.display,
        ))
    }
    /// The fastest periodic subscription on `instance`, matching the row's displayed frequency.
    pub(super) fn subscription_target(
        &self,
        instance: &tick_protocol::InstanceRef,
    ) -> Option<Uuid> {
        self.state.monitor.subscription_target(instance)
    }
    /// Moves `subscription` on to its next periodic frequency in Monitor at once, so the next
    /// press cycles on from it before the Agent reports the change, and says which it is now.
    pub(super) fn cycle_subscription(
        &mut self,
        subscription: Uuid,
        all_on_instance: bool,
    ) -> Option<tickloop_harness::protocol::TickSubscriptionChange> {
        self.state
            .monitor
            .request_next_subscription(subscription, all_on_instance)
    }

    /// Shows the row's periodic subscriptions as cancelled at once; returns one of them.
    pub(super) fn unsubscribe(&mut self, instance: &tick_protocol::InstanceRef) -> Option<Uuid> {
        self.state.monitor.request_unsubscribe(instance)
    }

    pub(super) fn monitor_entry(
        &self,
        instance: &tick_protocol::InstanceRef,
    ) -> Option<super::monitor::MonitorEntry> {
        self.state
            .monitor
            .entries()
            .iter()
            .find(|entry| &entry.instance == instance)
            .cloned()
    }
}

pub trait TickActionExecutor: Send + Sync {
    fn prepare(&self, ui: &mut TickActionUi<'_>) -> Result<PreparedTickAction, String>;
}
#[async_trait]
pub trait TickActionBackend: Send + Sync {
    async fn execute(&self, context: TickActionContext<'_>) -> Result<(), String>;
}
pub struct PreparedTickAction {
    pub instance: String,
    pub key: char,
    pub(super) backend: Arc<dyn TickActionBackend>,
}
impl std::fmt::Debug for PreparedTickAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedTickAction")
            .field("instance", &self.instance)
            .field("key", &self.key)
            .finish()
    }
}
impl PreparedTickAction {
    pub fn new(
        instance: impl Into<String>,
        key: char,
        backend: Arc<dyn TickActionBackend>,
    ) -> Self {
        Self {
            instance: instance.into(),
            key,
            backend,
        }
    }
}
#[derive(Clone, Copy)]
pub struct TickActionContext<'a> {
    pub(super) agent: &'a ConsoleExecution,
    pub(super) console: &'a Console,
    pub(super) root: Option<&'a Path>,
    pub(super) chat: Uuid,
}
impl TickActionContext<'_> {
    pub fn chat(&self) -> Uuid {
        self.chat
    }
    pub fn notify(&self, message: &str) -> Result<(), String> {
        self.console.report_monitor_error(self.chat, message)
    }
    pub fn update_detail<T: TickDetailView, R>(
        &self,
        panel: Uuid,
        update: impl FnOnce(&mut T) -> Option<R>,
    ) -> Result<Option<R>, String> {
        self.console.update_tick_detail(self.chat, panel, update)
    }
}

/// Runs the action. Opening a row never handles its updates: read state belongs to the Agent.
pub(super) async fn execute(
    action: PreparedTickAction,
    context: TickActionContext<'_>,
) -> Result<(), String> {
    action.backend.execute(context).await
}

/// Monitor actions waiting on their hosts, which may never answer. They run beside the Console's
/// loop so input and Agent events keep flowing, one at a time in the order the user gave them,
/// since each acts on the state the one before it left. A Chat whose action waits behind another
/// says so on its Monitor status line.
pub(super) struct MonitorActionQueue {
    agent: ConsoleExecution,
    console: Console,
    root: Option<PathBuf>,
    running: tokio::task::JoinSet<Result<(), String>>,
    running_chat: Option<Uuid>,
    waiting: VecDeque<(Uuid, PreparedTickAction)>,
}

impl MonitorActionQueue {
    pub(super) fn new(agent: ConsoleExecution, console: Console, root: Option<PathBuf>) -> Self {
        Self {
            agent,
            console,
            root,
            running: tokio::task::JoinSet::new(),
            running_chat: None,
            waiting: VecDeque::new(),
        }
    }

    pub(super) fn is_running(&self) -> bool {
        !self.running.is_empty()
    }

    pub(super) fn submit(&mut self, chat: Uuid, action: PreparedTickAction) -> Result<(), String> {
        self.waiting.push_back((chat, action));
        self.start_next();
        self.show_waiting(chat)
    }

    /// Waits for the running action and starts the next; an error, a panic included, comes with
    /// its Chat.
    pub(super) async fn finished(&mut self) -> Result<(), (Uuid, String)> {
        let result = self.running.join_next().await;
        let chat = self.running_chat.take();
        if let Some(started) = self.start_next() {
            self.show_waiting(started)
                .map_err(|error| (started, error))?;
        }
        let outcome = match result {
            Some(Ok(outcome)) => outcome,
            Some(Err(error)) if error.is_panic() => Err(format!("Monitor action failed: {error}")),
            _ => Ok(()),
        };
        match (chat, outcome) {
            (Some(chat), Err(error)) => Err((chat, error)),
            _ => Ok(()),
        }
    }

    /// Starts the next waiting action unless one runs, and returns its Chat.
    fn start_next(&mut self) -> Option<Uuid> {
        if self.is_running() {
            return None;
        }
        let (chat, action) = self.waiting.pop_front()?;
        let (agent, console, root) = (self.agent.clone(), self.console.clone(), self.root.clone());
        self.running_chat = Some(chat);
        self.running.spawn(async move {
            let context = TickActionContext {
                agent: &agent,
                console: &console,
                root: root.as_deref(),
                chat,
            };
            execute(action, context).await
        });
        Some(chat)
    }

    fn show_waiting(&self, chat: Uuid) -> Result<(), String> {
        let waiting = self.waiting.iter().any(|(waiting, _)| *waiting == chat);
        self.console.set_monitor_action_waiting(chat, waiting)
    }
}

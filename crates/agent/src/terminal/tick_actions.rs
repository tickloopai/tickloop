//! Adapter-owned list actions and details, with a shared terminal presentation boundary.
pub(crate) mod builtin;

use super::{
    console::{Console, InputEvent, State},
    execution::ConsoleExecution,
    style::{Role, RoleLine},
};
use async_trait::async_trait;
use crossterm::event::KeyEvent;
use std::{any::Any, path::Path, sync::Arc};
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
}
pub struct TickConfirmationView {
    pub title: String,
    pub message: String,
    pub label: String,
}
pub trait TickDetailView: Send + Any {
    fn confirmation(&self) -> Option<TickConfirmationView> {
        None
    }

    fn id(&self) -> Uuid;
    fn chat(&self) -> Uuid;
    fn lines(&mut self, width: usize, height: usize, now: u64) -> Vec<TickDetailLine>;
    fn hint(&self) -> String;
    fn key(&mut self, key: KeyEvent, width: usize, height: usize) -> TickDetailKey;
    fn poll(&mut self, _now: u64) -> Option<PreparedTickAction> {
        None
    }
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
        self.state.tick_list.detail = Some(detail);
        self.state.rendered_region = None;
    }
    pub fn notify(&mut self, message: &str) {
        self.state.hold_status(message);
    }
}

pub trait TickActionExecutor: Send + Sync {
    fn prepare(&self, ui: &mut TickActionUi<'_>) -> Result<PreparedTickAction, String>;
}
#[async_trait]
pub trait TickActionBackend: Send + Sync {
    async fn execute(&self, context: TickActionContext<'_>) -> Result<TickActionOutcome, String>;
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
pub enum TickActionOutcome {
    Done,
    Prompt(String),
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
        self.console.set_status(message)
    }
    pub fn update_detail<T: TickDetailView, R>(
        &self,
        panel: Uuid,
        update: impl FnOnce(&mut T) -> Option<R>,
    ) -> Result<Option<R>, String> {
        self.console.update_tick_detail(self.chat, panel, update)
    }
}

pub(super) async fn execute(
    action: PreparedTickAction,
    context: TickActionContext<'_>,
) -> Result<Option<InputEvent>, String> {
    match action.backend.execute(context).await? {
        TickActionOutcome::Done => Ok(None),
        TickActionOutcome::Prompt(text) => Ok(Some(InputEvent::Prompt {
            text,
            file_references: Vec::new(),
            images: Vec::new(),
        })),
    }
}

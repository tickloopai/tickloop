//! The id types of the Agent's own objects. Each is a [`tickloop_ids::Id`] of its own kind, so
//! one kind of id cannot be passed where another is expected.

pub use tick_protocol::{HostedRunId, HostedRunKind, ShellTaskId, ShellTaskKind};
use tickloop_ids::{Id, Kind};

pub enum ChatKind {}

impl Kind for ChatKind {
    const NOUN: &'static str = "agent";
    const SOURCE: &'static str = "Use a handle or chat_id from family_members in the host state.";
}

pub enum LoopKind {}

impl Kind for LoopKind {
    const NOUN: &'static str = "loop";
}

pub enum RunKind {}

impl Kind for RunKind {
    const NOUN: &'static str = "run";
}

pub enum InputKind {}

impl Kind for InputKind {
    const NOUN: &'static str = "input";
}

pub enum ResponseKind {}

impl Kind for ResponseKind {
    const NOUN: &'static str = "response";
}

pub type ChatId = Id<ChatKind>;

pub type LoopId = Id<LoopKind>;

pub type RunId = Id<RunKind>;

/// The identity a host gives one input, kept in the Chat's input ledger.
pub type InputId = Id<InputKind>;

/// One displayed answer: one attempt of one Run.
pub type ResponseId = Id<ResponseKind>;

/// A message one Agent of a family sends another.
pub enum AgentMessageKind {}

impl Kind for AgentMessageKind {
    const NOUN: &'static str = "agent message";
}

/// The exchange a family message belongs to.
pub enum MessageChainKind {}

impl Kind for MessageChainKind {
    const NOUN: &'static str = "message chain";
}

pub type AgentMessageId = Id<AgentMessageKind>;

pub type MessageChainId = Id<MessageChainKind>;

/// One delegation of a task from a parent Agent to a Subagent.
pub enum DelegationKind {}

impl Kind for DelegationKind {
    const NOUN: &'static str = "delegation";
}

pub type DelegationId = Id<DelegationKind>;

/// One task of a Loop's shared task board.
pub enum SharedTaskKind {}

impl Kind for SharedTaskKind {
    const NOUN: &'static str = "task";
    const SOURCE: &'static str = "TaskList gives each task's id.";
}

pub type SharedTaskId = Id<SharedTaskKind>;

/// One scheduled task of a Chat.
pub enum ScheduledTaskIdKind {}

impl Kind for ScheduledTaskIdKind {
    const NOUN: &'static str = "scheduled task";
    const SOURCE: &'static str = "CronList gives each task's id.";
}

pub type ScheduledTaskId = Id<ScheduledTaskIdKind>;

/// One Tick Subscription of a Loop.
pub enum SubscriptionKind {}

impl Kind for SubscriptionKind {
    const NOUN: &'static str = "subscription";
    const SOURCE: &'static str =
        "The host state lists this Loop's subscriptions under loop.subscriptions.";
}

pub type SubscriptionId = Id<SubscriptionKind>;

/// One tool call's operation in a Run.
pub enum OperationKind {}

impl Kind for OperationKind {
    const NOUN: &'static str = "operation";
}

pub type OperationId = Id<OperationKind>;

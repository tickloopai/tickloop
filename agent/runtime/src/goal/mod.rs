//! A Chat's Goal: its state machine, the root Agent's goal tools, the steering texts the host
//! sends while pursuing a goal, and the completion check that verifies a reported completion.

pub(crate) mod check;
pub(crate) mod prompts;
mod runtime;

pub(crate) use runtime::{GoalTurnEnd, background_work_runs, charged_tokens, current_continuation};
pub(crate) mod state;
mod store;
pub(crate) mod tools;

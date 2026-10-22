//! A Chat's Goal: its state machine, the root Agent's goal tools, the steering texts the host
//! sends while pursuing a goal, and the completion check that verifies a reported completion.

pub(crate) mod check;
pub(crate) mod prompts;
pub(crate) mod state;
pub(crate) mod tools;

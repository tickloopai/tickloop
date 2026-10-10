//! The facts an Agent row summarizes from a Chat's Loops and user Shells, shared by the daemon
//! and its workers.

use tickloop_agent_protocol::{LoopSnapshot, LoopState, RunId, UserShellRecord};

pub(crate) fn summary_run_id(loops: &[LoopSnapshot], state: &LoopState) -> Option<RunId> {
    loops
        .iter()
        .filter(|goal| &goal.state == state)
        .max_by_key(|goal| goal.state_since)
        .and_then(|goal| goal.run_id)
}

/// The latest moment a Loop or user Shell of the execution entered its current state. Reading,
/// displaying or configuring an Agent never moves it; Loops and Shells without a recorded time
/// leave it unknown when nothing else is known.
pub(crate) fn updated_at(loops: &[LoopSnapshot], user_shells: &[UserShellRecord]) -> Option<u64> {
    loops
        .iter()
        .map(|goal| goal.state_since)
        .chain(user_shells.iter().map(|shell| shell.state_since))
        .flatten()
        .max()
}

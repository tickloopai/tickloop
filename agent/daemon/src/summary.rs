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

pub(crate) fn run_started_at(loops: &[LoopSnapshot]) -> Option<u64> {
    loops
        .iter()
        .filter(|goal| goal.state == LoopState::Running)
        .max_by_key(|goal| goal.state_since)
        .and_then(|goal| goal.run_started_at)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tickloop_agent_protocol::LoopSnapshot;

    #[test]
    fn current_run_start_survives_tool_state_changes_and_excludes_finished_runs() {
        let mut goal = LoopSnapshot {
            id: uuid::Uuid::new_v4(),
            goal: "Check files".into(),
            state: LoopState::Running,
            run_id: Some(uuid::Uuid::new_v4()),
            run_started_at: Some(20_000),
            trigger: None,
            state_since: Some(25_000),
            event_sources: Vec::new(),
        };
        assert_eq!(run_started_at(std::slice::from_ref(&goal)), Some(20_000));
        goal.state_since = Some(30_000);
        assert_eq!(run_started_at(std::slice::from_ref(&goal)), Some(20_000));
        goal.state = LoopState::Completed;
        assert_eq!(run_started_at(std::slice::from_ref(&goal)), None);
        let shell: UserShellRecord = serde_json::from_value(serde_json::json!({
            "id": uuid::Uuid::new_v4(), "chat_id": uuid::Uuid::new_v4(),
            "command": "sleep 30", "cwd": "/work", "state": "Running",
            "pid": 42, "exit_code": null, "signal": null, "reason": null,
            "output_ref": null, "output_bytes": 0, "submitted_at_message": 0,
            "state_since": 80_000
        }))
        .unwrap();
        assert_eq!(
            tickloop_agent_runtime::summary_state(std::slice::from_ref(&goal), &[shell]),
            LoopState::Running
        );
        assert_eq!(
            run_started_at(std::slice::from_ref(&goal)),
            None,
            "a user Shell must not reuse the completed model Run's timestamp"
        );
        goal.state = LoopState::Running;
        goal.run_id = Some(uuid::Uuid::new_v4());
        goal.run_started_at = Some(90_000);
        goal.state_since = Some(91_000);
        assert_eq!(run_started_at(std::slice::from_ref(&goal)), Some(90_000));
    }
}

//! The facts an Agent row summarizes from a Chat's Loops and user Shells, shared by the daemon
//! and its workers.

use tickloop_harness_protocol::{LoopSnapshot, LoopState, RunId, UserShellRecord};

/// The Loop's run when the row reports the Loop's own state.
pub(crate) fn summary_run_id(chat_loop: &LoopSnapshot, state: &LoopState) -> Option<RunId> {
    (&chat_loop.state == state)
        .then_some(chat_loop.run_id)
        .flatten()
}

/// The latest moment the Loop or a user Shell of the execution entered its current state.
/// Reading, displaying or configuring an Agent never moves it; without a recorded time it stays
/// unknown.
pub(crate) fn updated_at(chat_loop: &LoopSnapshot, user_shells: &[UserShellRecord]) -> Option<u64> {
    std::iter::once(chat_loop.state_since)
        .chain(user_shells.iter().map(|shell| shell.state_since))
        .flatten()
        .max()
}

pub(crate) fn run_started_at(chat_loop: &LoopSnapshot, state: &LoopState) -> Option<u64> {
    (*state == LoopState::Running && chat_loop.state == LoopState::Running)
        .then_some(chat_loop.run_started_at)
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_run_start_survives_tool_state_changes_and_excludes_finished_runs() {
        let mut chat_loop = LoopSnapshot {
            state: LoopState::Running,
            run_id: Some(uuid::Uuid::new_v4().into()),
            run_started_at: Some(20_000),
            state_since: Some(25_000),
            ..LoopSnapshot::new(uuid::Uuid::new_v4().into())
        };
        assert_eq!(
            run_started_at(&chat_loop, &LoopState::Running),
            Some(20_000)
        );
        assert_eq!(
            run_started_at(&chat_loop, &LoopState::WaitingQuestion),
            None
        );
        let mut waiting = chat_loop.clone();
        waiting.state = LoopState::WaitingQuestion;
        let state = tickloop_harness_runtime::summary_state(&waiting, &[]);
        assert_eq!(summary_run_id(&waiting, &state), waiting.run_id);
        assert_eq!(run_started_at(&waiting, &state), None);
        chat_loop.state_since = Some(30_000);
        assert_eq!(
            run_started_at(&chat_loop, &LoopState::Running),
            Some(20_000)
        );
        chat_loop.state = LoopState::Idle;
        assert_eq!(run_started_at(&chat_loop, &LoopState::Running), None);
        let shell: UserShellRecord = serde_json::from_value(serde_json::json!({
            "id": uuid::Uuid::new_v4(), "chat_id": uuid::Uuid::new_v4(),
            "command": "sleep 30", "cwd": "/work", "state": "Running",
            "pid": 42, "exit_code": null, "signal": null, "reason": null,
            "output_ref": null, "output_bytes": 0, "submitted_at_message": 0,
            "state_since": 80_000
        }))
        .unwrap();
        assert_eq!(
            tickloop_harness_runtime::summary_state(&chat_loop, std::slice::from_ref(&shell)),
            LoopState::Running
        );
        assert_eq!(
            run_started_at(&chat_loop, &LoopState::Running),
            None,
            "a user Shell must not reuse the completed model Run's timestamp"
        );
        assert_eq!(updated_at(&chat_loop, &[shell]), Some(80_000));
        chat_loop.state = LoopState::Running;
        chat_loop.run_id = Some(uuid::Uuid::new_v4().into());
        chat_loop.run_started_at = Some(90_000);
        chat_loop.state_since = Some(91_000);
        assert_eq!(
            run_started_at(&chat_loop, &LoopState::Running),
            Some(90_000)
        );
    }
}

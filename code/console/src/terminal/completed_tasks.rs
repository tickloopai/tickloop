use std::time::{Duration, Instant};

use tickloop_harness::protocol::TaskBoard;
use uuid::Uuid;

const COMPLETED_TASK_RETENTION: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CompletedTaskCleanup {
    pub(super) root_chat_id: Uuid,
    pub(super) root_loop_id: Uuid,
    pub(super) expected_revision: u64,
}

struct CompletedTaskObservation {
    cleanup: CompletedTaskCleanup,
    observed_at: Instant,
    task_ids: Vec<Uuid>,
    dispatched: bool,
}

#[derive(Default)]
pub(super) struct CompletedTaskRetention {
    observation: Option<CompletedTaskObservation>,
}

impl CompletedTaskRetention {
    pub(super) fn observe(
        &mut self,
        board: Option<&TaskBoard>,
        now: Instant,
    ) -> Option<CompletedTaskCleanup> {
        let Some(board) = board.filter(|board| board.is_finished()) else {
            self.observation = None;
            return None;
        };
        let cleanup = CompletedTaskCleanup {
            root_chat_id: board.root_chat_id.into(),
            root_loop_id: board.root_loop_id.into(),
            expected_revision: board.revision,
        };
        let observation = self.observation.get_or_insert(CompletedTaskObservation {
            cleanup,
            observed_at: now,
            task_ids: board.tasks.iter().map(|task| task.id.uuid()).collect(),
            dispatched: false,
        });
        if observation.cleanup.root_chat_id != cleanup.root_chat_id
            || observation.cleanup.root_loop_id != cleanup.root_loop_id
            || !observation
                .task_ids
                .iter()
                .copied()
                .eq(board.tasks.iter().map(|task| task.id))
            || (observation.dispatched && observation.cleanup != cleanup)
        {
            *observation = CompletedTaskObservation {
                cleanup,
                observed_at: now,
                task_ids: board.tasks.iter().map(|task| task.id.uuid()).collect(),
                dispatched: false,
            };
        }
        observation.cleanup = cleanup;
        if !observation.dispatched
            && now.saturating_duration_since(observation.observed_at) >= COMPLETED_TASK_RETENTION
        {
            // Do not repeat an uncertain command on every refresh. A new board revision or
            // leaving and reopening its scope permits another conditional attempt.
            observation.dispatched = true;
            Some(cleanup)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::status_area::test_task_board;
    use tickloop_harness::protocol::SharedTaskStatus;

    fn completed_board() -> TaskBoard {
        let mut board = test_task_board(&["First", "Second"]);
        for task in &mut board.tasks {
            task.status = SharedTaskStatus::Completed;
        }
        board
    }

    #[test]
    fn completed_tasks_clear_after_thirty_seconds_with_latest_revision_once() {
        let mut retention = CompletedTaskRetention::default();
        let mut board = completed_board();
        let start = Instant::now();
        assert_eq!(retention.observe(Some(&board), start), None);
        assert_eq!(
            retention.observe(Some(&board), start + Duration::from_secs(29)),
            None
        );
        board.revision += 1;
        assert_eq!(
            retention.observe(Some(&board), start + Duration::from_secs(30)),
            Some(CompletedTaskCleanup {
                root_chat_id: board.root_chat_id.into(),
                root_loop_id: board.root_loop_id.into(),
                expected_revision: board.revision,
            })
        );
        assert_eq!(
            retention.observe(Some(&board), start + Duration::from_secs(90)),
            None
        );
    }

    #[test]
    fn unfinished_empty_or_disabled_boards_cancel_the_pending_cleanup() {
        for status in [
            SharedTaskStatus::Pending,
            SharedTaskStatus::InProgress,
            SharedTaskStatus::NeedsAttention,
        ] {
            let mut retention = CompletedTaskRetention::default();
            let mut board = completed_board();
            let start = Instant::now();
            retention.observe(Some(&board), start);
            board.tasks[0].status = status;
            assert_eq!(
                retention.observe(Some(&board), start + Duration::from_secs(29)),
                None
            );
            board.tasks[0].status = SharedTaskStatus::Completed;
            assert_eq!(
                retention.observe(Some(&board), start + Duration::from_secs(30)),
                None
            );
            retention.observe(None, start + Duration::from_secs(59));
            assert_eq!(
                retention.observe(Some(&board), start + Duration::from_secs(60)),
                None
            );
            let mut empty = board.clone();
            empty.tasks.clear();
            retention.observe(Some(&empty), start + Duration::from_secs(89));
            assert_eq!(
                retention.observe(Some(&board), start + Duration::from_secs(90)),
                None
            );
        }
    }

    #[test]
    fn a_board_of_completed_and_cancelled_tasks_is_finished_and_cleared() {
        let mut retention = CompletedTaskRetention::default();
        let mut board = completed_board();
        board.tasks[1].status = SharedTaskStatus::Cancelled;
        let start = Instant::now();
        assert_eq!(retention.observe(Some(&board), start), None);
        assert!(
            retention
                .observe(Some(&board), start + Duration::from_secs(30))
                .is_some()
        );
    }

    #[test]
    fn a_new_completed_task_gets_its_full_retention_period() {
        let mut retention = CompletedTaskRetention::default();
        let mut board = completed_board();
        let start = Instant::now();
        retention.observe(Some(&board), start);
        let mut added = board.tasks[0].clone();
        added.id = Uuid::new_v4().into();
        board.tasks.push(added);
        board.revision += 1;
        assert_eq!(
            retention.observe(Some(&board), start + Duration::from_secs(30)),
            None
        );
        assert!(
            retention
                .observe(Some(&board), start + Duration::from_secs(60))
                .is_some()
        );
    }

    #[test]
    fn switching_root_scope_starts_a_new_retention_period() {
        let mut retention = CompletedTaskRetention::default();
        let first = completed_board();
        let mut second = first.clone();
        second.root_loop_id = Uuid::new_v4().into();
        let start = Instant::now();
        retention.observe(Some(&first), start);
        assert_eq!(
            retention.observe(Some(&second), start + Duration::from_secs(29)),
            None
        );
        assert_eq!(
            retention.observe(Some(&first), start + Duration::from_secs(30)),
            None
        );
        assert!(
            retention
                .observe(Some(&first), start + Duration::from_secs(60))
                .is_some()
        );
    }

    #[test]
    fn an_unchanged_board_does_not_retry_an_unknown_result_but_reentry_can() {
        let mut retention = CompletedTaskRetention::default();
        let board = completed_board();
        let start = Instant::now();
        retention.observe(Some(&board), start);
        assert!(
            retention
                .observe(Some(&board), start + Duration::from_secs(30))
                .is_some()
        );
        assert_eq!(
            retention.observe(Some(&board), start + Duration::from_secs(120)),
            None
        );
        retention.observe(None, start + Duration::from_secs(121));
        retention.observe(Some(&board), start + Duration::from_secs(122));
        assert!(
            retention
                .observe(Some(&board), start + Duration::from_secs(152))
                .is_some()
        );
    }
}

//! Send now, following Claude Code's low-latency submit: a queued user message reaches the model
//! at once. The foreground calls of the Run ahead that go on unobserved, Subagents and Bash
//! commands, move to the background without being interrupted, so the Run reaches its tool
//! boundary and takes the message there; each later completes as a task notification. When only
//! a model reply is ahead, or other work still holds the Run after a short grace, the Run is
//! interrupted and the message starts at once.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use tickloop_harness_protocol::{ChatId, CommandState, OperationRecord, QueuedInputKind, RunId};
use tokio_util::sync::CancellationToken;

use super::{Agent, AgentError, Chat};

/// How often Claude Code's low-latency submit looks at the Run again.
const POLL_INTERVAL: Duration = Duration::from_millis(200);
/// Claude Code's polls for moved calls' results to land before it looks at other work.
const MOVED_RESULT_POLLS: u32 = 8;
/// Claude Code's polls for work that cannot move to end before it interrupts.
const UNMOVABLE_GRACE_POLLS: u32 = 2;

/// A foreground call send now can move to the background, listed while the call waits.
pub(crate) struct MovableCall<'a> {
    agent: &'a Agent,
    key: (RunId, String),
    moved: CancellationToken,
}

impl MovableCall<'_> {
    /// Ends when send now moves the call to the background.
    pub(crate) async fn moved(&self) {
        self.moved.cancelled().await;
    }
}

impl Drop for MovableCall<'_> {
    fn drop(&mut self) {
        self.agent
            .inner
            .movable_calls
            .lock()
            .unwrap()
            .remove(&self.key);
    }
}

/// What stands between a message sent now and the model, after the movable calls moved.
struct WorkAhead {
    moved: usize,
    /// Calls of the running Run that have not returned.
    is_executing: bool,
}

impl Agent {
    /// Sends the queued user message `run_id` now. Moves the running Run's foreground Subagents
    /// and Bash commands to the background, and keeps moving those that start until a Run takes
    /// the message. It interrupts the work ahead at once when only a model reply or a waiting Run
    /// is ahead, and otherwise when work still keeps the message waiting after Claude Code's grace
    /// while no person holds it. Each interruption checks the message under the same lock, so the
    /// run answering a message that has already joined or started it is never interrupted. Such a
    /// message, an unknown one, or one with no work ahead is left as is.
    pub async fn send_now(
        &self,
        chat_id: ChatId,
        run_id: RunId,
    ) -> Result<Vec<OperationRecord>, AgentError> {
        let Some(ahead) = self.move_calls_ahead_of(chat_id, run_id).await? else {
            return Ok(Vec::new());
        };
        if ahead.moved == 0 && !ahead.is_executing {
            return self.interrupt_ahead_of(chat_id, run_id).await;
        }
        let agent = self.clone();
        self.inner.tasks.spawn(Box::pin(async move {
            agent
                .follow_sent_message(chat_id, run_id, ahead.moved > 0)
                .await;
        }));
        Ok(Vec::new())
    }

    /// Lists the foreground call `call_id` of `run` as movable until the returned value drops.
    pub(crate) fn movable_call(&self, run: RunId, call_id: &str) -> MovableCall<'_> {
        let moved = CancellationToken::new();
        self.inner
            .movable_calls
            .lock()
            .unwrap()
            .insert((run, call_id.to_owned()), moved.clone());
        MovableCall {
            agent: self,
            key: (run, call_id.to_owned()),
            moved,
        }
    }

    /// Watches the message sent now until a Run takes it: moves the calls that become movable,
    /// and interrupts the work ahead once it has waited out the grace with no person holding it.
    async fn follow_sent_message(&self, chat_id: ChatId, queued: RunId, mut has_moved: bool) {
        let mut waited_polls = 0;
        loop {
            tokio::select! {
                () = tokio::time::sleep(POLL_INTERVAL) => {}
                () = self.inner.shutdown.cancelled() => return,
            }
            let Ok(Some(ahead)) = self.move_calls_ahead_of(chat_id, queued).await else {
                return;
            };
            if ahead.moved > 0 || self.is_held_by_person(chat_id).await {
                has_moved |= ahead.moved > 0;
                waited_polls = 0;
                continue;
            }
            waited_polls += 1;
            let grace = if has_moved {
                MOVED_RESULT_POLLS + UNMOVABLE_GRACE_POLLS
            } else {
                UNMOVABLE_GRACE_POLLS
            };
            if waited_polls >= grace {
                let _ = self.interrupt_ahead_of(chat_id, queued).await;
                return;
            }
        }
    }

    /// Moves the movable foreground calls of the running Run ahead of the queued message `queued`
    /// to the background; `None` when the message no longer waits for work ahead.
    async fn move_calls_ahead_of(
        &self,
        chat_id: ChatId,
        queued: RunId,
    ) -> Result<Option<WorkAhead>, AgentError> {
        let (tools, shell_operations, mut ahead) = {
            let chats = self.inner.chats.lock().await;
            let chat = chats.get(&chat_id).ok_or(AgentError::NotFound)?;
            if !waits_for_work_ahead(chat, queued) {
                return Ok(None);
            }
            let Some(running) = chat.active_run.filter(|_| {
                chat.active
                    .as_ref()
                    .is_some_and(|cancel| !cancel.is_cancelled())
            }) else {
                return Ok(Some(WorkAhead {
                    moved: 0,
                    is_executing: false,
                }));
            };
            let top_level_calls = chat
                .snapshot
                .operations
                .iter()
                .filter(|operation| {
                    operation.run_id == running && operation.parent_call_id.is_none()
                })
                .map(|operation| (operation.call_id.as_str(), operation))
                .collect::<HashMap<_, _>>();
            let mut ahead = WorkAhead {
                moved: 0,
                is_executing: top_level_calls
                    .values()
                    .any(|operation| operation.result.is_none()),
            };
            for ((run, call_id), call) in self.inner.movable_calls.lock().unwrap().iter() {
                if *run == running
                    && top_level_calls.contains_key(call_id.as_str())
                    && !call.is_cancelled()
                {
                    call.cancel();
                    ahead.moved += 1;
                }
            }
            let shell_operations = top_level_calls
                .values()
                .filter(|operation| operation.name == tickloop_harness_tools::BASH_TOOL_NAME)
                .map(|operation| operation.id)
                .collect::<HashSet<_>>();
            (chat.tools.clone(), shell_operations, ahead)
        };
        for command in tools.commands().await {
            if command.state != CommandState::Running
                || !shell_operations.contains(&command.operation_id)
            {
                continue;
            }
            let Ok(command) = tools
                .background_to_deliver_message(command.operation_id)
                .await
            else {
                continue;
            };
            if command.state == CommandState::Background {
                ahead.moved += 1;
                self.record_command(chat_id, command).await?;
            }
        }
        Ok(Some(ahead))
    }

    async fn interrupt_ahead_of(
        &self,
        chat_id: ChatId,
        queued: RunId,
    ) -> Result<Vec<OperationRecord>, AgentError> {
        let mut chats = self.inner.chats.lock().await;
        let chat = chats.get_mut(&chat_id).ok_or(AgentError::NotFound)?;
        if !waits_for_work_ahead(chat, queued) {
            return Ok(Vec::new());
        }
        Ok(chat.interrupt())
    }

    /// Claude Code's dialog hold: a permission request or question waits for a person.
    async fn is_held_by_person(&self, chat_id: ChatId) -> bool {
        self.inner
            .chats
            .lock()
            .await
            .get(&chat_id)
            .is_some_and(|chat| {
                !chat.snapshot.permissions.is_empty() || !chat.snapshot.questions.is_empty()
            })
    }
}

/// Whether the queued user message `queued` still waits for work ahead of it.
fn waits_for_work_ahead(chat: &Chat, queued: RunId) -> bool {
    let is_queued_message =
        chat.snapshot.queued_inputs().iter().any(|input| {
            input.id == queued && input.armed && input.kind == QueuedInputKind::Message
        });
    let has_work_ahead = chat.active.is_some() || !chat.parked.is_empty();
    is_queued_message && has_work_ahead
}

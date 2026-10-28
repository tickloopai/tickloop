//! Claude Code's print mode: a piped View whose own input and turns are done stays open while its
//! family still works in the background. A Subagent still running, or a completion its root has
//! not yet taken, keeps it waiting; the turn that takes the completion is the View's own again,
//! and the View exits once nothing is left. A long wait is announced on stderr once for each piece
//! of work, and Ctrl+C leaves at once.

use std::collections::{HashSet, VecDeque};
use std::time::{Duration, Instant};

use tickloop_harness::protocol::{ChatId, LoopState, SubagentNotificationDelivery};

use super::execution::ConsoleExecution as Agent;

/// How often Claude Code's print mode looks at the background work again.
pub(super) const POLL_INTERVAL: Duration = Duration::from_millis(100);
/// How long the View waits before Claude Code's print mode announces the wait.
const NOTICE_DELAY: Duration = Duration::from_secs(5);
/// Claude Code shortens a task's description to this many characters in the announcement.
const MAX_DESCRIPTION_CHARACTERS: usize = 60;

/// A family member still running, as the announcement names it.
struct RunningMember {
    chat_id: uuid::Uuid,
    label: String,
}

#[derive(Default)]
pub(super) struct BackgroundWork {
    waiting_since: Option<Instant>,
    last_check: Option<(Instant, bool)>,
    announced: HashSet<uuid::Uuid>,
}

impl BackgroundWork {
    pub(super) fn is_waiting(&self) -> bool {
        self.waiting_since.is_some()
    }

    /// The View has work of its own again, such as a turn a completion started.
    pub(super) fn note_busy(&mut self) {
        self.waiting_since = None;
        self.last_check = None;
    }

    /// Whether the family of `root` still works in the background; looks again at most every
    /// [`POLL_INTERVAL`]. A family the View cannot read counts as done.
    pub(super) async fn keeps_open(&mut self, agent: &Agent, root: Option<ChatId>) -> bool {
        let now = Instant::now();
        if let Some((checked_at, keeps_open)) = self.last_check
            && now.duration_since(checked_at) < POLL_INTERVAL
        {
            return keeps_open;
        }
        let work = match root {
            Some(root) => family_work(agent, root).await,
            None => None,
        };
        let keeps_open = work.is_some();
        self.last_check = Some((now, keeps_open));
        let Some(running) = work else {
            self.waiting_since = None;
            return false;
        };
        let since = *self.waiting_since.get_or_insert(now);
        if now.duration_since(since) >= NOTICE_DELAY
            && running
                .iter()
                .any(|member| !self.announced.contains(&member.chat_id))
        {
            self.announced
                .extend(running.iter().map(|member| member.chat_id));
            eprintln!("\n{}", wait_notice(&running));
        }
        true
    }
}

/// Claude Code's announcement of a long wait.
fn wait_notice(running: &[RunningMember]) -> String {
    let work = running
        .iter()
        .map(|member| member.label.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    if work.is_empty() {
        "Waiting for background work to finish. Press Ctrl+C to exit now.".into()
    } else {
        format!("Waiting for background work to finish ({work}). Press Ctrl+C to exit now.")
    }
}

/// The members of `root`'s family still running, or `None` when the family has no background
/// work left: no member runs and the root has taken every completion.
async fn family_work(agent: &Agent, root: ChatId) -> Option<Vec<RunningMember>> {
    let rows = agent.catalog().await.ok()?;
    let root_row = rows.iter().find(|row| row.chat_id == root.uuid())?;
    let mut members = VecDeque::from([root_row.execution]);
    let mut running = Vec::new();
    while let Some(parent) = members.pop_front() {
        for row in rows.iter().filter(|row| row.parent == Some(parent)) {
            members.push_back(row.execution);
            if !matches!(
                row.state,
                LoopState::Idle | LoopState::Failed | LoopState::Interrupted
            ) {
                let description = row
                    .subagent
                    .as_ref()
                    .map_or(row.title.as_str(), |member| member.description.as_str());
                running.push(RunningMember {
                    chat_id: row.chat_id,
                    label: format!("subagent \"{}\"", shortened(description)),
                });
            }
        }
    }
    let snapshot = agent.snapshot(root.uuid()).await.ok()?;
    let has_untaken_completion = !snapshot.pending_inputs.is_empty()
        || snapshot.family_completions.iter().any(|completion| {
            matches!(
                completion.notification,
                SubagentNotificationDelivery::Pending
            )
        });
    (!running.is_empty() || has_untaken_completion).then_some(running)
}

fn shortened(text: &str) -> String {
    if text.chars().count() <= MAX_DESCRIPTION_CHARACTERS {
        return text.to_owned();
    }
    let kept = text
        .chars()
        .take(MAX_DESCRIPTION_CHARACTERS - 1)
        .collect::<String>();
    format!("{kept}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_notice_names_each_running_subagent_as_claude_code_does() {
        let running = [
            RunningMember {
                chat_id: uuid::Uuid::new_v4(),
                label: format!("subagent \"{}\"", shortened("Audit the logs")),
            },
            RunningMember {
                chat_id: uuid::Uuid::new_v4(),
                label: format!("subagent \"{}\"", shortened(&"x".repeat(80))),
            },
        ];
        assert_eq!(
            wait_notice(&running),
            format!(
                "Waiting for background work to finish (subagent \"Audit the logs\", subagent \"{}…\"). Press Ctrl+C to exit now.",
                "x".repeat(59)
            )
        );
        assert_eq!(
            wait_notice(&[]),
            "Waiting for background work to finish. Press Ctrl+C to exit now."
        );
    }
}

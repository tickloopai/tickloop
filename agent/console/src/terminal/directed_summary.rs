use super::execution::ConsoleExecution as Agent;
use std::collections::HashMap;

use tokio::task::{Id, JoinSet};
use uuid::Uuid;

use super::{console::Console, safe_line, workflows};
use tickloop_agent_protocol::{
    AccessError, AccessErrorCode, ChatSnapshot, DirectedSummaryOutcome, DirectedSummaryRequest,
    SummaryDirection,
};

#[derive(Clone, Copy)]
pub(super) struct SummaryTaskMetadata {
    pub(super) chat: Uuid,
}

pub(super) enum SummaryTaskCompletion {
    Finished {
        metadata: SummaryTaskMetadata,
        result: Box<Result<DirectedSummaryOutcome, AccessError>>,
    },
    ObserverStopped {
        metadata: SummaryTaskMetadata,
    },
}

#[derive(Default)]
pub(super) struct DirectedSummaryTasks {
    tasks: JoinSet<(Uuid, Result<DirectedSummaryOutcome, AccessError>)>,
    metadata: HashMap<Id, SummaryTaskMetadata>,
}

impl DirectedSummaryTasks {
    pub(super) fn start(
        &mut self,
        agent: &Agent,
        chat: Uuid,
        request: DirectedSummaryRequest,
    ) -> SummaryDirection {
        let direction = request.direction;
        let agent = agent.clone();
        let task = self
            .tasks
            .spawn(async move { (chat, agent.summarize_checkpoint(chat, request).await) });
        self.metadata
            .insert(task.id(), SummaryTaskMetadata { chat });
        direction
    }

    pub(super) fn contains(&self, chat: Uuid) -> bool {
        self.metadata.values().any(|metadata| metadata.chat == chat)
    }

    pub(super) fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    pub(super) async fn join_next(&mut self) -> Result<Option<SummaryTaskCompletion>, String> {
        let Some(result) = self.tasks.join_next_with_id().await else {
            return Ok(None);
        };
        let task_id = match &result {
            Ok((task_id, _)) => *task_id,
            Err(error) => error.id(),
        };
        let metadata = self
            .metadata
            .remove(&task_id)
            .ok_or("unknown directed summary task")?;
        Ok(Some(match result {
            Ok((_, (chat, result))) if chat == metadata.chat => SummaryTaskCompletion::Finished {
                metadata,
                result: Box::new(result),
            },
            Ok((_, _)) => {
                return Err("directed summary completed for an unexpected Chat".into());
            }
            Err(_) => SummaryTaskCompletion::ObserverStopped { metadata },
        }))
    }
}

pub(super) struct DirectedSummaryEffect {
    pub(super) snapshot: ChatSnapshot,
    pub(super) clear_goal: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct DirectedSummaryFailureEffect {
    pub(super) failed: bool,
}

pub(super) fn finish_success(
    console: &Console,
    outcome: DirectedSummaryOutcome,
) -> Result<DirectedSummaryEffect, String> {
    let chat = outcome.snapshot.id;
    let direction = outcome.direction;
    console.reset_chat(chat)?;
    workflows::write_public_history(console, &outcome.snapshot)?;
    console.write(&format!(
        "Conversation summarized {}. ctrl+o opens the detailed transcript.\n",
        direction_phrase(direction)
    ))?;
    let clear_goal = if let Some(draft) = outcome.draft {
        console.restore_rewind_draft(&draft.prompt, &draft.contexts)?;
        true
    } else {
        console.clear_summary_draft()?;
        false
    };
    if let Some(error) = outcome.continuation_error {
        console.write(&format!(
            "Summary completed. Follow-up preparation needs attention: {}\n",
            safe_line(&error)
        ))?;
    }
    console.scroll_conversation_to_end()?;
    Ok(DirectedSummaryEffect {
        snapshot: outcome.snapshot,
        clear_goal,
    })
}

/// 失败、被拒绝或取消的摘要不改变对话，也没有写入 Transcript；按错误类型说明结果。
pub(super) fn finish_failure(
    console: &Console,
    error: &AccessError,
) -> Result<DirectedSummaryFailureEffect, String> {
    let (message, failed) = failure_presentation(error);
    console.write(&format!("{message}: {}\n", safe_line(&error.to_string())))?;
    Ok(DirectedSummaryFailureEffect { failed })
}

fn failure_presentation(error: &AccessError) -> (&'static str, bool) {
    match error.code {
        AccessErrorCode::Invalid | AccessErrorCode::Busy => (
            "Conversation summary was not applied; conversation and draft are unchanged",
            false,
        ),
        _ => (
            "Conversation summary failed; conversation and draft are unchanged",
            true,
        ),
    }
}

pub(super) fn direction_phrase(direction: SummaryDirection) -> &'static str {
    match direction {
        SummaryDirection::FromHere => "from here",
        SummaryDirection::UpToHere => "up to here",
    }
}

/// Where earlier messages were summarized, as Claude Code's `CompactSummary` marks it: the `⏺`,
/// a bold label and a muted hint; the summary itself is in the detailed transcript.
pub(super) fn history_marker(
    display: super::display::DisplayProfile,
) -> Vec<(String, Option<super::style::Role>)> {
    use super::style::Role;
    vec![
        ("\n".into(), None),
        (
            display.glyph(super::display::Glyph::Assistant).into(),
            Some(Role::Text),
        ),
        (" ".into(), None),
        ("Summarized conversation".into(), Some(Role::Strong)),
        (" (ctrl+o to expand)\n".into(), Some(Role::MutedText)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_rejection_is_not_a_failure() {
        assert!(!failure_presentation(&AccessError::invalid("stale")).1);
        assert!(!failure_presentation(&AccessError::new(AccessErrorCode::Busy, "busy")).1);
        assert!(failure_presentation(&AccessError::unavailable("worker lost")).1);
        assert!(failure_presentation(&AccessError::storage("disk full")).1);
    }
}

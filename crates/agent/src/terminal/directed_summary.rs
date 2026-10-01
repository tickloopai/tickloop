use std::collections::HashMap;

use tickloop_agent::model::ModelError;
use tickloop_agent::runtime::{
    Agent, AgentError, ChatSnapshot, DirectedSummaryOutcome, DirectedSummaryRequest,
    SummaryDirection,
};
use tokio::task::{Id, JoinSet};
use uuid::Uuid;

use super::{console::Console, safe_line, safe_text, workflows};

#[derive(Clone, Copy)]
pub(super) struct SummaryTaskMetadata {
    pub(super) chat: Uuid,
}

pub(super) enum SummaryTaskCompletion {
    Finished {
        metadata: SummaryTaskMetadata,
        result: Box<Result<DirectedSummaryOutcome, AgentError>>,
    },
    ObserverStopped {
        metadata: SummaryTaskMetadata,
    },
}

#[derive(Default)]
pub(super) struct DirectedSummaryTasks {
    tasks: JoinSet<(Uuid, Result<DirectedSummaryOutcome, AgentError>)>,
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
        "Conversation summarized {}. Ctrl+O opens the full archive.\n",
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
    error: &AgentError,
) -> Result<DirectedSummaryFailureEffect, String> {
    let (message, failed) = failure_presentation(error);
    console.write(&format!("{message}: {}\n", safe_line(&error.to_string())))?;
    Ok(DirectedSummaryFailureEffect { failed })
}

fn failure_presentation(error: &AgentError) -> (&'static str, bool) {
    match error {
        AgentError::Model(ModelError::Cancelled) => (
            "Conversation summary cancelled; conversation and draft are unchanged",
            false,
        ),
        AgentError::Model(ModelError::TimedOut) => (
            "Conversation summary timed out; conversation and draft are unchanged",
            true,
        ),
        AgentError::Invalid(_) | AgentError::Busy => (
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

pub(super) fn history_marker(summary: &str) -> String {
    format!("\nSummarized conversation\n{}\n", safe_text(summary))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_cancellation_and_rejection_are_not_failures() {
        assert!(!failure_presentation(&AgentError::Model(ModelError::Cancelled)).1);
        assert!(!failure_presentation(&AgentError::Invalid("stale".into())).1);
        assert!(failure_presentation(&AgentError::Model(ModelError::TimedOut)).1);
        assert!(failure_presentation(&AgentError::Model(ModelError::Connection)).1);
    }
}

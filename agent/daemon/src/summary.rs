//! The facts an Agent row summarizes from a Chat snapshot, shared by the daemon and its workers.

use tickloop_agent_protocol::{
    ChatSnapshot, LocalMessage, LoopSnapshot, LoopState, RunId, UserShellRecord,
};

pub(crate) fn summary_run_id(loops: &[LoopSnapshot], state: &LoopState) -> Option<RunId> {
    loops
        .iter()
        .filter(|goal| &goal.state == state)
        .max_by_key(|goal| goal.state_since)
        .and_then(|goal| goal.run_id)
}

/// The Loop state an Agent row reports: the goal most in need of attention, so a later completed
/// Loop cannot mask an earlier goal that still waits for input, plan review or its Trigger, and an
/// active user Shell keeps the Agent working. Without Loops, the latest user Shell decides between
/// a confirmed failure and Idle; an unknown Shell outcome is not reported as a failure.
/// Whether an execution ever received work. Configuration changes alone leave it empty.
pub(crate) fn has_work(
    conversation: &ChatSnapshot,
    user_shells: &[UserShellRecord],
    queued: usize,
) -> bool {
    !conversation.loops.is_empty()
        || !conversation.messages.is_empty()
        || !user_shells.is_empty()
        || queued > 0
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

const PREVIEW_MAX_CHARACTERS: usize = 160;

/// The latest conversation content as one bounded line: the newest reply, or the newest typed
/// prompt with its text (an image-only prompt reads as an image cue). Tool traffic, thinking and
/// local notices are skipped; text that only repeats `title` adds nothing.
pub(crate) fn preview(conversation: &ChatSnapshot, title: &str) -> Option<String> {
    use tickloop_agent_protocol::{Content, Role};
    let text = conversation
        .messages
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, message)| {
            if conversation
                .message_ids
                .get(&index)
                .is_some_and(|id| conversation.agent_message_origins.contains_key(id))
            {
                return None;
            }
            let texts = message
                .content
                .iter()
                .filter_map(|content| match content {
                    Content::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            let text = match message.role {
                Role::Assistant => texts.last().copied()?.to_owned(),
                Role::User
                    if message
                        .content
                        .iter()
                        .any(|content| matches!(content, Content::ToolResult { .. })) =>
                {
                    return None;
                }
                Role::User if texts.iter().all(|text| text.trim().is_empty()) => {
                    let images = message
                        .content
                        .iter()
                        .filter(|content| matches!(content, Content::Image { .. }))
                        .count();
                    match images {
                        0 => return None,
                        1 => "[image]".to_owned(),
                        images => format!("[{images} images]"),
                    }
                }
                Role::User => texts.join("\n"),
                _ => return None,
            };
            (!text.trim().is_empty()
                && matches!(
                    LocalMessage::parse(&text),
                    None | Some(LocalMessage::AgentCommunication { .. })
                ))
            .then_some(text)
        })?;
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line == title.split_whitespace().collect::<Vec<_>>().join(" ") {
        return None;
    }
    Some(line.chars().take(PREVIEW_MAX_CHARACTERS).collect())
}

//! Host notes about a Chat's Loop and Subagents. The system prompt never names the Loop's Tick
//! Subscriptions or the family's members, so that changing them keeps the prompt cache. Whether
//! the Loop receives needs no note: continuing the Chat or sending it an input, which every
//! request the user starts follows, makes it receive again. The model's own subscribe and unsubscribe calls and its Subagent calls answer in
//! their tool results, and Subagent completions arrive as task notifications; a note at the end
//! of the conversation tells the model only what changed without it, and after a compaction or a
//! load, Claude Code's `task_status` reminder of what still runs.

use std::collections::BTreeMap;

use tick_protocol::InstanceRef;
use tickloop_harness_protocol::{
    ChatSnapshot, HostNote, Receipt, SubscriptionId, TickSubscriptionFrequency,
};
use tickloop_ids::IdContext;
use uuid::Uuid;

use crate::{Agent, AgentError, Chat, FamilyAgent, FamilyAvailability};

/// What the model was last told of the Loop, and where in the history: a compaction or a shorter
/// history, as after a rewind, leaves it untold again.
#[derive(Clone)]
pub(super) struct AnnouncedLoopState {
    active_start: usize,
    history_len: usize,
    view: LoopStateView,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LoopStateView {
    subscriptions: BTreeMap<Uuid, SubscriptionView>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SubscriptionView {
    instance: InstanceRef,
    frequency: TickSubscriptionFrequency,
    paused: bool,
}

impl LoopStateView {
    fn of(snapshot: &ChatSnapshot) -> Self {
        Self {
            subscriptions: snapshot
                .chat_loop
                .subscriptions
                .iter()
                .map(|subscription| {
                    (
                        subscription.id,
                        SubscriptionView {
                            instance: subscription.instance.clone(),
                            frequency: subscription.frequency,
                            paused: subscription.paused,
                        },
                    )
                })
                .collect(),
        }
    }
}

impl Agent {
    /// Tells the model, at the end of the conversation, what changed in its Loop without its own
    /// tool calls; after a compaction or a load, which leave it untold, what still runs.
    pub(super) async fn announce_loop_state(&self, receipt: &Receipt) -> Result<(), AgentError> {
        let family = self.family_agents(receipt.chat_id).await?;
        let mut chats = self.inner.chats.lock().await;
        let chat = chats
            .get_mut(&receipt.chat_id)
            .ok_or(AgentError::NotFound)?;
        if chat.active_run != Some(receipt.run_id) {
            return Ok(());
        }
        let current = LoopStateView::of(&chat.snapshot);
        let active_start = chat.snapshot.active_start();
        let text = match &chat.announced_loop_state {
            Some(announced)
                if announced.active_start == active_start
                    && chat.snapshot.messages.len() >= announced.history_len =>
            {
                change_text(&announced.view, &current, &chat.ids)
            }
            _ => {
                let running = running_children(&family, chat.snapshot.id.into());
                snapshot_text(&current, &running, &chat.ids)
            }
        };
        if let Some(text) = text {
            crate::host_notes::append_note(&mut chat.snapshot, receipt, text, HostNote::LoopState);
            self.save(&chat.snapshot);
        }
        chat.announced_loop_state = Some(AnnouncedLoopState {
            active_start,
            history_len: chat.snapshot.messages.len(),
            view: current,
        });
        Ok(())
    }
}

/// The model's own TickManage call changed the subscriptions and its tool result says how, so
/// the next request owes no note for them.
pub(super) fn acknowledge_subscriptions(chat: &mut Chat) {
    let current = LoopStateView::of(&chat.snapshot);
    if let Some(announced) = &mut chat.announced_loop_state {
        announced.view.subscriptions = current.subscriptions;
    }
}

/// The caller's direct children still working or waiting for input.
fn running_children(family: &[FamilyAgent], parent: Uuid) -> Vec<&FamilyAgent> {
    family
        .iter()
        .filter(|member| {
            member.parent_chat_id.map(Uuid::from) == Some(parent)
                && matches!(
                    member.availability,
                    FamilyAvailability::Working | FamilyAvailability::NeedsInput
                )
        })
        .collect()
}

fn snapshot_text(
    current: &LoopStateView,
    running: &[&FamilyAgent],
    ids: &IdContext,
) -> Option<String> {
    if current.subscriptions.is_empty() && running.is_empty() {
        return None;
    }
    let mut lines = vec![
        "Host notice: the current state of this Chat's Tick Subscriptions and running Subagents, which replaces earlier statements about them.".to_owned(),
    ];
    if !current.subscriptions.is_empty() {
        lines.push("Tick Subscriptions:".into());
        lines.extend(
            current.subscriptions.iter().map(|(id, subscription)| {
                format!("- {}", subscription_line(*id, subscription, ids))
            }),
        );
    }
    // Claude Code's `task_status` reminder for a background agent still running.
    lines.extend(running.iter().map(|child| {
        format!(
            "Background agent \"{}\" ({}) is still running. Do NOT spawn a duplicate. You will be notified when it completes. Send it a message with {} if you need a progress report before then.",
            child.handle,
            ids.model_text_of(child.chat_id.into()),
            tickloop_harness_protocol::SEND_AGENT_MESSAGE_TOOL_NAME,
        )
    }));
    Some(lines.join("\n"))
}

fn change_text(
    announced: &LoopStateView,
    current: &LoopStateView,
    ids: &IdContext,
) -> Option<String> {
    let mut lines = Vec::new();
    for (id, subscription) in &current.subscriptions {
        match announced.subscriptions.get(id) {
            None => lines.push(format!(
                "- Added: {}",
                subscription_line(*id, subscription, ids)
            )),
            Some(before) if before != subscription => lines.push(format!(
                "- Now: {}",
                subscription_line(*id, subscription, ids)
            )),
            Some(_) => {}
        }
    }
    for id in announced.subscriptions.keys() {
        if !current.subscriptions.contains_key(id) {
            lines.push(format!(
                "- Cancelled: {}",
                ids.model_text(SubscriptionId::from(*id))
            ));
        }
    }
    if !lines.is_empty() {
        lines.insert(
            0,
            "Host notice: this Chat's Tick Subscriptions changed outside your tool calls:".into(),
        );
    }
    (!lines.is_empty()).then(|| lines.join("\n"))
}

fn subscription_line(id: Uuid, subscription: &SubscriptionView, ids: &IdContext) -> String {
    format!(
        "{} on Tick Instance {}, {}{}",
        ids.model_text(SubscriptionId::from(id)),
        subscription.instance.model_text(ids),
        snake_case(&subscription.frequency),
        if subscription.paused { ", paused" } else { "" }
    )
}

fn snake_case(value: &impl serde::Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(subscriptions: &[(u128, &str, bool)]) -> LoopStateView {
        LoopStateView {
            subscriptions: subscriptions
                .iter()
                .map(|(id, run, paused)| {
                    (
                        Uuid::from_u128(*id),
                        SubscriptionView {
                            instance: InstanceRef::new("calendar", run).unwrap(),
                            frequency: TickSubscriptionFrequency::High,
                            paused: *paused,
                        },
                    )
                })
                .collect(),
        }
    }

    fn child(parent: Uuid, availability: FamilyAvailability) -> FamilyAgent {
        FamilyAgent {
            chat_id: Uuid::from_u128(7).into(),
            parent_chat_id: Some(parent.into()),
            depth: 1,
            role: crate::FamilyRole::Child,
            handle: "audit".into(),
            display_name: "audit".into(),
            description: None,
            availability,
            run_state: None,
            created_at: None,
            run_started_at: None,
            idle_since: None,
        }
    }

    #[test]
    fn only_what_changed_outside_the_models_calls_is_told() {
        let ids = IdContext::new();
        let before = view(&[(1, "run-1", false), (2, "run-2", false)]);
        assert_eq!(change_text(&before, &before, &ids), None);
        let after = view(&[(2, "run-2", true), (3, "run-3", false)]);
        let text = change_text(&before, &after, &ids).unwrap();
        let lines = text.lines().collect::<Vec<_>>();
        assert_eq!(
            lines[0],
            "Host notice: this Chat's Tick Subscriptions changed outside your tool calls:"
        );
        assert!(lines[1].starts_with("- Now: ") && lines[1].ends_with("high, paused"));
        assert!(lines[2].starts_with("- Added: ") && lines[2].contains("run-3"));
        assert_eq!(
            lines[3],
            format!(
                "- Cancelled: {}",
                ids.model_text(SubscriptionId::from(Uuid::from_u128(1)))
            )
        );
    }

    /// Claude Code's `task_status` reminder names each running child; finished children and
    /// grandchildren are left to the notifications and ListSubagents.
    #[test]
    fn the_snapshot_names_the_subscriptions_and_running_children_only() {
        let ids = IdContext::new();
        let parent = Uuid::from_u128(1);
        assert_eq!(snapshot_text(&view(&[]), &[], &ids), None);
        let family = [
            child(parent, FamilyAvailability::Working),
            child(parent, FamilyAvailability::Idle),
            child(Uuid::from_u128(9), FamilyAvailability::Working),
        ];
        let running = running_children(&family, parent);
        assert_eq!(running.len(), 1);
        let text = snapshot_text(&view(&[(1, "run-1", false)]), &running, &ids).unwrap();
        assert!(text.contains("Tick Subscriptions:\n- "), "{text}");
        assert!(text.ends_with(&format!(
            "Background agent \"audit\" ({}) is still running. Do NOT spawn a duplicate. You will be notified when it completes. Send it a message with SendMessage if you need a progress report before then.",
            ids.model_text_of(Uuid::from_u128(7))
        )), "{text}");
    }
}

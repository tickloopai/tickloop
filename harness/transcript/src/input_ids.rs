//! Name-based ids of the inputs the Agent queues for itself, and of the host's own rules. The
//! same source names the same id on every host, so a repeated delivery finds the input's ledger
//! record.

use sha2::{Digest, Sha256};
use tickloop_harness_protocol::TriggerIdentity;
use uuid::Uuid;

const FEEDBACK: &str = "tickloop.input.feedback";
const TICK_UPDATE: &str = "tickloop.input.tick-update";
const TICK_MANUAL: &str = "tickloop.input.tick-manual";
const GOAL_CONTINUATION: &str = "tickloop.input.goal-continuation";
const SCHEDULED_TASK: &str = "tickloop.input.scheduled-task";
const FAMILY_MESSAGE: &str = "tickloop.input.family-message";
const HOST_RULE: &str = "tickloop.host-rule";

/// The id named by `parts` within `namespace`, laid out as a name-based UUID.
fn named(namespace: &str, parts: &[&[u8]]) -> Uuid {
    let mut hasher = Sha256::new();
    hasher.update(namespace.as_bytes());
    for part in parts {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part);
    }
    let mut bytes = [0; 16];
    bytes.copy_from_slice(&hasher.finalize()[..16]);
    uuid::Builder::from_sha1_bytes(bytes).into_uuid()
}

/// An @tickloop request, by the delivery that brought it.
pub fn feedback(origin: &str, account: &str, delivery: Uuid) -> Uuid {
    named(
        FEEDBACK,
        &[origin.as_bytes(), account.as_bytes(), delivery.as_bytes()],
    )
}

/// A Trigger delivery to the Chat `chat`, by its identity: Tick Updates by subscription and
/// sequence, a sent Tick by its instance and sequence, feedback by its delivery, a Goal
/// continuation by the Goal and the turn it follows, a scheduled run by its task and time.
pub fn trigger(chat: Uuid, identity: &TriggerIdentity) -> Uuid {
    match identity {
        TriggerIdentity::TickUpdate { updates } => {
            let parts: Vec<Vec<u8>> = updates
                .iter()
                .map(|update| {
                    [
                        update.subscription.as_bytes().as_slice(),
                        &update.sequence.to_be_bytes(),
                    ]
                    .concat()
                })
                .collect();
            let parts: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
            named(TICK_UPDATE, &parts)
        }
        TriggerIdentity::TickManual { instance, sequence } => named(
            TICK_MANUAL,
            &[
                chat.as_bytes(),
                instance.to_string().as_bytes(),
                &sequence.to_be_bytes(),
            ],
        ),
        TriggerIdentity::ArtifactFeedback {
            origin,
            account_id,
            delivery_id,
        } => feedback(origin, account_id, *delivery_id),
        TriggerIdentity::GoalContinuation { goal_id, after_run } => named(
            GOAL_CONTINUATION,
            &[goal_id.as_bytes(), after_run.as_bytes()],
        ),
        TriggerIdentity::ScheduledTask {
            task_id,
            scheduled_for_ms,
            ..
        } => named(
            SCHEDULED_TASK,
            &[
                chat.as_bytes(),
                task_id.as_bytes(),
                &scheduled_for_ms.to_be_bytes(),
            ],
        ),
    }
}

/// A message one family member sends with a tool call of its answer.
pub fn family_message(sender_chat: Uuid, response: Uuid, call_id: &str) -> Uuid {
    named(
        FAMILY_MESSAGE,
        &[
            sender_chat.as_bytes(),
            response.as_bytes(),
            call_id.as_bytes(),
        ],
    )
}

/// The fixed id of a host rule that allows `tool`, the same on every host.
pub fn host_rule(tool: &str) -> Uuid {
    named(HOST_RULE, &[tool.as_bytes()])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheduled_display_wrappers_do_not_change_the_input_id() {
        let chat = Uuid::new_v4();
        let identity = |wrappers| TriggerIdentity::ScheduledTask {
            task_id: Uuid::from_u128(0x1234_5678),
            scheduled_for_ms: 1,
            wrappers,
        };
        assert_eq!(
            trigger(chat, &identity(Vec::new())),
            trigger(
                chat,
                &identity(vec![
                    tickloop_harness_protocol::ScheduledInputWrapper::Skill {
                        instruction: "/review main".into()
                    },
                    tickloop_harness_protocol::ScheduledInputWrapper::FinalRun,
                ])
            )
        );
    }

    #[test]
    fn a_source_names_one_id_and_different_sources_name_different_ones() {
        let chat = Uuid::new_v4();
        let response = Uuid::new_v4();
        assert_eq!(
            family_message(chat, response, "a"),
            family_message(chat, response, "a")
        );
        assert_ne!(
            family_message(chat, response, "a"),
            family_message(chat, response, "b")
        );
        assert_eq!(family_message(chat, response, "a").get_version_num(), 5);
    }
}

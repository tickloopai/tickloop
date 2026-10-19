//! Name-based ids of the inputs the Agent queues for itself, and of the Loops its calls
//! register. The same source names the same id on every host, so a repeated delivery finds the
//! input's ledger record and a load finds what a call registered.

use sha2::{Digest, Sha256};
use uuid::Uuid;

const TRIGGER: &str = "tickloop.input.trigger";
const FEEDBACK: &str = "tickloop.input.feedback";
const FAMILY_MESSAGE: &str = "tickloop.input.family-message";
const GOAL: &str = "tickloop.input.goal";
const STARTED_GOAL: &str = "tickloop.loop.started-goal";

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

/// A Trigger's fire at its scheduled time.
pub fn trigger(loop_id: Uuid, scheduled_for_ms: u64) -> Uuid {
    named(
        TRIGGER,
        &[loop_id.as_bytes(), &scheduled_for_ms.to_be_bytes()],
    )
}

/// An @tickloop request, by the delivery that brought it.
pub fn feedback(origin: &str, account: &str, delivery: Uuid) -> Uuid {
    named(
        FEEDBACK,
        &[origin.as_bytes(), account.as_bytes(), delivery.as_bytes()],
    )
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

/// The Loop a StartGoal call of an answer registers.
pub fn started_goal(chat: Uuid, response: Uuid, call_id: &str) -> Uuid {
    named(
        STARTED_GOAL,
        &[chat.as_bytes(), response.as_bytes(), call_id.as_bytes()],
    )
}

/// The goal a StartGoal call registered as the Loop `loop_id`.
pub fn goal(loop_id: Uuid) -> Uuid {
    named(GOAL, &[loop_id.as_bytes()])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_source_names_one_id_and_different_sources_name_different_ones() {
        let loop_id = Uuid::new_v4();
        assert_eq!(goal(loop_id), goal(loop_id));
        assert_ne!(goal(loop_id), goal(Uuid::new_v4()));
        assert_ne!(trigger(loop_id, 1), trigger(loop_id, 2));
        assert_ne!(
            family_message(loop_id, loop_id, "a"),
            family_message(loop_id, loop_id, "b")
        );
        assert_eq!(goal(loop_id).get_version_num(), 5);
    }
}

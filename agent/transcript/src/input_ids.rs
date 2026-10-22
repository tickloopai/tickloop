//! Name-based ids of the inputs the Agent queues for itself, and of the host's own rules. The
//! same source names the same id on every host, so a repeated delivery finds the input's ledger
//! record.

use sha2::{Digest, Sha256};
use uuid::Uuid;

const FEEDBACK: &str = "tickloop.input.feedback";
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

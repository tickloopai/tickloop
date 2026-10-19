use super::*;
use std::collections::VecDeque;
use tickloop_agent_protocol::{
    AgentMessageSource, ChatSnapshot, FamilyMessageRejection, FamilyMessageRejectionCode,
    MessageCause, Receipt, RunId,
};

pub(super) const MAX_MESSAGE_DEPTH: u8 = 8;
pub(super) const MAX_CHAIN_MESSAGES: usize = 64;
pub(super) const MAX_SENDER_MESSAGES_PER_MINUTE: usize = 20;
pub(super) const MAX_PENDING_AGENT_MESSAGES: usize = 128;
const SENDER_RATE_WINDOW_MS: u64 = 60_000;

#[derive(Default, serde::Serialize, serde::Deserialize)]
pub(super) struct FamilyMailbox {
    #[serde(with = "crate::upgrade::map_entries")]
    pub sent: HashMap<(RunId, String), agent_messages::SentMessage>,
    chains: HashMap<Uuid, usize>,
    seen: HashSet<Uuid>,
    senders: HashMap<ChatId, VecDeque<u64>>,
}

impl FamilyMailbox {
    pub fn check(
        &mut self,
        sender: ChatId,
        cause: &MessageCause,
        pending: usize,
        now: u64,
    ) -> Result<(), FamilyMessageRejection> {
        let code = if cause.depth > MAX_MESSAGE_DEPTH {
            Some(FamilyMessageRejectionCode::DepthLimit)
        } else if self
            .chains
            .get(&cause.chain_id)
            .copied()
            .unwrap_or_default()
            >= MAX_CHAIN_MESSAGES
        {
            Some(FamilyMessageRejectionCode::ChainLimit)
        } else if pending >= MAX_PENDING_AGENT_MESSAGES {
            Some(FamilyMessageRejectionCode::InboxFull)
        } else {
            let times = self.senders.entry(sender).or_default();
            while times
                .front()
                .is_some_and(|sent_at| now.saturating_sub(*sent_at) >= SENDER_RATE_WINDOW_MS)
            {
                times.pop_front();
            }
            (times.len() >= MAX_SENDER_MESSAGES_PER_MINUTE)
                .then_some(FamilyMessageRejectionCode::SenderRateLimit)
        };
        if let Some(code) = code {
            return Err(FamilyMessageRejection {
                code,
                message: "family message budget exhausted; do not retry automatically".into(),
            });
        }
        Ok(())
    }

    pub fn check_notification(
        &self,
        cause: &MessageCause,
        pending: usize,
    ) -> Result<(), FamilyMessageRejection> {
        let code = if cause.depth > MAX_MESSAGE_DEPTH {
            Some(FamilyMessageRejectionCode::DepthLimit)
        } else if pending >= MAX_PENDING_AGENT_MESSAGES {
            Some(FamilyMessageRejectionCode::InboxFull)
        } else {
            None
        };
        match code {
            Some(code) => Err(FamilyMessageRejection {
                code,
                message: "Subagent completion delivery budget exhausted".into(),
            }),
            None => Ok(()),
        }
    }

    pub fn synchronize(&mut self, snapshot: &ChatSnapshot) {
        for source in snapshot.sent_agent_messages.values() {
            self.record_source(source);
        }
        for source in snapshot
            .agent_message_origins
            .values()
            .filter_map(|value| serde_json::from_value::<AgentMessageSource>(value.clone()).ok())
        {
            self.record_source(&source);
        }
        for source in snapshot
            .pending_inputs
            .iter()
            .filter_map(|pending| pending.agent_message.as_ref())
        {
            self.record_source(source);
        }
    }

    pub fn record_source(&mut self, source: &AgentMessageSource) {
        if source.is_result || source.call_id.is_none() || !self.seen.insert(source.message_id) {
            return;
        }
        self.record(source.sender_chat_id, &source.cause, source.created_at);
    }

    pub fn record(&mut self, sender: ChatId, cause: &MessageCause, now: u64) {
        *self.chains.entry(cause.chain_id).or_default() += 1;
        let times = self.senders.entry(sender).or_default();
        let index = times
            .iter()
            .position(|timestamp| *timestamp > now)
            .unwrap_or(times.len());
        times.insert(index, now);
    }
}

pub(super) fn current_cause(
    snapshot: &ChatSnapshot,
    bookkeeping: &ChatBookkeeping,
    receipt: &Receipt,
) -> Result<MessageCause, FamilyMessageRejection> {
    let primary_input = snapshot
        .messages
        .iter()
        .enumerate()
        .find_map(|(index, message)| {
            (message.role == Role::User
                && snapshot.message_runs.get(&index) == Some(&receipt.run_id))
            .then_some(index)
        });
    for (index, message) in snapshot.messages.iter().enumerate().rev() {
        let genuine_human = snapshot.prompt_execution_modes.contains_key(&index)
            && !snapshot.inherited_prompts.contains(&index);
        if primary_input
            .is_some_and(|primary| index < primary || (index != primary && !genuine_human))
        {
            continue;
        }
        if message.role != Role::User
            || snapshot
                .message_runs
                .get(&index)
                .and_then(|run| bookkeeping.run_loops.get(run))
                != Some(&receipt.loop_id)
        {
            continue;
        }
        let Some(id) = snapshot.message_ids.get(&index) else {
            if Some(index) == primary_input {
                return Ok(MessageCause {
                    chain_id: receipt.run_id,
                    depth: 0,
                    reply_to: None,
                });
            }
            continue;
        };
        if let Some(source) = snapshot.agent_message_origins.get(id) {
            let source: AgentMessageSource =
                serde_json::from_value(source.clone()).map_err(|_| FamilyMessageRejection {
                    code: FamilyMessageRejectionCode::OriginInactive,
                    message: "Agent message causal source is invalid".into(),
                })?;
            return Ok(MessageCause {
                reply_to: Some(*id),
                ..source.cause
            });
        }
        if snapshot.prompt_execution_modes.contains_key(&index)
            && !snapshot.inherited_prompts.contains(&index)
        {
            return Ok(MessageCause {
                chain_id: snapshot
                    .original_message_ids
                    .get(&index)
                    .copied()
                    .unwrap_or(*id),
                depth: 0,
                reply_to: None,
            });
        }
    }
    Ok(MessageCause {
        chain_id: receipt.run_id,
        depth: 0,
        reply_to: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persisted_sources_rebuild_guard_counts_once_and_sort_sender_timestamps() {
        let sender = Uuid::new_v4();
        let chain = Uuid::new_v4();
        let source = |created_at| AgentMessageSource {
            message_id: Uuid::new_v4(),
            call_id: Some("send".into()),
            created_at,
            sender_chat_id: sender,
            sender_loop_id: Uuid::new_v4(),
            sender_run_id: Uuid::new_v4(),
            sender_relationship: "sibling".into(),
            sender_label: "Evidence".into(),
            sender_handle: "audit".into(),
            relay: None,
            cause: MessageCause {
                chain_id: chain,
                depth: 1,
                reply_to: None,
            },
            is_result: false,
        };
        let mut snapshot = empty_snapshot(
            Uuid::new_v4(),
            PathBuf::from("/tmp"),
            ModelConfig::unselected(),
        );
        for index in (0..20).rev() {
            let source = source(index * 10);
            snapshot
                .agent_message_origins
                .insert(source.message_id, serde_json::to_value(source).unwrap());
        }
        let mut mailbox = FamilyMailbox::default();
        mailbox.synchronize(&snapshot);
        mailbox.synchronize(&snapshot);
        assert_eq!(mailbox.chains[&chain], 20);
        let cause = MessageCause {
            chain_id: chain,
            depth: 1,
            reply_to: None,
        };
        assert!(matches!(
            mailbox.check(sender, &cause, 0, 59999).unwrap_err().code,
            FamilyMessageRejectionCode::SenderRateLimit
        ));
        assert!(mailbox.check(sender, &cause, 0, 60000).is_ok());
    }

    #[test]
    fn guards_bound_depth_chain_sender_window_and_pending_without_consuming_rejected_capacity() {
        let sender = Uuid::new_v4();
        let mut mailbox = FamilyMailbox::default();
        let mut cause = MessageCause {
            chain_id: Uuid::new_v4(),
            depth: 9,
            reply_to: None,
        };
        assert!(matches!(
            mailbox.check(sender, &cause, 0, 0).unwrap_err().code,
            FamilyMessageRejectionCode::DepthLimit
        ));
        cause.depth = 8;
        assert!(matches!(
            mailbox.check(sender, &cause, 128, 0).unwrap_err().code,
            FamilyMessageRejectionCode::InboxFull
        ));
        for _ in 0..20 {
            mailbox.check(sender, &cause, 127, 0).unwrap();
            mailbox.record(sender, &cause, 0);
        }
        assert!(matches!(
            mailbox.check(sender, &cause, 0, 59_999).unwrap_err().code,
            FamilyMessageRejectionCode::SenderRateLimit
        ));
        mailbox.check(sender, &cause, 0, 60_000).unwrap();
        for index in 20..64 {
            let peer = Uuid::from_u128(index as u128 + 100);
            mailbox.check(peer, &cause, 0, 60_000).unwrap();
            mailbox.record(peer, &cause, 60_000);
        }
        assert!(matches!(
            mailbox
                .check(Uuid::new_v4(), &cause, 0, 60_000)
                .unwrap_err()
                .code,
            FamilyMessageRejectionCode::ChainLimit
        ));
        cause.chain_id = Uuid::new_v4();
        mailbox.check(sender, &cause, 0, 60_000).unwrap();
    }
}

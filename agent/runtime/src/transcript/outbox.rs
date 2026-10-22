//! The Lines each recorded Chat has yet to append to its store, and what the store accepted.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tickloop_agent_protocol::ChatId;
use tickloop_agent_transcript::{
    AiTitleEntry, ArtifactTicksEntry, BranchOriginEntry, CustomTitleEntry, DelegationEntry,
    FamilyCompletionsEntry, FamilyInputsEntry, LastPromptEntry, Line, MonitorMembersEntry,
    ProvenanceEntry, SentAgentMessagesEntry, SubagentsEntry, TaskBoardsEntry,
};
use uuid::Uuid;

use super::retained::RetainedFold;
use super::store::{AppendAck, AppendBatch, BatchId, StorePosition, TranscriptError};

/// 照 Claude Code `FLUSH_INTERVAL_MS`：条目先排队，最多每 100ms 批量追加一次。
pub(crate) const FLUSH_INTERVAL: Duration = Duration::from_millis(100);

/// 同类元数据的最后一条；退出时重新追加到 Transcript 末尾，让列表只读尾部就能取到。
#[derive(Clone, Debug, Default)]
pub(crate) struct CachedMetadata {
    pub task_boards: Option<TaskBoardsEntry>,
    pub goal: Option<tickloop_agent_transcript::GoalEntry>,
    pub artifact_ticks: Option<ArtifactTicksEntry>,
    pub monitor_members: Option<MonitorMembersEntry>,
    pub custom_title: Option<CustomTitleEntry>,
    pub ai_title: Option<AiTitleEntry>,
    pub subagents: Option<SubagentsEntry>,
    pub family_completions: Option<FamilyCompletionsEntry>,
    pub family_inputs: Option<FamilyInputsEntry>,
    pub sent_agent_messages: Option<SentAgentMessagesEntry>,
    pub provenance: Option<ProvenanceEntry>,
    pub citation_sources: Option<tickloop_agent_transcript::CitationSourcesEntry>,
    pub last_prompt: Option<LastPromptEntry>,
    pub branch_origin: Option<BranchOriginEntry>,
    pub delegation: Option<DelegationEntry>,
}

impl CachedMetadata {
    fn remember(&mut self, line: &Line) {
        match line {
            Line::ArtifactTicks(entry) => self.artifact_ticks = Some(entry.clone()),
            Line::MonitorMembers(entry) => self.monitor_members = Some(entry.clone()),
            Line::CustomTitle(entry) => self.custom_title = Some(entry.clone()),
            Line::AiTitle(entry) => self.ai_title = Some(entry.clone()),
            Line::Subagents(entry) => self.subagents = Some(entry.clone()),
            Line::TaskBoards(entry) => self.task_boards = Some(entry.clone()),
            Line::Goal(entry) => self.goal = Some(entry.clone()),
            Line::FamilyCompletions(entry) => self.family_completions = Some(entry.clone()),
            Line::FamilyInputs(entry) => self.family_inputs = Some(entry.clone()),
            Line::SentAgentMessages(entry) => self.sent_agent_messages = Some(entry.clone()),
            Line::Provenance(entry) => self.provenance = Some(entry.clone()),
            Line::CitationSources(entry) => self.citation_sources = Some(entry.clone()),
            Line::LastPrompt(entry) => self.last_prompt = Some(entry.clone()),
            Line::BranchOrigin(entry) => self.branch_origin = Some(entry.clone()),
            Line::Delegation(entry) => self.delegation = Some(entry.clone()),
            _ => {}
        }
    }

    fn lines(&self) -> Vec<Line> {
        self.custom_title
            .clone()
            .map(Line::CustomTitle)
            .into_iter()
            .chain(self.ai_title.clone().map(Line::AiTitle))
            .chain(self.subagents.clone().map(Line::Subagents))
            .chain(self.task_boards.clone().map(Line::TaskBoards))
            .chain(self.goal.clone().map(Line::Goal))
            .chain(self.family_completions.clone().map(Line::FamilyCompletions))
            .chain(self.family_inputs.clone().map(Line::FamilyInputs))
            .chain(
                self.sent_agent_messages
                    .clone()
                    .map(Line::SentAgentMessages),
            )
            .chain(self.provenance.clone().map(Line::Provenance))
            .chain(self.citation_sources.clone().map(Line::CitationSources))
            .chain(self.last_prompt.clone().map(Line::LastPrompt))
            .chain(self.branch_origin.clone().map(Line::BranchOrigin))
            .chain(self.delegation.clone().map(Line::Delegation))
            .chain(self.artifact_ticks.clone().map(Line::ArtifactTicks))
            .chain(self.monitor_members.clone().map(Line::MonitorMembers))
            .collect()
    }
}

struct ChatOutbox {
    /// The Transcript has begun: its first user or assistant entry was queued, or it was loaded.
    materialized: bool,
    /// The store keeps something of this Chat: it was loaded, or an append succeeded.
    stored: bool,
    /// Lines queued before the first user or assistant entry, while nothing is stored yet.
    buffered: Vec<Line>,
    queued: Vec<Line>,
    /// The batch sent last and not yet acknowledged. A storage error keeps it, and it is retried
    /// as is before anything queued after it.
    pending: Option<AppendBatch>,
    written_uuids: HashSet<Uuid>,
    metadata: CachedMetadata,
    /// The retained state of everything queued or loaded, which snapshots are made from.
    retained: RetainedFold,
    /// The position the store acknowledged last, or gave at registration or load.
    position: StorePosition,
    events_through: u64,
    /// The display node the conversation ended at when the queued Lines were formed.
    display_leaf: Option<Uuid>,
    /// The last failure, until an append succeeds.
    write_failure: Option<String>,
    /// The store refused this holder for good: nothing more is written.
    refusal: Option<TranscriptError>,
}

impl ChatOutbox {
    fn refuse(&mut self, error: TranscriptError) {
        self.pending = None;
        self.buffered.clear();
        self.queued.clear();
        self.write_failure = Some(error.to_string());
        self.refusal = Some(error);
    }

    fn new(position: StorePosition) -> Self {
        Self {
            materialized: false,
            stored: false,
            buffered: Vec::new(),
            queued: Vec::new(),
            pending: None,
            written_uuids: HashSet::new(),
            metadata: CachedMetadata::default(),
            retained: RetainedFold::default(),
            position,
            events_through: 0,
            display_leaf: None,
            write_failure: None,
            refusal: None,
        }
    }
}

/// What one append settled for its Chat.
pub(crate) enum Settled {
    Committed(AppendAck),
    /// The batch stays pending for a retry.
    Failed(String),
    Refused(TranscriptError),
}

/// Lines queued per Chat until a flush appends them. A failed batch stays pending with its
/// identity, so a retry neither duplicates nor breaks the chain; a refused Chat writes nothing
/// more.
#[derive(Default)]
pub(crate) struct TranscriptOutbox {
    chats: Mutex<HashMap<ChatId, ChatOutbox>>,
    /// A store fenced one of the Chats: a newer holder owns this Harness's Chats, so none of
    /// them writes anything more.
    fenced: AtomicBool,
}

impl TranscriptOutbox {
    pub(crate) fn register(&self, chat: ChatId, position: StorePosition) {
        self.insert(chat, ChatOutbox::new(position));
    }

    fn insert(&self, chat: ChatId, mut outbox: ChatOutbox) {
        if self.is_fenced() {
            outbox.refuse(TranscriptError::Fenced);
        }
        self.chats.lock().unwrap().insert(chat, outbox);
    }

    /// A Chat loaded from its store: later entries are appended directly, and uuids already
    /// stored are not written again.
    pub(crate) fn adopt(
        &self,
        chat: ChatId,
        position: StorePosition,
        uuids: HashSet<Uuid>,
        metadata: CachedMetadata,
        retained: RetainedFold,
    ) {
        let mut outbox = ChatOutbox::new(position);
        outbox.materialized = true;
        outbox.stored = true;
        outbox.written_uuids = uuids;
        outbox.metadata = metadata;
        outbox.retained = retained;
        self.insert(chat, outbox);
    }

    /// The store already holds these chain entries of the Chat, the branch `retained` was
    /// folded from: they are not written again, and later snapshots keep what that branch needs.
    pub(crate) fn remember_written(
        &self,
        chat: ChatId,
        uuids: HashSet<Uuid>,
        retained: RetainedFold,
    ) {
        if let Some(outbox) = self.chats.lock().unwrap().get_mut(&chat) {
            outbox.written_uuids.extend(uuids);
            outbox.retained = retained;
            if let Some(sources) = &outbox.metadata.citation_sources {
                outbox
                    .retained
                    .apply(&Line::CitationSources(sources.clone()));
            }
        }
    }

    /// The display node the conversation ends at as of the Lines queued next.
    pub(crate) fn set_display_leaf(&self, chat: ChatId, leaf: Option<Uuid>) {
        if let Some(outbox) = self.chats.lock().unwrap().get_mut(&chat) {
            outbox.display_leaf = leaf;
        }
    }

    pub(crate) fn enqueue(&self, chat: ChatId, lines: Vec<Line>, events_through: Option<u64>) {
        let mut chats = self.chats.lock().unwrap();
        let Some(outbox) = chats.get_mut(&chat) else {
            return;
        };
        if outbox.refusal.is_some() {
            return;
        }
        if let Some(events_through) = events_through {
            outbox.events_through = outbox.events_through.max(events_through);
        }
        if lines.is_empty() {
            return;
        }
        let mut accepted = Vec::with_capacity(lines.len());
        for line in lines {
            if let Some(entry) = line.chain_entry()
                && !outbox.written_uuids.insert(entry.uuid)
            {
                continue;
            }
            outbox.metadata.remember(&line);
            outbox.retained.apply(&line);
            accepted.push(line);
        }
        // A new start of the chain, or enough retained Lines since the last snapshot, are folded
        // into one in the same group.
        if accepted.iter().any(|line| {
            line.chain_entry()
                .is_some_and(tickloop_agent_transcript::ChainEntry::restarts_chain)
        }) || outbox.retained.is_due()
        {
            let snapshot = outbox.retained.snapshot(chat);
            outbox.retained.apply(&snapshot);
            accepted.push(snapshot);
        }
        if !outbox.materialized {
            if !accepted.iter().any(|line| {
                line.is_conversation_message()
                    || matches!(line, Line::System(entry) if entry.loop_record.as_ref().is_some_and(|record| !record.event_sources.is_empty()))
                    || matches!(line, Line::ArtifactTicks(entry) if entry.cwd.is_some() && (!entry.artifact_ticks.is_empty() || !entry.removed_artifact_ticks.is_empty()))
                    || matches!(line, Line::MonitorMembers(entry) if entry.materializes_chat())
                    || matches!(line, Line::Goal(entry) if entry.cwd.is_some() && entry.goal.is_some())
            }) {
                outbox.buffered.extend(accepted);
                return;
            }
            outbox.materialized = true;
            accepted.splice(0..0, std::mem::take(&mut outbox.buffered));
        }
        outbox.queued.extend(accepted);
    }

    /// Queues the cached metadata again at the end, after `title` replaced the custom title when
    /// given. Only a Chat the store already keeps.
    pub(crate) fn reappend_metadata(&self, chat: ChatId, title: Option<String>) -> bool {
        let mut chats = self.chats.lock().unwrap();
        let Some(outbox) = chats
            .get_mut(&chat)
            .filter(|outbox| outbox.stored && outbox.refusal.is_none())
        else {
            return false;
        };
        if let Some(title) = title
            && let Some(current) = &mut outbox.metadata.custom_title
        {
            current.custom_title = title;
        }
        let lines = outbox.metadata.lines();
        let queued = !lines.is_empty();
        for line in &lines {
            outbox.retained.apply(line);
        }
        outbox.queued.extend(lines);
        queued
    }

    /// A newer holder owns this Harness's Chats: none of them writes anything more.
    pub(crate) fn fence(&self) {
        self.fence_chats(&mut self.chats.lock().unwrap());
    }

    fn fence_chats(&self, chats: &mut HashMap<ChatId, ChatOutbox>) {
        self.fenced.store(true, Ordering::SeqCst);
        for outbox in chats.values_mut() {
            outbox.refuse(TranscriptError::Fenced);
        }
    }

    pub(crate) fn is_fenced(&self) -> bool {
        self.fenced.load(Ordering::SeqCst)
    }

    pub(crate) fn has_queued(&self, chat: ChatId) -> bool {
        self.chats
            .lock()
            .unwrap()
            .get(&chat)
            .is_some_and(|outbox| !outbox.queued.is_empty())
    }

    pub(crate) fn chats(&self) -> Vec<ChatId> {
        self.chats.lock().unwrap().keys().copied().collect()
    }

    /// The Chat is stored from now on, before a conversation message would store it: what it
    /// buffered is queued first.
    pub(crate) fn materialize(&self, chat: ChatId) {
        if let Some(outbox) = self.chats.lock().unwrap().get_mut(&chat)
            && !outbox.materialized
            && outbox.refusal.is_none()
        {
            outbox.materialized = true;
            let buffered = std::mem::take(&mut outbox.buffered);
            outbox.queued.splice(0..0, buffered);
        }
    }

    pub(crate) fn is_materialized(&self, chat: ChatId) -> bool {
        self.chats
            .lock()
            .unwrap()
            .get(&chat)
            .is_some_and(|outbox| outbox.materialized)
    }

    pub(crate) fn is_stored(&self, chat: ChatId) -> bool {
        self.chats
            .lock()
            .unwrap()
            .get(&chat)
            .is_some_and(|outbox| outbox.stored)
    }

    /// The batch to append next: the pending one, else everything queued. `durable` asks for a
    /// batch even when nothing is queued, to make earlier appends durable.
    pub(crate) fn next_batch(&self, chat: ChatId, durable: bool) -> Option<AppendBatch> {
        let mut chats = self.chats.lock().unwrap();
        let outbox = chats.get_mut(&chat)?;
        if outbox.refusal.is_some() {
            return None;
        }
        if let Some(pending) = &outbox.pending {
            return Some(pending.clone());
        }
        if outbox.queued.is_empty() && !durable {
            return None;
        }
        let batch = AppendBatch {
            id: BatchId::new(),
            expected: Some(outbox.position),
            lines: std::mem::take(&mut outbox.queued),
            events_through: outbox.events_through,
            display_leaf: outbox.display_leaf,
            durable,
        };
        outbox.pending = Some(batch.clone());
        Some(batch)
    }

    pub(crate) fn settle(
        &self,
        chat: ChatId,
        batch: BatchId,
        result: Result<AppendAck, TranscriptError>,
    ) -> Settled {
        let mut chats = self.chats.lock().unwrap();
        let Some(outbox) = chats.get_mut(&chat) else {
            return Settled::Failed("the Chat is no longer recorded".into());
        };
        if outbox
            .pending
            .as_ref()
            .is_none_or(|pending| pending.id != batch)
        {
            return Settled::Failed("the batch is no longer pending".into());
        }
        match result {
            Ok(ack) => {
                outbox.pending = None;
                outbox.position = ack.position;
                outbox.stored = true;
                outbox.write_failure = None;
                Settled::Committed(ack)
            }
            Err(TranscriptError::Fenced) => {
                self.fence_chats(&mut chats);
                Settled::Refused(TranscriptError::Fenced)
            }
            Err(error @ TranscriptError::Conflict(_)) => {
                outbox.refuse(error.clone());
                Settled::Refused(error)
            }
            Err(error) => {
                let message = error.to_string();
                outbox.write_failure = Some(message.clone());
                Settled::Failed(message)
            }
        }
    }

    /// Drops what the Chat has queued and not committed, a failed batch included, and writes
    /// nothing more of it: what this holder could not record of it is left unrecorded.
    pub(crate) fn withhold(&self, chat: ChatId, reason: &str) {
        if let Some(outbox) = self.chats.lock().unwrap().get_mut(&chat) {
            outbox.refuse(TranscriptError::Storage(reason.into()));
        }
    }

    pub(crate) fn refusal(&self, chat: ChatId) -> Option<TranscriptError> {
        self.chats
            .lock()
            .unwrap()
            .get(&chat)
            .and_then(|outbox| outbox.refusal.clone())
    }

    pub(crate) fn position(&self, chat: ChatId) -> Option<StorePosition> {
        self.chats
            .lock()
            .unwrap()
            .get(&chat)
            .map(|outbox| outbox.position)
    }

    /// 仍未解决的写入失败：某个 Chat 最近一次追加失败且之后没有成功追加过，或存储已拒绝它。
    pub(crate) fn unresolved_error(&self) -> Option<String> {
        self.chats
            .lock()
            .unwrap()
            .values()
            .find_map(|outbox| outbox.write_failure.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tickloop_agent_protocol::{Content, Message, Role};

    fn chain(uuid: Uuid, role: Role) -> Line {
        let line = serde_json::json!({
            "type": "user",
            "uuid": uuid,
            "parentUuid": null,
            "chatId": Uuid::nil(),
            "timestamp": "2026-01-01T00:00:00.000Z",
            "version": "test",
            "message": Message { role, content: vec![Content::Text { text: "x".into() }] },
            "humanFamilyOrigin": null,
        });
        serde_json::from_value(line).unwrap()
    }

    fn title() -> Line {
        Line::CustomTitle(CustomTitleEntry {
            chat_id: Uuid::nil(),
            custom_title: "title".into(),
        })
    }

    #[test]
    fn the_first_message_starts_the_transcript_and_duplicates_are_dropped() {
        let outbox = TranscriptOutbox::default();
        let chat = Uuid::new_v4();
        outbox.register(chat, StorePosition(3));
        outbox.enqueue(chat, vec![title()], None);
        assert!(outbox.next_batch(chat, false).is_none());

        let uuid = Uuid::new_v4();
        outbox.enqueue(chat, vec![chain(uuid, Role::User)], Some(9));
        outbox.enqueue(chat, vec![chain(uuid, Role::User)], None);
        let leaf = Uuid::new_v4();
        outbox.set_display_leaf(chat, Some(leaf));
        let batch = outbox.next_batch(chat, false).unwrap();
        assert_eq!(batch.display_leaf, Some(leaf));
        assert_eq!(batch.lines.len(), 2);
        assert!(matches!(batch.lines[0], Line::CustomTitle(_)));
        assert_eq!(batch.expected, Some(StorePosition(3)));
        assert_eq!(batch.events_through, 9);
    }

    #[test]
    fn a_failed_batch_is_retried_as_is_and_a_refusal_stops_everything() {
        let outbox = TranscriptOutbox::default();
        let chat = Uuid::new_v4();
        outbox.register(chat, StorePosition(0));
        outbox.enqueue(chat, vec![chain(Uuid::new_v4(), Role::User)], None);
        let first = outbox.next_batch(chat, false).unwrap();
        assert!(matches!(
            outbox.settle(chat, first.id, Err(TranscriptError::Storage("down".into()))),
            Settled::Failed(_)
        ));
        outbox.enqueue(chat, vec![chain(Uuid::new_v4(), Role::Assistant)], None);
        let retried = outbox.next_batch(chat, false).unwrap();
        assert_eq!(retried.id, first.id);
        assert_eq!(retried.lines.len(), 1);
        assert_eq!(outbox.unresolved_error().as_deref(), Some("down"));
        outbox.settle(
            chat,
            first.id,
            Ok(AppendAck {
                position: StorePosition(10),
            }),
        );
        let next = outbox.next_batch(chat, false).unwrap();
        assert_eq!(next.expected, Some(StorePosition(10)));
        assert_eq!(outbox.unresolved_error(), None);

        outbox.settle(chat, next.id, Err(TranscriptError::Fenced));
        outbox.enqueue(chat, vec![chain(Uuid::new_v4(), Role::User)], None);
        assert!(outbox.next_batch(chat, true).is_none());
        assert_eq!(outbox.refusal(chat), Some(TranscriptError::Fenced));
    }

    /// A result's chain entry and the lifecycle records of the same change, such as a resumed
    /// answer, are enqueued as one group: they commit together or not at all.
    #[test]
    fn a_group_enqueued_together_is_never_split_across_batches() {
        let outbox = TranscriptOutbox::default();
        let chat = Uuid::new_v4();
        outbox.register(chat, StorePosition(0));
        outbox.enqueue(chat, vec![chain(Uuid::new_v4(), Role::User), title()], None);
        let first = outbox.next_batch(chat, false).unwrap();
        assert_eq!(first.lines.len(), 2);
        let group = vec![chain(Uuid::new_v4(), Role::User), title(), title()];
        outbox.enqueue(chat, group.clone(), None);
        assert_eq!(outbox.next_batch(chat, false).unwrap().id, first.id);
        outbox.settle(
            chat,
            first.id,
            Ok(AppendAck {
                position: StorePosition(2),
            }),
        );
        let second = outbox.next_batch(chat, false).unwrap();
        assert_eq!(second.lines.len(), group.len());
    }
}

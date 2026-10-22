//! Loading from a retained snapshot gives what loading every retained Line gives: random
//! sequences of every kind of Line, with snapshots written where the recorder writes them and at
//! random places besides, load the same conversation, metadata, file history, content
//! replacements, lifecycle records and display projection as the same Lines without snapshots.

use std::collections::{BTreeMap, HashSet};

use serde_json::{Value, json};
use tickloop_agent_transcript::display::project;
use tickloop_agent_transcript::{
    InputState, InterruptedBy, Line, ResponseEnding, ResponseState, WaitState,
};
use uuid::Uuid;

use super::RetainedFold;
use crate::lifecycle::Lifecycle;
use crate::transcript::reader::assemble;
use crate::transcript::{StorePosition, StoredTranscript, retained_before};

const SEQUENCES: u64 = 40;
const LINES_PER_SEQUENCE: usize = 600;
const TIMESTAMP: &str = "2026-01-01T00:00:00.000Z";

fn parse(value: Value) -> Line {
    serde_json::from_value(value).expect("a generated Line parses")
}

/// What a generated Transcript refers to so far.
struct Generator {
    random: fastrand::Rng,
    chat: Uuid,
    counter: u64,
    parent: Option<Uuid>,
    entries: Vec<Uuid>,
    /// The chain entries since the latest boundary.
    chain_entries: Vec<Uuid>,
    /// The chain entries the latest boundary compacted; a message it keeps carries one's id as
    /// its original id.
    compacted_entries: Vec<Uuid>,
    /// The nodes answers without messages hang under.
    answered_without_messages: Vec<Uuid>,
    tool_uses: Vec<String>,
    responses: Vec<Uuid>,
    /// The answers that have not ended: only they get more messages or records.
    live_responses: Vec<Uuid>,
    /// The answers with messages on the chain since the latest boundary.
    chain_responses: Vec<Uuid>,
    /// The answers with messages on the chain the latest boundary compacted, which the
    /// messages it keeps belong to.
    compacted_responses: Vec<Uuid>,
    inputs: Vec<Uuid>,
    /// Inputs not settled yet, with their operation: only they get more records.
    open_inputs: Vec<(Uuid, &'static str)>,
    waits: Vec<Uuid>,
    /// Waits still open: only they get more records.
    open_waits: Vec<Uuid>,
}

impl Generator {
    fn new(seed: u64) -> Self {
        Self {
            random: fastrand::Rng::with_seed(seed),
            chat: Uuid::new_v4(),
            counter: 0,
            parent: None,
            entries: Vec::new(),
            chain_entries: Vec::new(),
            compacted_entries: Vec::new(),
            answered_without_messages: Vec::new(),
            tool_uses: Vec::new(),
            responses: Vec::new(),
            live_responses: Vec::new(),
            chain_responses: Vec::new(),
            compacted_responses: Vec::new(),
            inputs: Vec::new(),
            open_inputs: Vec::new(),
            waits: Vec::new(),
            open_waits: Vec::new(),
        }
    }

    fn next(&mut self) -> u64 {
        self.counter += 1;
        self.counter
    }

    fn pick<T: Clone>(&mut self, items: &[T]) -> Option<T> {
        (!items.is_empty()).then(|| items[self.random.usize(..items.len())].clone())
    }

    /// A message of a live answer, or, as compaction rewrites it right after the boundary, of
    /// any answer.
    fn chain_entry(&mut self, is_kept_by_compaction: bool) -> Line {
        let uuid = Uuid::new_v4();
        let role = if self.random.bool() {
            "user"
        } else {
            "assistant"
        };
        let mut content = vec![json!({"type": "text", "text": format!("text {}", self.next())})];
        if role == "assistant" && self.random.u8(..4) == 0 {
            let id = format!("call-{}", self.next());
            self.tool_uses.push(id.clone());
            content.push(json!({"type": "tool_use", "id": id, "name": "Read", "input": {}}));
        }
        if role == "user"
            && let Some(id) = self.pick(&self.tool_uses.clone())
            && self.random.bool()
        {
            content = vec![
                json!({"type": "tool_result", "tool_use_id": id, "content": "result", "is_error": false}),
            ];
        }
        let answers = match is_kept_by_compaction {
            true => self.compacted_responses.clone(),
            false => self.live_responses.clone(),
        };
        let response = self.pick(&answers).filter(|_| self.random.bool());
        self.chain_responses.extend(response);
        let input = self
            .pick(&self.inputs.clone())
            .filter(|_| self.random.u8(..4) == 0);
        // Compaction often keeps the prompt an answer without messages hangs under.
        let answered = self
            .compacted_entries
            .iter()
            .filter(|entry| self.answered_without_messages.contains(entry))
            .copied()
            .collect::<Vec<_>>();
        let original = match is_kept_by_compaction {
            true if !answered.is_empty() && self.random.bool() => self.pick(&answered),
            true => self.pick(&self.compacted_entries.clone()),
            false => None,
        };
        let line = parse(json!({
            "type": role,
            "uuid": uuid,
            "originalMessageId": original,
            "parentUuid": self.parent,
            "chatId": self.chat,
            "timestamp": TIMESTAMP,
            "version": "test",
            "message": {"role": role, "content": content},
            "humanFamilyOrigin": null,
            "responseId": response,
            "inputId": input,
        }));
        self.parent = Some(uuid);
        self.entries.push(uuid);
        self.chain_entries.push(uuid);
        line
    }

    /// A compact boundary the chain starts again at; calls before it get no more results.
    fn boundary(&mut self) -> Line {
        self.tool_uses.clear();
        self.compacted_responses = std::mem::take(&mut self.chain_responses);
        self.compacted_entries = std::mem::take(&mut self.chain_entries);
        let uuid = Uuid::new_v4();
        let line = parse(json!({
            "type": "system",
            "uuid": uuid,
            "parentUuid": null,
            "logicalParentUuid": self.parent,
            "chatId": self.chat,
            "timestamp": TIMESTAMP,
            "version": "test",
            "message": {"role": "system", "content": [{"type": "text", "text": "summary"}]},
            "humanFamilyOrigin": null,
            "subtype": "compact_boundary",
            "compactMetadata": {"trigger": "auto", "preTokens": 1},
        }));
        self.parent = Some(uuid);
        self.entries.push(uuid);
        line
    }

    fn response_state(&mut self) -> Value {
        match self.random.u8(..6) {
            0 => json!({"kind": "started"}),
            1 => json!({"kind": "suspended"}),
            2 => json!({"kind": "resumed"}),
            3 => json!({"kind": "ended", "ending": {"kind": "completed"}}),
            4 => json!({"kind": "ended", "ending": {"kind": "interrupted", "by": "host"}}),
            _ => json!({"kind": "ended", "ending": {"kind": "interrupted", "by": "handover"}}),
        }
    }

    fn retained(&mut self) -> Line {
        let chat = self.chat;
        let n = self.next();
        match self.random.u8(..17) {
            0 => parse(
                json!({"type": "custom-title", "chatId": chat, "customTitle": format!("title {n}")}),
            ),
            1 => parse(json!({"type": "ai-title", "chatId": chat, "aiTitle": format!("ai {n}")})),
            2 => parse(
                json!({"type": "last-prompt", "chatId": chat, "lastPrompt": format!("prompt {n}")}),
            ),
            3 => parse(
                json!({"type": "branch-origin", "chatId": chat, "forkedFrom": {"chatId": chat, "messageUuid": Uuid::new_v4()}}),
            ),
            4 => parse(
                json!({"type": "artifact-ticks", "chatId": chat, "artifactTicks": [], "removedArtifactTicks": [format!("removed {n}")]}),
            ),
            5 => parse(json!({"type": "subagents", "chatId": chat, "subagents": []})),
            6 => parse(json!({"type": "family-inputs", "chatId": chat, "inputs": {}})),
            7 => parse(json!({"type": "sent-agent-messages", "chatId": chat, "messages": {}})),
            8 => {
                let message = self
                    .pick(&self.entries.clone())
                    .unwrap_or_else(Uuid::new_v4);
                parse(
                    json!({"type": "file-history-snapshot", "messageId": message, "snapshot": {"n": n}, "isSnapshotUpdate": self.random.bool()}),
                )
            }
            9 => {
                let id = self
                    .pick(&self.tool_uses.clone())
                    .unwrap_or_else(|| format!("call-{n}"));
                parse(
                    json!({"type": "content-replacement", "chatId": chat, "replacements": [{"kind": "tool-result", "toolUseId": id, "replacement": format!("replaced {n}")}]}),
                )
            }
            10..=11 => {
                let id = match self
                    .pick(&self.live_responses.clone())
                    .filter(|_| self.random.bool())
                {
                    Some(id) => id,
                    None => {
                        let id = Uuid::new_v4();
                        self.responses.push(id);
                        self.live_responses.push(id);
                        id
                    }
                };
                let input = self.pick(&self.inputs.clone());
                let state = self.response_state();
                let ended = state["kind"] == "ended";
                if ended {
                    self.live_responses.retain(|live| *live != id);
                }
                // An answer that ends without messages hangs under the node it answered; it
                // starts and ends with no message between.
                let without_messages = ended
                    && !self.chain_responses.contains(&id)
                    && !self.compacted_responses.contains(&id)
                    && self.random.u8(..2) == 0;
                let parent = match without_messages {
                    true => self.pick(&self.chain_entries.clone()).or(input),
                    false => input,
                };
                if without_messages {
                    self.answered_without_messages.extend(parent);
                }
                parse(
                    json!({"type": "response", "responseId": id, "inputId": input, "attempt": 1, "runId": id, "loopId": chat, "displayParent": parent, "timestamp": TIMESTAMP, "state": state, "withoutMessages": without_messages}),
                )
            }
            12..=13 => {
                let (id, op) = match self
                    .pick(&self.open_inputs.clone())
                    .filter(|_| self.random.bool())
                {
                    Some(open) => open,
                    None => {
                        let id = Uuid::new_v4();
                        let op = ["continue", "trigger", "answer", "agentMessage", "feedback"]
                            [self.random.usize(..5)];
                        self.inputs.push(id);
                        self.open_inputs.push((id, op));
                        (id, op)
                    }
                };
                let state = match self.random.u8(..3) {
                    0 => json!({"kind": "accepted"}),
                    1 => json!({"kind": "started"}),
                    _ => {
                        self.open_inputs.retain(|(open, _)| *open != id);
                        json!({"kind": "settled", "outcome": {"kind": "completed", "usage": {}}})
                    }
                };
                parse(
                    json!({"type": "input", "inputId": id, "op": op, "digest": "d", "loopId": chat, "runId": id, "timestamp": TIMESTAMP, "state": state}),
                )
            }
            14 => {
                let id = match self
                    .pick(&self.open_waits.clone())
                    .filter(|_| self.random.bool())
                {
                    Some(id) => id,
                    None => {
                        let id = Uuid::new_v4();
                        self.waits.push(id);
                        self.open_waits.push(id);
                        id
                    }
                };
                let state = match self.random.u8(..3) {
                    0 => json!({"kind": "open"}),
                    1 => json!({"kind": "appended"}),
                    _ => json!({"kind": "abandoned", "reason": "hostLost"}),
                };
                if state["kind"] != "open" {
                    self.open_waits.retain(|open| *open != id);
                }
                parse(
                    json!({"type": "wait", "waitId": id, "chatId": chat, "loopId": chat, "runId": id, "responseId": id,
                    "kind": {"kind": "question", "request": {"id": id, "chat_id": chat, "loop_id": chat, "run_id": id, "call_id": "q", "questions": []}, "operationId": id},
                    "resume": {"kind": "inBatch", "assistantMessage": id, "callId": "q"}, "timestamp": TIMESTAMP, "state": state}),
                )
            }
            _ => {
                let kind = ["future-a", "future-b"][self.random.usize(..2)];
                parse(json!({"type": kind, "n": n}))
            }
        }
    }

    /// A Transcript with snapshots where the recorder writes them and at random places.
    fn transcript(&mut self) -> Vec<Line> {
        let mut fold = RetainedFold::default();
        let mut lines = Vec::new();
        for _ in 0..LINES_PER_SEQUENCE {
            let group = match self.random.u8(..100) {
                // Compaction writes the boundary and the messages it keeps in one group.
                0 => std::iter::once(self.boundary())
                    .chain((0..self.random.usize(1..4)).map(|_| self.chain_entry(true)))
                    .collect::<Vec<_>>(),
                1..=34 => vec![self.chain_entry(false)],
                _ => vec![self.retained()],
            };
            let restarts = group.iter().any(|line| {
                line.chain_entry()
                    .is_some_and(|entry| entry.restarts_chain())
            });
            for line in group {
                fold.apply(&line);
                lines.push(line);
            }
            if restarts || fold.is_due() || self.random.u8(..30) == 0 {
                let snapshot = fold.snapshot(self.chat);
                fold.apply(&snapshot);
                lines.push(snapshot);
            }
        }
        lines
    }
}

/// The Lines a load from the latest boundary reads, as a store returns them.
fn from_latest_boundary(lines: &[Line]) -> StoredTranscript {
    let boundary = lines
        .iter()
        .rposition(|line| {
            line.chain_entry()
                .is_some_and(|entry| entry.restarts_chain())
        })
        .unwrap_or(0);
    StoredTranscript {
        retained: retained_before(lines, boundary),
        lines: lines[boundary..].to_vec(),
        position: StorePosition(0),
        warnings: Vec::new(),
    }
}

fn without_snapshots(lines: &[Line]) -> Vec<Line> {
    lines
        .iter()
        .filter(|line| !matches!(line, Line::RetainedSnapshot(_)))
        .cloned()
        .collect()
}

/// A record a load of the chain needs: the conversation shows it, or the Chat may go on with
/// it.
fn needed_response(
    lifecycle: &Lifecycle,
    id: Uuid,
    chain_responses: &HashSet<Uuid>,
    shown: &HashSet<Uuid>,
) -> bool {
    let Some(response) = lifecycle.response(id) else {
        return false;
    };
    let input_is_open = response.input_id.is_some_and(|input| {
        lifecycle
            .input(input)
            .is_some_and(|input| !matches!(input.state, InputState::Settled { .. }))
    });
    let lost_host = matches!(
        response.state,
        ResponseState::Ended {
            ending: ResponseEnding::Interrupted {
                by: InterruptedBy::Host | InterruptedBy::Handover
            }
        }
    );
    let shows_without_messages = response.without_messages
        && response
            .display_parent
            .is_some_and(|parent| shown.contains(&parent));
    chain_responses.contains(&id)
        || !response.state.is_ended()
        || (lost_host && input_is_open)
        || shows_without_messages
}

#[test]
fn a_load_from_a_snapshot_equals_a_load_of_every_retained_line() {
    for seed in 0..SEQUENCES {
        let mut generator = Generator::new(seed);
        let lines = generator.transcript();
        assert!(
            lines
                .iter()
                .any(|line| matches!(line, Line::RetainedSnapshot(_)))
        );
        let (full, full_reaches_root) =
            assemble(&from_latest_boundary(&without_snapshots(&lines)), None);
        let (folded, folded_reaches_root) = assemble(&from_latest_boundary(&lines), None);
        assert_eq!(
            folded_reaches_root, full_reaches_root,
            "seed {seed}: no Full fallback"
        );
        let (Some(full), Some(folded)) = (full, folded) else {
            continue;
        };
        assert_eq!(
            serde_json::to_string(&folded.chain).unwrap(),
            serde_json::to_string(&full.chain).unwrap(),
            "seed {seed}: chain"
        );
        assert_eq!(
            format!("{:?}", folded.metadata),
            format!("{:?}", full.metadata),
            "seed {seed}: metadata"
        );
        let chain_entries = full
            .chain
            .iter()
            .map(|entry| entry.uuid)
            .collect::<HashSet<_>>();
        let file_history = |loaded: &super::super::LoadedTranscript| {
            loaded
                .file_history_snapshots
                .iter()
                .filter(|(message, _)| chain_entries.contains(message))
                .map(|(message, entry)| (*message, serde_json::to_string(entry).unwrap()))
                .collect::<BTreeMap<_, _>>()
        };
        assert_eq!(
            file_history(&folded),
            file_history(&full),
            "seed {seed}: file history"
        );
        let tool_results = full
            .chain
            .iter()
            .flat_map(|entry| &entry.message.content)
            .filter_map(|content| match content {
                tickloop_agent_protocol::Content::ToolResult { tool_use_id, .. } => {
                    Some(tool_use_id.clone())
                }
                _ => None,
            })
            .collect::<HashSet<_>>();
        let replacements = |loaded: &super::super::LoadedTranscript| {
            loaded
                .content_replacements
                .iter()
                .flat_map(|entry| &entry.replacements)
                .filter(|record| tool_results.contains(record.tool_use_id()))
                .map(|record| {
                    (
                        record.tool_use_id().to_owned(),
                        serde_json::to_string(record).unwrap(),
                    )
                })
                .collect::<BTreeMap<_, _>>()
        };
        assert_eq!(
            replacements(&folded),
            replacements(&full),
            "seed {seed}: replacements"
        );

        let full_lifecycle = Lifecycle::restore(&full.lifecycle, true);
        let folded_lifecycle = Lifecycle::restore(&folded.lifecycle, true);
        let chain_responses = full
            .chain
            .iter()
            .filter_map(|entry| entry.response_id)
            .collect::<HashSet<_>>();
        // The display nodes of the loaded conversation: its entries under their original ids too,
        // and its answers.
        let shown = chain_entries
            .iter()
            .copied()
            .chain(
                full.chain
                    .iter()
                    .filter_map(|entry| entry.original_message_id),
            )
            .chain(chain_responses.iter().copied())
            .collect::<HashSet<_>>();
        for id in &generator.responses {
            if needed_response(&full_lifecycle, *id, &chain_responses, &shown) {
                assert_eq!(
                    folded_lifecycle.response(*id),
                    full_lifecycle.response(*id),
                    "seed {seed}: response {id}"
                );
            }
        }
        for id in &generator.inputs {
            if let Some(input) = full_lifecycle.input(*id)
                && !matches!(input.state, InputState::Settled { .. })
            {
                assert_eq!(
                    folded_lifecycle.input(*id),
                    Some(input),
                    "seed {seed}: input {id}"
                );
            }
        }
        for id in &generator.waits {
            if let Some(wait) = full_lifecycle.wait(*id)
                && matches!(wait.state, WaitState::Open | WaitState::Resolved { .. })
            {
                assert_eq!(
                    serde_json::to_string(&folded_lifecycle.wait(*id)).unwrap(),
                    serde_json::to_string(&Some(wait)).unwrap(),
                    "seed {seed}: wait {id}"
                );
            }
        }

        // What the display shows of the loaded conversation.

        let display = |stored: StoredTranscript| {
            project(
                &stored
                    .retained
                    .into_iter()
                    .chain(stored.lines)
                    .collect::<Vec<_>>(),
            )
            .into_iter()
            .filter(|node| {
                shown.contains(&node.id)
                    || node.parent.is_some_and(|parent| shown.contains(&parent))
            })
            .map(|node| serde_json::to_string(&node).unwrap())
            // The order nodes first appeared in differs where the snapshot repeats records.
            .collect::<std::collections::BTreeSet<_>>()
        };
        assert_eq!(
            display(from_latest_boundary(&lines)),
            display(from_latest_boundary(&without_snapshots(&lines))),
            "seed {seed}: display"
        );

        // The snapshot after a load carries the newest Lines of each unknown kind.
        let unknown = |loaded: &super::super::LoadedTranscript| {
            let Line::RetainedSnapshot(snapshot) = loaded.retained.snapshot(generator.chat) else {
                unreachable!();
            };
            snapshot
                .lines
                .into_iter()
                .filter(|line| matches!(line, Line::Unknown(_)))
                .map(|line| serde_json::to_string(&line).unwrap())
                .collect::<Vec<_>>()
        };
        let carried = unknown(&folded);
        let newest = lines
            .iter()
            .filter_map(|line| match line {
                Line::Unknown(unknown) => Some(unknown.kind.clone()),
                _ => None,
            })
            .collect::<HashSet<_>>()
            .into_iter()
            .map(|kind| {
                lines
                    .iter()
                    .filter(|line| matches!(line, Line::Unknown(unknown) if unknown.kind == kind))
                    .count()
                    .min(16)
            })
            .sum::<usize>();
        assert_eq!(carried.len(), newest, "seed {seed}: unknown Lines carried");
        assert_eq!(
            unknown(&full).len(),
            newest,
            "seed {seed}: unknown Lines folded"
        );
    }
}

#[test]
fn answered_question_blocks_survive_snapshots_until_their_answer_is_compacted() {
    let chat = Uuid::new_v4();
    let response = Uuid::new_v4();
    let assistant = Uuid::new_v4();
    let question = Uuid::new_v4();
    let call = parse(json!({
        "type": "assistant", "uuid": assistant, "chatId": chat, "timestamp": TIMESTAMP,
        "version": "test", "humanFamilyOrigin": null, "responseId": response,
        "message": {"role": "assistant", "content": [{"type": "tool_use", "id": "ask", "name": "AskUserQuestion", "input": {}}]},
    }));
    let result = parse(json!({
        "type": "user", "uuid": Uuid::new_v4(), "parentUuid": assistant, "chatId": chat,
        "timestamp": TIMESTAMP, "version": "test", "humanFamilyOrigin": null, "responseId": response,
        "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "ask", "content": "Selected A", "is_error": false}]},
    }));
    let wait = parse(json!({
        "type": "wait", "waitId": question, "chatId": chat, "loopId": chat, "runId": response,
        "responseId": response, "timestamp": TIMESTAMP, "state": {"kind": "appended"},
        "kind": {"kind": "question", "operationId": question, "request": {
            "id": question, "chat_id": chat, "loop_id": chat, "run_id": response,
            "call_id": "ask", "questions": [{"question": "Which?", "header": "Choice", "options": []}],
        }},
        "resume": {"kind": "inBatch", "assistantMessage": assistant, "callId": "ask"},
    }));
    let ended = parse(json!({
        "type": "response", "responseId": response, "attempt": 1, "runId": response,
        "loopId": chat, "displayParent": null, "timestamp": TIMESTAMP,
        "state": {"kind": "ended", "ending": {"kind": "completed"}},
    }));
    let lines = vec![call.clone(), result.clone(), wait, ended];
    let mut fold = RetainedFold::default();
    for line in &lines {
        fold.apply(line);
    }
    let snapshot = fold.snapshot(chat);
    let projected = project(&[call, result, snapshot.clone()]);
    assert_eq!(projected, project(&lines));
    assert!(
        matches!(&projected[0].blocks[1], tickloop_agent_transcript::display::DisplayBlock::Question { reply: Some(reply), .. } if reply.text == "Selected A")
    );
    let Line::RetainedSnapshot(snapshot) = snapshot else {
        panic!("snapshot")
    };
    assert!(
        snapshot
            .lines
            .iter()
            .any(|line| matches!(line, Line::Wait(_)))
    );
    let boundary = parse(json!({
        "type": "system", "uuid": Uuid::new_v4(), "chatId": chat, "timestamp": TIMESTAMP,
        "version": "test", "humanFamilyOrigin": null, "subtype": "compact_boundary",
        "message": {"role": "system", "content": []},
    }));
    fold.apply(&boundary);
    let Line::RetainedSnapshot(snapshot) = fold.snapshot(chat) else {
        panic!("snapshot")
    };
    assert!(
        !snapshot
            .lines
            .iter()
            .any(|line| matches!(line, Line::Wait(_)))
    );
}

#[test]
fn goal_events_survive_every_snapshot_in_order() {
    let chat = Uuid::new_v4();
    let event = |notice: tickloop_agent_protocol::GoalNotice| {
        Line::GoalEvent(tickloop_agent_transcript::GoalEventEntry {
            chat_id: chat,
            goal_id: Uuid::nil(),
            objective: "Ship it".into(),
            notice,
            timestamp: TIMESTAMP.into(),
        })
    };
    let mut fold = RetainedFold::default();
    fold.apply(&event(tickloop_agent_protocol::GoalNotice::Set));
    let first = fold.snapshot(chat);
    let mut reloaded = RetainedFold::default();
    reloaded.apply(&first);
    reloaded.apply(&event(tickloop_agent_protocol::GoalNotice::Cleared));
    let second = reloaded.snapshot(chat);

    let notices = tickloop_agent_transcript::display::goal_events(&[second])
        .into_iter()
        .map(|entry| entry.notice)
        .collect::<Vec<_>>();
    assert_eq!(
        notices,
        [
            tickloop_agent_protocol::GoalNotice::Set,
            tickloop_agent_protocol::GoalNotice::Cleared
        ]
    );
}

//! The display projection: what a user interface shows of a Chat, derived from its Lines alone.
//!
//! Display nodes are prompts and answers. A prompt's display id is its earliest source id
//! (`originalMessageId`, else its uuid; a prompt submitted under a ledger input has that input's
//! id). An answer's display id is its response id. Entries a compaction rewrote only alias the
//! node they copy. Internal entries (Loop records, compaction markers, host notes, protocol
//! data) are folded away: a prompt hangs under the display node its physical parent belongs to,
//! and an answer under its Run's prompt or the parent its lifecycle record names. Loops and
//! task boards and scheduled tasks have separate projections, without adding conversation nodes.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use tickloop_agent_protocol::{
    AgentColor, AgentMessageSource, Authority, ChatId, Content, GoalSnapshot, ImageContext,
    InputContext, LocalMessage, LoopId, LoopState, Protocol, Question, Receiving, Role,
    ScheduledTask, SharedTask, SubagentRegistration, TaskBoard, TickSubscription, TriggerIdentity,
};
use uuid::Uuid;

use crate::entry::{
    ChainEntry, GoalEventEntry, InputEntry, InputOp, InputState, InterruptedBy, Line, LoopRecord,
    ResponseEnding, ResponseEntry, ResponseState, WaitEntry, WaitKind, WaitState,
};

/// The longest tool result text a tool card carries.
pub const DISPLAY_TOOL_RESULT_CHARS: usize = 4096;
const SUBAGENT_COMPLETION_CALL_ID: &str = "subagent-completion";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DisplayNode {
    pub id: Uuid,
    pub parent: Option<Uuid>,
    pub kind: DisplayKind,
    pub blocks: Vec<DisplayBlock>,
    pub status: DisplayStatus,
    pub timestamp: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum DisplayKind {
    Prompt {
        input_id: Option<Uuid>,
        #[serde(default)]
        origin: PromptOrigin,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        is_event: bool,
    },
    /// One attempt of one Run; `attempt` is 0 until its lifecycle record is known.
    Answer {
        input_id: Option<Uuid>,
        attempt: u32,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum PromptOrigin {
    #[default]
    User,
    /// A Trigger delivery: where it came from, and how many source events it merges.
    Trigger {
        identity: TriggerIdentity,
        authority: Authority,
        merged: u64,
    },
    Feedback {
        artifact: String,
    },
    AgentMessage {
        from: ChatId,
    },
    SubagentNotification {
        child: ChatId,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DisplayMember {
    pub chat: ChatId,
    pub handle: String,
    pub color: Option<AgentColor>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum DisplayBlock {
    Text {
        text: String,
    },
    /// The model's visible reasoning before or between its answer text and tool calls. The
    /// Transcript does not record when it started or ended; a host that wants its duration
    /// measures the streamed `Thinking` events.
    Thinking {
        text: String,
    },
    Image(ImageContext),
    ToolCall {
        call_id: String,
        name: String,
        input: serde_json::Value,
        result: Option<DisplayToolResult>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        member: Option<DisplayMember>,
    },
    Question {
        request_id: Uuid,
        questions: Vec<Question>,
        reply: Option<DisplayToolResult>,
    },
    /// A prompt that joined the answer at a tool boundary.
    FollowUp {
        prompt_id: Uuid,
        input_id: Option<Uuid>,
        text: String,
        images: Vec<ImageContext>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DisplayToolResult {
    pub text: String,
    pub is_error: bool,
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum DisplayStatus {
    Complete,
    Streaming,
    Waiting { r#for: DisplayWait },
    Interrupted { by: InterruptedBy },
    Failed { error: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum DisplayWait {
    Person { request: Uuid },
    Member { chat: Uuid },
}

#[derive(Clone, Debug, PartialEq)]
pub enum DisplayChange {
    /// The node's whole current state, new or changed.
    Upsert(DisplayNode),
}

/// Applies Lines in store order and reports the display nodes each one changed.
#[derive(Clone, Debug, Default)]
pub struct DisplayProjector {
    nodes: HashMap<Uuid, DisplayNode>,
    order: Vec<Uuid>,
    /// The display node each applied chain entry is part of, or follows when it is internal.
    positions: HashMap<Uuid, Option<Uuid>>,
    responses: HashMap<Uuid, ResponseEntry>,
    waits: Vec<WaitEntry>,
    subagents: Vec<SubagentRegistration>,
    inputs: HashMap<Uuid, InputEntry>,
    prompt_candidates: HashMap<Uuid, ChainEntry>,
    prompts_by_run: HashMap<Uuid, Uuid>,
}

/// The display nodes of `lines`, in the order they first appeared.
pub fn project(lines: &[Line]) -> Vec<DisplayNode> {
    let mut projector = DisplayProjector::default();
    for line in lines {
        projector.apply(line);
    }
    projector.nodes().cloned().collect()
}

/// The Chat Loop's latest recorded state.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DisplayLoop {
    pub id: Uuid,
    pub state: LoopState,
    pub receiving: Receiving,
    pub subscriptions: Vec<TickSubscription>,
    pub state_since: Option<u64>,
    pub run_started_at: Option<u64>,
}

/// The Chat Loop as its latest current-format record on the chain left it; records in the format
/// of a Chat with several Loops are skipped.
pub fn chat_loop(lines: &[Line]) -> Option<DisplayLoop> {
    let mut latest: Option<LoopRecord> = None;
    visit_lines(lines, &mut |line| {
        if let Some(record) = line
            .chain_entry()
            .filter(|entry| entry.is_current_loop_record())
            .and_then(|entry| entry.loop_record.as_ref())
        {
            latest = Some(record.clone());
        }
    });
    let record = latest?;
    let state = serde_json::from_value(record.state).ok()?;
    let receiving = record
        .receiving
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default();
    let subscriptions = record
        .subscriptions
        .into_iter()
        .filter_map(|value| serde_json::from_value(value).ok())
        .collect();
    Some(DisplayLoop {
        id: record.loop_id,
        state,
        receiving,
        subscriptions,
        state_since: record.state_since.and_then(|value| value.as_u64()),
        run_started_at: record.run_started_at,
    })
}

/// Public board state, without mutation receipts. Disabling Tasks hides, rather than clears, it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DisplayTaskBoard {
    pub root_chat_id: ChatId,
    pub root_loop_id: LoopId,
    pub revision: u64,
    pub is_team: bool,
    pub task_number_offset: u64,
    pub tasks: Vec<SharedTask>,
}

pub fn task_boards(lines: &[Line], enabled: bool) -> Vec<DisplayTaskBoard> {
    if !enabled {
        return Vec::new();
    }
    let mut boards = Vec::new();
    visit_lines(lines, &mut |line| {
        if let Line::TaskBoards(entry) = line {
            boards = entry.boards.values().cloned().collect();
        }
    });
    boards
        .into_iter()
        .filter(|board: &TaskBoard| !board.tasks.is_empty())
        .map(|board| DisplayTaskBoard {
            root_chat_id: board.root_chat_id,
            root_loop_id: board.root_loop_id,
            revision: board.revision,
            is_team: board.is_team,
            task_number_offset: board.task_number_offset,
            tasks: board.tasks,
        })
        .collect()
}

/// The Chat's scheduled tasks as its latest record keeps them.
pub fn scheduled_tasks(lines: &[Line]) -> Vec<ScheduledTask> {
    let mut tasks = Vec::new();
    visit_lines(lines, &mut |line| {
        if let Line::ScheduledTasks(entry) = line {
            tasks = entry.tasks.clone();
        }
    });
    tasks
}

/// One Trigger delivery as the ledger last recorded it: its source, how many source events it
/// merges, and where it went: waiting, taken into a turn, merged into another delivery, or
/// withdrawn with the reason.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DisplayTriggerDelivery {
    pub input_id: Uuid,
    pub run_id: Uuid,
    pub identity: TriggerIdentity,
    pub merged: u64,
    pub state: InputState,
    pub timestamp: String,
}

/// Every Trigger delivery the Chat's ledger records, in the order they were first accepted.
pub fn trigger_deliveries(lines: &[Line]) -> Vec<DisplayTriggerDelivery> {
    let mut deliveries: Vec<DisplayTriggerDelivery> = Vec::new();
    visit_lines(lines, &mut |line| {
        let Line::Input(entry) = line else {
            return;
        };
        let Some(trigger) = &entry.trigger else {
            return;
        };
        let delivery = DisplayTriggerDelivery {
            input_id: entry.input_id,
            run_id: entry.run_id,
            identity: trigger.identity.clone(),
            merged: trigger.merged,
            state: entry.state.clone(),
            timestamp: entry.timestamp.clone(),
        };
        match deliveries
            .iter_mut()
            .find(|known| known.input_id == entry.input_id)
        {
            Some(known) => *known = delivery,
            None => deliveries.push(delivery),
        }
    });
    deliveries
}

/// The Chat's goal as its latest record left it; `None` when it has none or it was cleared.
pub fn goal(lines: &[Line]) -> Option<GoalSnapshot> {
    let mut latest = None;
    visit_lines(lines, &mut |line| {
        if let Line::Goal(entry) = line {
            latest = entry.goal.clone();
        }
    });
    latest
}

/// The goal changes the conversation shows as lines, in the order they happened: set, complete,
/// blocked, budget limited, paused and cleared.
pub fn goal_events(lines: &[Line]) -> Vec<GoalEventEntry> {
    let mut events = Vec::new();
    visit_lines(lines, &mut |line| {
        if let Line::GoalEvent(entry) = line {
            events.push(entry.clone());
        }
    });
    events
}

fn visit_lines(lines: &[Line], visit: &mut impl FnMut(&Line)) {
    for line in lines {
        match line {
            Line::RetainedSnapshot(snapshot) => visit_lines(&snapshot.lines, visit),
            _ => visit(line),
        }
    }
}

fn display_result(text: &str, is_error: bool) -> DisplayToolResult {
    DisplayToolResult {
        text: text.chars().take(DISPLAY_TOOL_RESULT_CHARS).collect(),
        is_error,
        truncated: text.chars().count() > DISPLAY_TOOL_RESULT_CHARS,
    }
}

impl DisplayProjector {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn node(&self, id: Uuid) -> Option<&DisplayNode> {
        self.nodes.get(&id)
    }

    pub fn nodes(&self) -> impl Iterator<Item = &DisplayNode> {
        self.order.iter().filter_map(|id| self.nodes.get(id))
    }

    /// The display node a physical chain entry is part of or, for an internal entry, follows.
    pub fn canonical_id(&self, physical: Uuid) -> Option<Uuid> {
        self.positions.get(&physical).copied().flatten()
    }

    pub fn apply(&mut self, line: &Line) -> Vec<DisplayChange> {
        if let Line::RetainedSnapshot(snapshot) = line {
            return snapshot
                .lines
                .iter()
                .flat_map(|line| self.apply(line))
                .collect();
        }
        let mut changed = match line {
            Line::Response(response) => {
                self.responses
                    .insert(response.response_id, response.clone());
                self.answer(
                    response.response_id,
                    response.display_parent,
                    &response.timestamp,
                );
                vec![response.response_id]
            }
            Line::Wait(wait) => {
                if let Some(previous) = self
                    .waits
                    .iter_mut()
                    .find(|old| old.wait_id == wait.wait_id)
                {
                    *previous = wait.clone();
                } else {
                    self.waits.push(wait.clone());
                }
                vec![wait.response_id]
            }
            Line::Subagents(entry) => {
                self.subagents = entry.subagents.clone();
                self.order.clone()
            }
            Line::Input(input) if input.op != InputOp::Answer => {
                self.inputs.insert(input.run_id, input.clone());
                let mut changed = Vec::new();
                if let Some(entry) = self.prompt_candidates.get(&input.run_id).cloned() {
                    if !self.prompts_by_run.contains_key(&input.run_id) {
                        self.positions.remove(&entry.uuid);
                        changed.extend(self.apply_entry(&entry));
                    } else if let Some(id) = self.prompts_by_run.get(&input.run_id).copied() {
                        let origin = self.prompt_origin(&entry).unwrap_or_default();
                        if let Some(node) = self.nodes.get_mut(&id)
                            && let DisplayKind::Prompt {
                                origin: shown,
                                is_event,
                                ..
                            } = &mut node.kind
                        {
                            *shown = origin;
                            *is_event = is_goal_continuation(shown);
                            node.blocks = prompt_blocks(&entry, shown);
                            changed.push(id);
                        }
                    }
                    changed.extend(
                        self.responses
                            .values()
                            .filter(|response| response.run_id == input.run_id)
                            .map(|response| response.response_id),
                    );
                }
                changed
            }
            _ => line
                .chain_entry()
                .and_then(|entry| self.apply_entry(entry))
                .into_iter()
                .collect(),
        };
        // A prompt may arrive after its answer's lifecycle metadata in a retained snapshot.
        let mut related = Vec::new();
        for id in &changed {
            if self
                .nodes
                .get(id)
                .is_some_and(|node| matches!(node.kind, DisplayKind::Prompt { .. }))
            {
                related.extend(
                    self.responses
                        .values()
                        .filter(|response| self.prompts_by_run.get(&response.run_id) == Some(id))
                        .map(|response| response.response_id),
                );
            }
        }
        changed.extend(related);
        let mut seen = std::collections::HashSet::new();
        changed.retain(|id| seen.insert(*id));
        for id in &changed {
            self.refresh_answer(*id);
        }
        changed
            .into_iter()
            .filter_map(|id| self.nodes.get(&id).cloned())
            .map(DisplayChange::Upsert)
            .collect()
    }

    fn refresh_answer(&mut self, id: Uuid) {
        let prompt_parent = self
            .responses
            .get(&id)
            .and_then(|response| self.prompts_by_run.get(&response.run_id))
            .copied()
            .filter(|prompt| {
                self.nodes.get(prompt).is_some_and(|node| {
                    matches!(node.kind,
                DisplayKind::Prompt { ref origin, .. } if *origin != PromptOrigin::User)
                })
            });
        let Some(node) = self.nodes.get_mut(&id) else {
            return;
        };
        if !matches!(node.kind, DisplayKind::Answer { .. }) {
            return;
        }
        if let Some(response) = self.responses.get(&id) {
            node.parent = prompt_parent.or(response.display_parent);
            node.kind = DisplayKind::Answer {
                input_id: response.input_id,
                attempt: response.attempt,
            };
            node.status = match &response.state {
                ResponseState::Started | ResponseState::Suspended | ResponseState::Resumed => {
                    DisplayStatus::Streaming
                }
                ResponseState::Ended { ending } => match ending {
                    ResponseEnding::Completed => DisplayStatus::Complete,
                    ResponseEnding::Interrupted {
                        by: InterruptedBy::Handover,
                    } => DisplayStatus::Streaming,
                    ResponseEnding::Interrupted { by } => DisplayStatus::Interrupted { by: *by },
                    ResponseEnding::Failed { error } => DisplayStatus::Failed {
                        error: error.clone(),
                    },
                },
            };
            if response.state == ResponseState::Suspended
                && let Some(wait) = self
                    .waits
                    .iter()
                    .find(|wait| wait.response_id == id && matches!(wait.state, WaitState::Open))
            {
                let waiting = match &wait.kind {
                    WaitKind::Question { request, .. } => Some(DisplayWait::Person {
                        request: request.id,
                    }),
                    WaitKind::Subagent { child_chat, .. } => {
                        Some(DisplayWait::Member { chat: *child_chat })
                    }
                    WaitKind::Children { children } => children
                        .first()
                        .map(|chat| DisplayWait::Member { chat: *chat }),
                    // A handover waits for no one: the answer goes on streaming.
                    WaitKind::Handover => None,
                };
                if let Some(waiting) = waiting {
                    node.status = DisplayStatus::Waiting { r#for: waiting };
                }
            }
        }
        for block in &mut node.blocks {
            if let DisplayBlock::ToolCall {
                name,
                input,
                result,
                member,
                call_id,
            } = block
                && name == tickloop_agent_protocol::SUBAGENT_TOOL_NAME
            {
                let child = self
                    .waits
                    .iter()
                    .find_map(|wait| {
                        (wait.response_id == id && wait.resume.call_id() == Some(call_id.as_str()))
                            .then_some(&wait.kind)
                            .and_then(|kind| match kind {
                                WaitKind::Subagent { child_chat, .. } => Some(*child_chat),
                                _ => None,
                            })
                    })
                    .or_else(|| {
                        result.as_ref().and_then(|result| {
                            let value: serde_json::Value =
                                serde_json::from_str(&result.text).ok()?;
                            serde_json::from_value::<ChatId>(value["child_chat_id"].clone())
                                .ok()
                                .map(ChatId::uuid)
                        })
                    });
                *member = self
                    .subagents
                    .iter()
                    .find(|registration| {
                        child.map_or_else(
                            || {
                                result.is_none()
                                    && input["name"].as_str() == Some(registration.handle.as_str())
                            },
                            |chat| registration.child_chat_id.uuid() == chat,
                        )
                    })
                    .map(|registration| DisplayMember {
                        chat: registration.child_chat_id,
                        handle: registration.handle.clone(),
                        color: registration.color,
                    });
            }
        }
        for wait in self.waits.iter().filter(|wait| wait.response_id == id) {
            let WaitKind::Question { request, .. } = &wait.kind else {
                continue;
            };
            let Some(call_index) = node.blocks.iter().position(|block| {
                matches!(block,
                DisplayBlock::ToolCall { call_id, .. } if call_id == &request.call_id)
            }) else {
                continue;
            };
            let reply = match &node.blocks[call_index] {
                DisplayBlock::ToolCall {
                    result: Some(result),
                    ..
                } => Some(result.clone()),
                _ => match &wait.state {
                    WaitState::Resolved { result, .. } => {
                        Some(display_result(&result.content.text(), result.tool.is_error))
                    }
                    _ => None,
                },
            };
            let block = DisplayBlock::Question {
                request_id: request.id,
                questions: request.questions.clone(),
                reply,
            };
            if let Some(previous) = node.blocks.iter_mut().find(|block| {
                matches!(block,
                DisplayBlock::Question { request_id, .. } if *request_id == request.id)
            }) {
                *previous = block;
            } else {
                node.blocks.insert(call_index + 1, block);
            }
        }
    }

    fn prompt_origin(&self, entry: &ChainEntry) -> Option<PromptOrigin> {
        if let Some(source) = entry
            .agent_message_origin
            .as_ref()
            .and_then(|value| serde_json::from_value::<AgentMessageSource>(value.clone()).ok())
        {
            return Some(
                if source.call_id.as_deref() == Some(SUBAGENT_COMPLETION_CALL_ID)
                    && source.is_result
                {
                    PromptOrigin::SubagentNotification {
                        child: source.sender_chat_id,
                    }
                } else {
                    PromptOrigin::AgentMessage {
                        from: source.sender_chat_id,
                    }
                },
            );
        }
        let input = entry.run_id.and_then(|run| self.inputs.get(&run));
        match input.map(|input| input.op) {
            Some(InputOp::Trigger) => {
                let trigger = input?.trigger.as_ref()?;
                Some(PromptOrigin::Trigger {
                    identity: trigger.identity.clone(),
                    authority: trigger.kind().authority(),
                    merged: trigger.merged,
                })
            }
            Some(InputOp::Feedback) => {
                let artifact = match LocalMessage::parse(&message_text(&entry.message.content)) {
                    Some(LocalMessage::ArtifactRequest { artifact_id, .. }) => Some(artifact_id),
                    _ => input?
                        .queued
                        .as_ref()
                        .and_then(|queued| queued["trigger_source"]["artifact_id"].as_str())
                        .map(str::to_owned),
                }?;
                Some(PromptOrigin::Feedback { artifact })
            }
            _ if entry.execution_mode.is_some() => Some(PromptOrigin::User),
            _ => None,
        }
    }

    fn apply_entry(&mut self, entry: &ChainEntry) -> Option<Uuid> {
        if self.positions.contains_key(&entry.uuid) {
            return None;
        }
        let follows = entry
            .parent_uuid
            .or(entry.logical_parent_uuid)
            .and_then(|parent| self.positions.get(&parent).copied().flatten());
        // A compaction copies whole entries; a fragment appended later to a copied message
        // carries the message's source id but holds new content.
        if !entry.continues_message
            && let Some(source) = entry.original_message_id
            && let Some(copied) = self.positions.get(&source).copied()
        {
            self.positions.insert(entry.uuid, copied);
            return None;
        }
        let is_candidate = entry.message.role == Role::User
            && !entry.is_meta
            && !entry.is_compact_summary
            && !entry.continues_message
            && (entry.execution_mode.is_some()
                || entry
                    .message
                    .content
                    .iter()
                    .any(|content| matches!(content, Content::Text { .. })))
            && !entry
                .message
                .content
                .iter()
                .any(|content| matches!(content, Content::ToolResult { .. }));
        if is_candidate
            && entry.response_id.is_none()
            && let Some(run) = entry.run_id
        {
            self.prompt_candidates
                .entry(run)
                .or_insert_with(|| entry.clone());
        }
        let origin = is_candidate.then(|| self.prompt_origin(entry)).flatten();
        let is_prompt = origin.is_some()
            && (entry.execution_mode.is_some()
                || entry
                    .run_id
                    .is_none_or(|run| !self.prompts_by_run.contains_key(&run)));
        let (position, changed) = match entry.response_id {
            Some(response) => {
                let follow_up = is_prompt.then(|| DisplayBlock::FollowUp {
                    prompt_id: entry.original_message_id.unwrap_or(entry.uuid),
                    input_id: entry.input_id,
                    text: message_text(&entry.message.content),
                    images: prompt_images(entry),
                });
                let node = self.answer(response, follows, &entry.timestamp);
                match follow_up {
                    Some(block) => node.blocks.push(block),
                    None => add_answer_content(node, entry),
                }
                (Some(response), Some(response))
            }
            None if is_prompt => {
                let id = entry.original_message_id.unwrap_or(entry.uuid);
                let origin = origin.unwrap_or_default();
                let blocks = prompt_blocks(entry, &origin);
                if let Some(run) = entry.run_id {
                    self.prompts_by_run.insert(run, id);
                }
                self.insert(DisplayNode {
                    id,
                    parent: follows,
                    kind: DisplayKind::Prompt {
                        input_id: entry.input_id,
                        is_event: is_goal_continuation(&origin),
                        origin,
                    },
                    blocks,
                    status: DisplayStatus::Complete,
                    timestamp: entry.timestamp.clone(),
                });
                if let Some(answer_index) = self.order.iter().position(|answer| {
                    self.responses
                        .get(answer)
                        .is_some_and(|response| Some(response.run_id) == entry.run_id)
                }) {
                    self.order.retain(|node| *node != id);
                    self.order.insert(answer_index, id);
                }
                (Some(id), Some(id))
            }
            None => (follows, None),
        };
        self.positions.insert(entry.uuid, position);
        // A copy seen before its source (a load from a later boundary) stands for the source, so
        // a later copy of the same source only aliases it.
        if !entry.continues_message
            && let Some(source) = entry.original_message_id
        {
            self.positions.entry(source).or_insert(position);
        }
        changed
    }

    fn insert(&mut self, node: DisplayNode) {
        if !self.nodes.contains_key(&node.id) {
            self.order.push(node.id);
        }
        self.nodes.insert(node.id, node);
    }

    /// The answer node, created streaming under `parent` when it is new.
    fn answer(&mut self, id: Uuid, parent: Option<Uuid>, timestamp: &str) -> &mut DisplayNode {
        if !self.nodes.contains_key(&id) {
            self.insert(DisplayNode {
                id,
                parent,
                kind: DisplayKind::Answer {
                    input_id: None,
                    attempt: 0,
                },
                blocks: Vec::new(),
                status: DisplayStatus::Streaming,
                timestamp: timestamp.to_owned(),
            });
        }
        self.nodes.get_mut(&id).unwrap()
    }
}

fn prompt_blocks(entry: &ChainEntry, origin: &PromptOrigin) -> Vec<DisplayBlock> {
    let mut text = message_text(&entry.message.content);
    if let PromptOrigin::Trigger { identity, .. } = origin {
        text = identity.display_input(&text).into_owned();
    }
    let mut blocks = Vec::new();
    if !text.is_empty() {
        blocks.push(DisplayBlock::Text { text });
    }
    if !is_goal_continuation(origin) {
        blocks.extend(prompt_images(entry).into_iter().map(DisplayBlock::Image));
    }
    blocks
}

fn is_goal_continuation(origin: &PromptOrigin) -> bool {
    matches!(
        origin,
        PromptOrigin::Trigger {
            identity: TriggerIdentity::GoalContinuation { .. },
            ..
        }
    )
}

fn message_text(content: &[Content]) -> String {
    content
        .iter()
        .filter_map(|content| match content {
            Content::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn prompt_images(entry: &ChainEntry) -> Vec<ImageContext> {
    entry
        .contexts
        .clone()
        .and_then(|contexts| serde_json::from_value::<Vec<InputContext>>(contexts).ok())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|context| match context {
            InputContext::Image(image) => Some(image),
            _ => None,
        })
        .collect()
}

/// Thinking continues the block right before it (Gemini stores each streamed thought part as
/// its own content), so one stretch of thinking is one block; a block never starts with only
/// whitespace.
fn add_thinking(node: &mut DisplayNode, text: &str) {
    match node.blocks.last_mut() {
        Some(DisplayBlock::Thinking { text: shown }) => shown.push_str(text),
        _ if !text.trim().is_empty() => node.blocks.push(DisplayBlock::Thinking {
            text: text.to_owned(),
        }),
        _ => {}
    }
}

/// The readable text of a Responses reasoning item: its reasoning text, else its summary.
fn responses_reasoning_text(item: &serde_json::Value) -> String {
    let texts = |field: &str| -> Vec<&str> {
        item[field]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|part| part["text"].as_str())
            .filter(|text| !text.is_empty())
            .collect()
    };
    let content = texts("content");
    let parts = if content.is_empty() {
        texts("summary")
    } else {
        content
    };
    parts.join("\n\n")
}

/// An answer shows its model text, its thinking (including Responses reasoning summaries) and
/// its tool cards; other protocol data and host-written user text stay out.
fn add_answer_content(node: &mut DisplayNode, entry: &ChainEntry) {
    let is_model_output = entry.message.role == Role::Assistant;
    for content in &entry.message.content {
        match content {
            Content::Text { text } if is_model_output => {
                node.blocks.push(DisplayBlock::Text { text: text.clone() });
            }
            Content::Thinking { thinking, .. } if is_model_output => add_thinking(node, thinking),
            Content::ProtocolData {
                protocol: Protocol::Responses,
                value,
            } if is_model_output && value["type"] == "reasoning" => {
                add_thinking(node, &responses_reasoning_text(value));
            }
            Content::ToolUse { id, name, input } => node.blocks.push(DisplayBlock::ToolCall {
                call_id: id.clone(),
                name: name.clone(),
                input: input.clone(),
                result: None,
                member: None,
            }),
            Content::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                let card = node.blocks.iter_mut().find_map(|block| match block {
                    DisplayBlock::ToolCall {
                        call_id, result, ..
                    } if call_id == tool_use_id => Some(result),
                    _ => None,
                });
                if let Some(result) = card {
                    let text = content.text();
                    *result = Some(display_result(&text, *is_error));
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tickloop_agent_protocol::Message;

    fn entry(uuid: Uuid, parent: Option<Uuid>, role: Role, content: Vec<Content>) -> ChainEntry {
        let role_name = match role {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::System => "system",
        };
        serde_json::from_value(serde_json::json!({
            "type": role_name,
            "uuid": uuid,
            "parentUuid": parent,
            "chatId": Uuid::nil(),
            "timestamp": "2026-01-01T00:00:00.000Z",
            "version": "test",
            "message": Message { role, content },
            "humanFamilyOrigin": null,
        }))
        .unwrap()
    }

    fn tool_use(id: &str) -> Content {
        Content::ToolUse {
            id: id.into(),
            name: "Read".into(),
            input: serde_json::json!({}),
        }
    }

    fn tool_result(id: &str) -> Content {
        Content::ToolResult {
            tool_use_id: id.into(),
            content: format!("result {id}").into(),
            is_error: false,
        }
    }

    fn thinking(text: &str) -> Content {
        Content::Thinking {
            thinking: text.into(),
            signature: Some("signed".into()),
        }
    }

    /// Thinking shows where the model wrote it, once per model call, and empty thinking (a
    /// signature or redacted reasoning without text) shows nothing.
    #[test]
    fn an_answer_shows_its_thinking_in_order_and_skips_empty_thinking() {
        let prompt = Uuid::new_v4();
        let response = Uuid::new_v4();
        let mut prompt_entry = entry(
            prompt,
            None,
            Role::User,
            vec![Content::Text {
                text: "question".into(),
            }],
        );
        prompt_entry.execution_mode = Some(serde_json::json!("default"));
        let first_call = Uuid::new_v4();
        let mut call = entry(
            first_call,
            Some(prompt),
            Role::Assistant,
            vec![thinking("Look it up first."), tool_use("a")],
        );
        call.response_id = Some(response);
        let results = Uuid::new_v4();
        let mut result = entry(
            results,
            Some(first_call),
            Role::User,
            vec![tool_result("a")],
        );
        result.response_id = Some(response);
        let mut reply = entry(
            Uuid::new_v4(),
            Some(results),
            Role::Assistant,
            vec![
                thinking(" \n"),
                thinking("The result "),
                Content::ProtocolData {
                    protocol: Protocol::Google,
                    value: serde_json::json!({ "paired": true }),
                },
                thinking("answers it."),
                Content::Text {
                    text: "Answer.".into(),
                },
            ],
        );
        reply.response_id = Some(response);

        let nodes = project(
            &[prompt_entry, call, result, reply]
                .into_iter()
                .map(|entry| Line::chain(entry.message.role, entry))
                .collect::<Vec<_>>(),
        );
        let answer = &nodes[1];
        assert_eq!(answer.id, response);
        let shown = answer
            .blocks
            .iter()
            .map(|block| match block {
                DisplayBlock::Thinking { text } => format!("thinking: {text}"),
                DisplayBlock::Text { text } => format!("text: {text}"),
                DisplayBlock::ToolCall { call_id, .. } => format!("tool: {call_id}"),
                other => format!("{other:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            shown,
            [
                "thinking: Look it up first.",
                "tool: a",
                "thinking: The result answers it.",
                "text: Answer.",
            ]
        );
        let serialized = serde_json::to_string(&nodes).unwrap();
        assert!(!serialized.contains("signed"), "signatures stay out");
    }

    #[test]
    fn a_responses_reasoning_item_shows_its_text_or_summary() {
        let reasoning = |value: serde_json::Value| Content::ProtocolData {
            protocol: Protocol::Responses,
            value,
        };
        let mut node = DisplayNode {
            id: Uuid::new_v4(),
            parent: None,
            kind: DisplayKind::Answer {
                input_id: None,
                attempt: 1,
            },
            blocks: Vec::new(),
            status: DisplayStatus::Streaming,
            timestamp: String::new(),
        };
        let mut reply = entry(
            Uuid::new_v4(),
            None,
            Role::Assistant,
            vec![
                reasoning(serde_json::json!({
                    "type": "reasoning", "encrypted_content": "opaque",
                    "summary": [{ "type": "summary_text", "text": "Plan." },
                                { "type": "summary_text", "text": "Check." }],
                })),
                reasoning(serde_json::json!({ "type": "reasoning", "summary": [] })),
                Content::Text {
                    text: "Done.".into(),
                },
                reasoning(serde_json::json!({ "type": "message", "content": [{ "text": "no" }] })),
            ],
        );
        reply.response_id = Some(node.id);
        add_answer_content(&mut node, &reply);
        assert_eq!(
            node.blocks,
            [
                DisplayBlock::Thinking {
                    text: "Plan.\n\nCheck.".into()
                },
                DisplayBlock::Text {
                    text: "Done.".into()
                },
            ]
        );
        assert!(!serde_json::to_string(&node).unwrap().contains("opaque"));
    }

    /// A compaction's copy of an answer entry only aliases it, while a fragment appended to the
    /// copied message later still adds its content.
    #[test]
    fn copies_alias_and_later_fragments_of_a_copied_message_still_show() {
        let prompt = Uuid::new_v4();
        let response = Uuid::new_v4();
        let mut prompt_entry = entry(
            prompt,
            None,
            Role::User,
            vec![Content::Text {
                text: "question".into(),
            }],
        );
        prompt_entry.execution_mode = Some(serde_json::json!("default"));
        let assistant = Uuid::new_v4();
        let mut call = entry(
            assistant,
            Some(prompt),
            Role::Assistant,
            vec![tool_use("a"), tool_use("b")],
        );
        call.response_id = Some(response);
        let results = Uuid::new_v4();
        let mut first_result = entry(results, Some(assistant), Role::User, vec![tool_result("a")]);
        first_result.response_id = Some(response);
        let mut copy = call.clone();
        copy.uuid = Uuid::new_v4();
        copy.original_message_id = Some(assistant);
        let mut copied_results = first_result.clone();
        copied_results.uuid = Uuid::new_v4();
        copied_results.parent_uuid = Some(copy.uuid);
        copied_results.original_message_id = Some(results);
        let mut fragment = entry(
            Uuid::new_v4(),
            Some(copied_results.uuid),
            Role::User,
            vec![tool_result("b")],
        );
        fragment.response_id = Some(response);
        fragment.continues_message = true;
        fragment.original_message_id = Some(results);

        let nodes = project(
            &[
                prompt_entry,
                call,
                first_result,
                copy,
                copied_results,
                fragment,
            ]
            .into_iter()
            .map(|entry| Line::chain(entry.message.role, entry))
            .collect::<Vec<_>>(),
        );
        assert_eq!(nodes.len(), 2);
        let answer = &nodes[1];
        assert_eq!((answer.id, answer.parent), (response, Some(prompt)));
        let results = answer
            .blocks
            .iter()
            .map(|block| match block {
                DisplayBlock::ToolCall { result, .. } => result.as_ref().map(|r| r.text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(results, [Some("result a".into()), Some("result b".into())]);
    }
}

#[cfg(test)]
mod durable_tests;
#[cfg(test)]
mod goal_tests;

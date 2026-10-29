//! A durable load brings a whole Agent family in at once: one Chat Lease covers the root Chat and
//! every member it delegated, so they all go on in the instance that holds it. Every member is
//! restored before any of them goes on: the family's calls whose effect stayed inside the family
//! are settled by its records, a foreground call waits on its child again before that child goes
//! on, and a child whose end the load decides reports it to its parent like any other end.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use serde_json::json;
use tickloop_harness_protocol::{
    ChatId, ChatSnapshot, Content, DelegationMetadata, FamilyCompletion, LoopId, Message, Receipt,
    RunId, SEND_AGENT_MESSAGE_TOOL_NAME, SUBAGENT_TOOL_NAME, SubagentNotificationDelivery,
    SubagentRegistration, TaskBoard, ToolResult, projection::HistoryCall,
};
use tickloop_harness_transcript::{Line, ModelVisibleResult, WaitKind};
use uuid::Uuid;

use super::{Agent, AgentError, persistence::RestoreTarget};
use crate::transcript::LoadedTranscript;

/// Why a member's run ended when the family's load found no Transcript of that member.
const LOST_BEFORE_IT_STARTED: &str = "lost before it started";

/// What a family's stored records tell of the calls its members made to one another.
#[derive(Default)]
pub(crate) struct FamilyRecords {
    /// Each delegated child, by the parent Chat, Run and call that delegated it.
    launched: HashMap<(ChatId, RunId, String), LaunchedChild>,
    /// Every input some member accepted, by its id, with where it was accepted.
    accepted: HashMap<Uuid, Receipt>,
    /// The `Subagent` calls that waited for their child's report, by Chat, Run and call: a call
    /// that recorded no such wait launched its child in the background.
    waiting_calls: HashSet<(ChatId, RunId, String)>,
    task_boards: HashMap<(ChatId, LoopId), TaskBoard>,
}

struct LaunchedChild {
    /// The child's first run: the task its delegation gave it.
    task: Receipt,
    delegation: DelegationMetadata,
    registration: Option<SubagentRegistration>,
}

impl FamilyRecords {
    fn of(loaded: &HashMap<ChatId, LoadedTranscript>) -> Self {
        let mut records = Self::default();
        for (id, transcript) in loaded {
            if let Some(entry) = &transcript.metadata.task_boards
                && entry.chat_id == *id
            {
                for (loop_id, board) in &entry.boards {
                    records
                        .task_boards
                        .insert(((*id), (*loop_id).into()), board.clone());
                }
            }
            records
                .waiting_calls
                .extend(transcript.lifecycle.iter().filter_map(|line| match line {
                    Line::Wait(entry) if matches!(entry.kind, WaitKind::Subagent { .. }) => {
                        Some((*id, entry.run_id.into(), entry.resume.call_id()?.to_owned()))
                    }
                    _ => None,
                }));
            let inputs = transcript.lifecycle.iter().filter_map(|line| match line {
                Line::Input(entry) => Some(entry),
                _ => None,
            });
            let mut task = None;
            for entry in inputs {
                let receipt = Receipt {
                    chat_id: *id,
                    loop_id: entry.loop_id.into(),
                    run_id: entry.run_id.into(),
                };
                task.get_or_insert_with(|| receipt.clone());
                records.accepted.insert(entry.input_id, receipt);
            }
            let Some(delegation) = transcript
                .metadata
                .delegation
                .as_ref()
                .filter(|entry| entry.chat_id == *id)
                .map(|entry| entry.delegation.clone())
            else {
                continue;
            };
            let Some(task) = task else {
                continue;
            };
            let registration = loaded
                .get(&delegation.parent_chat_id)
                .and_then(|parent| parent.metadata.subagents.as_ref())
                .and_then(|entry| {
                    entry
                        .subagents
                        .iter()
                        .find(|registration| registration.child_chat_id == *id)
                        .cloned()
                });
            records.launched.insert(
                (
                    delegation.parent_chat_id,
                    delegation.parent_run_id,
                    delegation.parent_call_id.clone(),
                ),
                LaunchedChild {
                    task,
                    delegation,
                    registration,
                },
            );
        }
        records
    }

    /// The result the family's records give a dangling call of `snapshot`'s Chat: the child a
    /// `Subagent` call delegated, the message a `SendMessage` call's receiver accepted, the Task
    /// board's receipt of a Task mutation. `None` for any other call, and for one whose effect the
    /// records do not hold.
    pub(crate) fn reconcile(
        &self,
        ids: &tickloop_ids::IdContext,
        snapshot: &ChatSnapshot,
        message_responses: &BTreeMap<usize, tickloop_harness_protocol::ResponseId>,
        call: &HistoryCall,
    ) -> Option<ToolResult> {
        let Content::ToolUse { name, input, .. } = snapshot
            .messages
            .get(call.message_index)?
            .content
            .get(call.content_index)?
        else {
            return None;
        };
        let run = call.run_id?;
        match name.as_str() {
            SUBAGENT_TOOL_NAME => {
                let child = self
                    .launched
                    .get(&(snapshot.id, run, call.call_id.clone()))?;
                let registration = child.registration.as_ref()?;
                let task = &child.task;
                if !self
                    .waiting_calls
                    .contains(&(snapshot.id, run, call.call_id.clone()))
                {
                    return Some(crate::delegation::launched_result(ids, task, registration));
                }
                if let Some(completion) = snapshot.family_completions.iter().find(|completion| {
                    completion.child_chat_id == task.chat_id
                        && completion.child_run_id == task.run_id
                }) {
                    return crate::agent_messages::child_run_result(
                        ids,
                        completion,
                        &child.delegation,
                        Some(registration),
                        snapshot.id,
                    );
                }
                // The call stopped observing the child when its Run lost its host: the child
                // goes on, and its end reaches the parent as a notification.
                Some(crate::delegation::interrupted_observation_result(
                    ids,
                    task,
                    Some(registration),
                ))
            }
            SEND_AGENT_MESSAGE_TOOL_NAME => {
                let response = message_responses.get(&call.message_index)?;
                let message_id = tickloop_harness_transcript::input_ids::family_message(
                    snapshot.id.into(),
                    (*response).into(),
                    &call.call_id,
                );
                let target = self.accepted.get(&message_id)?;
                Some(ToolResult {
                    content: json!({"status":"accepted","message_id":ids.model_text(tickloop_harness_protocol::AgentMessageId::from(message_id)),"target_chat_id":ids.model_text(target.chat_id),
                        "target_run_id":ids.model_text(target.run_id),
                        "delivery":"accepted_before_interruption"})
                    .to_string(),
                    is_error: false,
                    output_ref: None,
                })
            }
            "TaskCreate" | "TaskUpdate" => {
                let mutation_id = input["mutation_id"].as_str()?.parse::<Uuid>().ok()?;
                let (root, loop_id) = snapshot
                    .delegation
                    .as_ref()
                    .map_or((snapshot.id, snapshot.chat_loop.id), |delegation| {
                        (delegation.root_chat_id(), delegation.root_loop_id())
                    });
                let board = self.task_boards.get(&(root, loop_id))?;
                let mutation = board.mutations.get(&mutation_id)?;
                // The receipt holds the input as the call was prepared, the Transcript as the
                // model wrote it.
                let prepared = if name == "TaskUpdate" {
                    crate::shared_tasks::prepared_update_input(input.clone())
                } else {
                    input.clone()
                };
                if mutation.actor != snapshot.id || mutation.input != format!("{name}:{prepared}") {
                    return None;
                }
                Some(crate::shared_tasks::reconciled_mutation_result(
                    ids, board, mutation,
                ))
            }
            _ => None,
        }
    }
}

/// The calls of `messages` whose effect stays inside the family: a delegation, a message, a
/// subtree stop or a Task mutation.
pub(crate) fn family_call_ids(messages: &[Message]) -> impl Iterator<Item = String> + '_ {
    messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|content| match content {
            Content::ToolUse { id, name, .. }
                if [
                    SUBAGENT_TOOL_NAME,
                    SEND_AGENT_MESSAGE_TOOL_NAME,
                    crate::agent_messages::STOP_SUBAGENT_TOOL_NAME,
                    "TaskCreate",
                    "TaskUpdate",
                ]
                .contains(&name.as_str()) =>
            {
                Some(id.clone())
            }
            _ => None,
        })
}

/// A member a parent registered whose Transcript the load did not find.
pub(crate) struct LostMember {
    parent: ChatId,
    registration: SubagentRegistration,
}

impl Agent {
    /// Brings the family of `member` into this instance, every member at once, unless it is
    /// here already; then the family goes on with its work.
    pub(crate) async fn resume_family(&self, member: ChatId) -> Result<(), AgentError> {
        if self.inner.chats.lock().await.contains_key(&member) {
            return Ok(());
        }
        let recorder = self.inner.recorder.as_ref().ok_or(AgentError::NotFound)?;
        let _resume = recorder.resume_lock.lock().await;
        if self.inner.chats.lock().await.contains_key(&member) {
            return Ok(());
        }
        let mut loaded = HashMap::new();
        let mut current = member;
        let root = loop {
            if loaded.contains_key(&current)
                || loaded.len() > tickloop_harness_protocol::MAX_SUBAGENT_DEPTH
            {
                return Err(AgentError::Invalid(
                    "invalid or excessive delegation ancestry".into(),
                ));
            }
            if self.inner.chats.lock().await.contains_key(&current) {
                return Err(AgentError::Invalid(
                    "a family is loaded whole: part of it is already here".into(),
                ));
            }
            let transcript = self.load_saved_transcript(current).await?;
            let parent = transcript
                .metadata
                .delegation
                .as_ref()
                .map(|entry| entry.delegation.parent_chat_id);
            loaded.insert(current, transcript);
            match parent {
                Some(parent) => current = parent,
                None => break current,
            }
        };
        let mut order = vec![root];
        let mut lineages = HashMap::from([(root, Vec::new())]);
        let mut lost = Vec::new();
        let mut pending = VecDeque::from([root]);
        while let Some(parent) = pending.pop_front() {
            let registrations = loaded[&parent]
                .metadata
                .subagents
                .as_ref()
                .filter(|entry| entry.chat_id == parent)
                .map(|entry| entry.subagents.clone())
                .unwrap_or_default();
            for registration in registrations {
                let child = registration.child_chat_id;
                if lineages.contains_key(&child) {
                    return Err(AgentError::Invalid("cyclic family registry".into()));
                }
                if let std::collections::hash_map::Entry::Vacant(entry) = loaded.entry(child) {
                    match self.load_saved_transcript(child).await {
                        Ok(transcript) => {
                            entry.insert(transcript);
                        }
                        Err(AgentError::NotFound) => {
                            lost.push(LostMember {
                                parent,
                                registration,
                            });
                            continue;
                        }
                        Err(error) => return Err(error),
                    }
                }
                let delegation = loaded[&child]
                    .metadata
                    .delegation
                    .as_ref()
                    .map(|entry| &entry.delegation)
                    .filter(|delegation| delegation.parent_chat_id == parent)
                    .ok_or_else(|| {
                        AgentError::Invalid(
                            "a registered member names another parent than the one that \
                             registered it"
                                .into(),
                        )
                    })?;
                let mut lineage = lineages[&parent].clone();
                lineage.push(tickloop_harness_protocol::DelegationParent {
                    chat_id: parent,
                    loop_id: delegation.parent_loop_id,
                });
                if lineage.len() > tickloop_harness_protocol::MAX_SUBAGENT_DEPTH
                    || lineage != delegation.lineage()
                {
                    return Err(AgentError::Invalid(
                        "persisted delegation ancestry is inconsistent".into(),
                    ));
                }
                lineages.insert(child, lineage);
                order.push(child);
                pending.push_back(child);
            }
        }
        if !lineages.contains_key(&member) {
            return Err(AgentError::Invalid(
                "the Chat is not a registered member of its family".into(),
            ));
        }
        let records = FamilyRecords::of(&loaded);
        for id in &order {
            let transcript = loaded.remove(id).expect("every member in order was loaded");
            // Boxed: restoring a Chat is a large future, and the family restores it in a loop.
            Box::pin(self.insert_restored_chat(
                *id,
                transcript,
                RestoreTarget::Resume,
                Some(&records),
            ))
            .await?;
        }
        self.restore_family_after_load(&order, lost).await;
        Ok(())
    }

    /// After a durable load of the family whose members are `order`, root first: each member's
    /// lost answers go on and its queue comes back; suspended Runs wait again, a foreground call
    /// before its child goes on; the ends the load decided reach the parents; then the members
    /// start, the deepest first so a waiting member finds its children holding their slots.
    pub(crate) async fn restore_family_after_load(&self, order: &[ChatId], lost: Vec<LostMember>) {
        for id in order {
            self.queue_after_load(*id).await;
        }
        let mut rebuilt = Vec::new();
        for id in order {
            rebuilt.extend(self.register_suspended_runs(*id).await);
        }
        for member in lost {
            self.settle_lost_member(member).await;
        }
        self.restore_task_claims(order).await;
        for id in order {
            self.report_ends_decided_at_load(*id).await;
        }
        for id in order.iter().rev() {
            if let Ok(Some(next)) = self.next_run(*id).await {
                self.run_task(next);
            }
        }
        self.start_rebuilt_runs(rebuilt);
        for id in order {
            self.flush_subagent_notifications(*id).await;
        }
    }

    /// The child Loops of a delegated member whose input settled for good while the family
    /// loaded, such as one that lost its host three times, or whose end no completion records
    /// yet: each reports its end to its parent, which a foreground call returns.
    async fn report_ends_decided_at_load(&self, chat_id: ChatId) {
        let ended = {
            let chats = self.inner.chats.lock().await;
            let Some(chat) = chats.get(&chat_id) else {
                return;
            };
            let Some(delegation) = &chat.snapshot.delegation else {
                return;
            };
            let Some(parent) = chats.get(&delegation.parent_chat_id) else {
                return;
            };
            let is_live = |run: RunId| {
                chat.active_run == Some(run)
                    || chat.parked.contains_key(&run)
                    || chat.ready_runs.contains(&run)
                    || chat
                        .snapshot
                        .pending_inputs
                        .iter()
                        .any(|pending| pending.receipt.run_id == run)
                    || chat.bookkeeping.lifecycle.open_input(run).is_some()
            };
            let chat_loop = &chat.snapshot.chat_loop;
            chat_loop
                .run_id
                .filter(|run| {
                    !is_live(*run)
                        && crate::agent_messages::child_run_status(&chat.snapshot, *run).is_some()
                        && !parent.snapshot.family_completions.iter().any(|completion| {
                            completion.child_chat_id == chat_id && completion.child_run_id == *run
                        })
                })
                .map(|run_id| Receipt {
                    chat_id,
                    loop_id: chat_loop.id,
                    run_id,
                })
        };
        // The family's notifications go out once every member is on its way.
        if let Some(receipt) = ended {
            let Ok(snapshot) = self.snapshot(chat_id).await else {
                return;
            };
            self.record_completion(&receipt, &snapshot).await;
        }
    }

    /// A member registered without a Transcript was lost before it started: a foreground call
    /// that waits on it returns that it failed, so its parent does not wait for ever.
    async fn settle_lost_member(&self, member: LostMember) {
        // It registered no children of its own.
        self.inner
            .subagent_registry
            .lock()
            .await
            .entry(member.registration.child_chat_id)
            .or_default();
        let mut chats = self.inner.chats.lock().await;
        let Some(parent) = chats.get_mut(&member.parent) else {
            return;
        };
        let child_chat = member.registration.child_chat_id;
        let waits = parent
            .bookkeeping
            .lifecycle
            .suspended_waits()
            .into_iter()
            .filter_map(|wait| match wait.kind {
                WaitKind::Subagent {
                    child_chat: waited,
                    child_loop,
                    child_run,
                    ..
                } if waited == child_chat => Some((wait.wait_id, child_loop, child_run)),
                _ => None,
            })
            .collect::<Vec<_>>();
        for (wait_id, child_loop, child_run) in waits {
            if parent
                .snapshot
                .family_completions
                .iter()
                .any(|completion| completion.child_run_id == child_run)
            {
                continue;
            }
            let completion =
                lost_completion(&member.registration, child_loop.into(), child_run.into());
            let registration = &member.registration;
            let tool = ToolResult {
                content: json!({"child_chat_id":parent.ids.model_text(ChatId::from(child_chat)),"handle":registration.handle,
                    "description":registration.description,"created_at":registration.created_at,
                    "child_loop_id":parent.ids.model_text(LoopId::from(child_loop)),"child_run_id":parent.ids.model_text(RunId::from(child_run)),"parent_chat_id":parent.ids.model_text(ChatId::from(member.parent)),
                    "status":"failed","child_state":"failed","result":"","error":LOST_BEFORE_IT_STARTED})
                .to_string(),
                is_error: true,
                output_ref: None,
            };
            parent.snapshot.family_completions.push(completion);
            parent.bookkeeping.lifecycle.resolve_wait(
                wait_id,
                ModelVisibleResult {
                    content: tool.content.clone().into(),
                    tool,
                    ends_run: false,
                },
                None,
            );
            if let Some(recorder) = &self.inner.recorder {
                recorder.record_family_completions(
                    member.parent,
                    parent.snapshot.family_completions.clone(),
                );
            }
            self.save(&parent.snapshot);
        }
    }
}

fn lost_completion(
    registration: &SubagentRegistration,
    child_loop: LoopId,
    child_run: RunId,
) -> FamilyCompletion {
    FamilyCompletion {
        child_chat_id: registration.child_chat_id,
        child_loop_id: child_loop,
        child_run_id: child_run,
        handle: registration.handle.clone(),
        state: tickloop_harness_protocol::LoopState::Failed,
        result: String::new(),
        error: Some(LOST_BEFORE_IT_STARTED.into()),
        started_at: None,
        finished_at: tickloop_harness_base::private_fs::unix_time_milliseconds(),
        triggering_message_ids: Vec::new(),
        notification: SubagentNotificationDelivery::Foreground,
        usage: Default::default(),
        total_tokens: None,
        run_status: Some(tickloop_harness_protocol::ChildRunStatus::Failed),
    }
}

use std::collections::{HashMap, HashSet};

use tickloop_harness_protocol::{ChatId, DelegationParent, MAX_SUBAGENT_DEPTH};

use crate::Chat;
use crate::{Agent, AgentError};
use tickloop_harness_protocol::{CommandState, OperationRecord, Receipt, UserShellState};
use tokio_util::sync::CancellationToken;

#[derive(serde::Serialize)]
pub(super) struct SubagentStopFailure {
    chat_id: ChatId,
    operation_id: Option<uuid::Uuid>,
    stage: &'static str,
    error: String,
}

pub(super) struct SubagentStopResult {
    pub operations: Vec<OperationRecord>,
    pub failures: Vec<SubagentStopFailure>,
}

pub(super) fn lineage(
    chats: &HashMap<ChatId, Chat>,
    member: ChatId,
) -> Result<Vec<DelegationParent>, String> {
    let mut visited = HashSet::new();
    let mut current = member;
    let mut parents = Vec::new();
    let mut metadata = Vec::new();
    loop {
        if !visited.insert(current) {
            return Err("delegation ancestry contains a cycle".into());
        }
        let chat = chats
            .get(&current)
            .ok_or("ancestor authorization is unavailable")?;
        let Some(delegation) = &chat.snapshot.delegation else {
            break;
        };
        if parents.len() == MAX_SUBAGENT_DEPTH {
            return Err("delegation exceeds the maximum depth".into());
        }
        parents.push(DelegationParent {
            chat_id: delegation.parent_chat_id,
            loop_id: delegation.parent_loop_id,
        });
        metadata.push(delegation);
        current = delegation.parent_chat_id;
    }
    parents.reverse();
    for (index, delegation) in metadata.iter().enumerate() {
        if delegation.lineage() != parents[..parents.len() - index] {
            return Err("persisted delegation ancestry does not match its parents".into());
        }
    }
    Ok(parents)
}

pub(super) fn root(chats: &HashMap<ChatId, Chat>, member: ChatId) -> Result<ChatId, String> {
    Ok(lineage(chats, member)?
        .first()
        .map_or(member, |parent| parent.chat_id))
}

pub(super) fn descendants(
    chats: &HashMap<ChatId, Chat>,
    ancestor: ChatId,
) -> Result<Vec<ChatId>, String> {
    let mut descendants = Vec::new();
    for (id, chat) in chats {
        if *id == ancestor || chat.snapshot.delegation.is_none() {
            continue;
        }
        if lineage(chats, *id)?
            .iter()
            .any(|parent| parent.chat_id == ancestor)
        {
            descendants.push(*id);
        }
    }
    descendants.sort();
    Ok(descendants)
}

type StopAdmission<'a> = (&'a [(ChatId, u64)], &'a Receipt, &'a CancellationToken);

impl Agent {
    pub(super) async fn stop_subagent_tree(
        &self,
        target: ChatId,
        admission: Option<StopAdmission<'_>>,
    ) -> Result<SubagentStopResult, AgentError> {
        let members = {
            let _creation = self.inner.subagent_invocations.lock().await;
            let mut mailbox = self.inner.family_mailbox.lock().await;
            let mut chats = self.inner.chats.lock().await;
            let mut ids = descendants(&chats, target).map_err(AgentError::Invalid)?;
            ids.push(target);
            ids.sort();
            let target_chat = chats.get(&target).ok_or(AgentError::NotFound)?;
            let parent = target_chat
                .snapshot
                .delegation
                .as_ref()
                .map(|delegation| delegation.parent_chat_id);
            if let Some((expected, sender, cancel)) = admission {
                let origin = chats.get(&sender.chat_id).ok_or(AgentError::NotFound)?;
                if cancel.is_cancelled()
                    || origin.active_run != Some(sender.run_id)
                    || (parent != Some(sender.chat_id)
                        && root(&chats, target).map_err(AgentError::Invalid)? != sender.chat_id)
                    || ids
                        .iter()
                        .map(|id| (*id, chats[id].family_generation))
                        .collect::<Vec<_>>()
                        != expected
                {
                    return Err(AgentError::Invalid(
                        "prepared Subagent subtree or caller changed".into(),
                    ));
                }
            }
            // Late completion delivery still belongs to this stop after the call returns.
            let stopped_chats = ids.iter().copied().collect::<HashSet<_>>();
            let stopped_runs = ids
                .iter()
                .flat_map(|id| {
                    let chat = &chats[id];
                    chat.active_run
                        .into_iter()
                        .chain(chat.parked.keys().copied())
                        .chain(chat.family_finishing.iter().copied())
                        .chain(chat.snapshot.chat_loop.run_id)
                        .chain(
                            chat.snapshot
                                .family_completions
                                .iter()
                                .filter(|completion| {
                                    stopped_chats.contains(&completion.child_chat_id)
                                })
                                .map(|completion| completion.child_run_id),
                        )
                })
                .collect::<HashSet<_>>();
            let mut members = Vec::new();
            for id in ids {
                let child = chats.get_mut(&id).ok_or(AgentError::NotFound)?;
                child.family_stopping = true;
                child.family_generation = child.family_generation.saturating_add(1);
                if let Some(cancel) = &child.active {
                    cancel.cancel();
                }
                for run in child.parked.values() {
                    run.cancel.cancel();
                }
                child.hook_state.cancel_background();
                mailbox
                    .stopped_runs
                    .entry(id)
                    .or_default()
                    .extend(&stopped_runs);
                for pending in &mut child.snapshot.pending_inputs {
                    if pending
                        .agent_message
                        .as_ref()
                        .is_some_and(|source| stopped_chats.contains(&source.sender_chat_id))
                    {
                        pending.armed = false;
                        child
                            .bookkeeping
                            .lifecycle
                            .set_queued_armed(pending.receipt.run_id, false);
                    }
                }
                self.persist_family_message_hold(child, &mailbox);
                child.snapshot.pending_commands.clear();
                child.ready_runs.clear();
                let commands = child
                    .snapshot
                    .commands
                    .iter()
                    .filter(|command| command.state == CommandState::Running)
                    .map(|command| command.operation_id)
                    .collect::<Vec<_>>();
                let shells = child
                    .snapshot
                    .user_shells
                    .iter_mut()
                    .filter(|shell| {
                        matches!(
                            shell.state,
                            UserShellState::Running | UserShellState::Cancelling
                        )
                    })
                    .map(|shell| {
                        shell.armed = false;
                        shell.id
                    })
                    .collect::<Vec<_>>();
                self.save(&child.snapshot);
                members.push((id, commands, shells));
            }
            members
        };
        #[cfg(any(test, feature = "test-support"))]
        self.wait_family_pause(target, crate::family_pauses::Point::StopAdmitted)
            .await;
        let mut unknown = Vec::new();
        let mut errors = Vec::new();
        for (id, commands, shells) in &members {
            match self.stop_loop(*id).await {
                Ok(operations) => unknown.extend(operations),
                Err(error) => errors.push(SubagentStopFailure {
                    chat_id: *id,
                    operation_id: None,
                    stage: "loop",
                    error: error.to_string(),
                }),
            }
            for command in commands {
                if let Err(error) = self.stop_command(*id, *command).await {
                    errors.push(SubagentStopFailure {
                        chat_id: *id,
                        operation_id: Some(*command),
                        stage: "command",
                        error: error.to_string(),
                    });
                }
            }
            for shell in shells {
                if let Err(error) = self.stop_user_shell(*id, *shell).await {
                    errors.push(SubagentStopFailure {
                        chat_id: *id,
                        operation_id: Some(*shell),
                        stage: "shell",
                        error: error.to_string(),
                    });
                }
            }
            let settled = {
                let chats = self.inner.chats.lock().await;
                let child = &chats[id];
                let chat_loop = &child.snapshot.chat_loop;
                chat_loop
                    .run_id
                    .filter(|run_id| {
                        child.active_run != Some(*run_id)
                            && !child.parked.contains_key(run_id)
                            && !child.family_finishing.contains(run_id)
                    })
                    .map(|run_id| {
                        (
                            Receipt {
                                chat_id: *id,
                                loop_id: chat_loop.id,
                                run_id,
                            },
                            child.snapshot.clone(),
                        )
                    })
            };
            if let Some((receipt, snapshot)) = settled {
                self.record_family_completion(&receipt, &snapshot).await;
            }
        }
        {
            let mut chats = self.inner.chats.lock().await;
            for (id, ..) in &members {
                if let Some(child) = chats.get_mut(id) {
                    child.family_stopping = false;
                }
            }
        }
        for (id, ..) in members {
            let agent = self.clone();
            self.inner.tasks.spawn(async move {
                let _ = agent.start_next_agent_run(id).await;
            });
        }
        Ok(SubagentStopResult {
            operations: unknown,
            failures: errors,
        })
    }

    /// Waits until every child the member's Loop delegated has reported, without polling the
    /// model: `Ok(true)` when it waited. The wait suspends the Run where it waits, at the call
    /// `at_call` or else before its next model request; a release ends it for another instance
    /// to go on with.
    pub(super) async fn wait_for_subagents(
        &self,
        receipt: &Receipt,
        cancel: &CancellationToken,
        at_call: Option<&str>,
    ) -> Result<bool, String> {
        let mut events = self.subscribe();
        let mut waited = false;
        let mut release = None;
        loop {
            let foreground = self
                .inner
                .subagent_invocations
                .lock()
                .await
                .values()
                .filter(|invocation| !invocation.run_in_background)
                .map(|invocation| invocation.receipt.run_id)
                .collect::<HashSet<_>>();
            let (pending, children, parent_cancel) = {
                let chats = self.inner.chats.lock().await;
                let parent = chats.get(&receipt.chat_id).ok_or("parent is unavailable")?;
                if parent.snapshot.delegation.is_none() {
                    return Ok(false);
                }
                if parent
                    .active
                    .as_ref()
                    .is_some_and(CancellationToken::is_cancelled)
                    || parent.family_stopping
                {
                    return Err("delegating run was interrupted".into());
                }
                let mut pending = false;
                let mut children = Vec::new();
                for child in chats.values().filter(|child| {
                    child
                        .snapshot
                        .delegation
                        .as_ref()
                        .is_some_and(|delegation| {
                            delegation.parent_chat_id == receipt.chat_id
                                && delegation.parent_loop_id == receipt.loop_id
                        })
                }) {
                    children.push(child.snapshot.id);
                    let live = child.active_run.is_some() || !child.parked.is_empty();
                    pending |= live;
                    if let Some(run) = child
                        .snapshot
                        .chat_loop
                        .run_id
                        .filter(|run| !foreground.contains(run))
                    {
                        let is_reported = parent
                            .snapshot
                            .family_completions
                            .iter()
                            .find(|completion| {
                                completion.child_chat_id == child.snapshot.id
                                    && completion.child_run_id == run
                            })
                            .is_some_and(|completion| {
                                !matches!(
                                    completion.notification,
                                    tickloop_harness_protocol::SubagentNotificationDelivery::Pending
                                )
                            });
                        if !is_reported {
                            if !live && !child.family_finishing.contains(&run) {
                                return Err(format!(
                                    "Subagent {} has an interrupted or unknown run without a live executor; explicitly continue or stop that child before reporting completion",
                                    parent.ids.model_text(child.snapshot.id)
                                ));
                            }
                            pending = true;
                        }
                    }
                }
                children.sort();
                (
                    pending,
                    children,
                    parent.active.clone().unwrap_or_else(|| cancel.clone()),
                )
            };
            if !pending {
                self.end_children_wait(receipt).await;
                return Ok(waited);
            }
            waited = true;
            let release = match &release {
                Some(release) => release,
                None => release.insert(self.suspend_for_children(receipt, children, at_call).await),
            };
            tokio::select! {
                biased;
                _ = release.cancelled() => return Err(super::suspension::RUN_RELEASED.into()),
                _ = cancel.cancelled() => return Err("waiting for Subagents was interrupted".into()),
                _ = parent_cancel.cancelled() => return Err("delegating run was interrupted".into()),
                event = events.recv() => match event {
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {},
                    Err(_) => return Err("Subagent event stream closed".into()),
                }
            }
        }
    }
}

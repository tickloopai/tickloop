use std::collections::{HashMap, HashSet};

use tickloop_agent_protocol::{ChatId, DelegationParent, MAX_SUBAGENT_DEPTH};

use crate::Chat;
use crate::{Agent, AgentError};
use tickloop_agent_protocol::{CommandState, LoopState, OperationRecord, Receipt, UserShellState};
use tickloop_agent_transcript::{DiscardReason, SettledOutcome};
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
            let mut chats = self.inner.chats.lock().await;
            let mut ids = descendants(&chats, target).map_err(AgentError::Invalid)?;
            ids.push(target);
            ids.sort();
            let target_chat = chats.get(&target).ok_or(AgentError::NotFound)?;
            let parent = target_chat
                .snapshot
                .delegation
                .as_ref()
                .ok_or_else(|| AgentError::Invalid("a subtree stop requires a Subagent".into()))?
                .parent_chat_id;
            if let Some((expected, sender, cancel)) = admission {
                let origin = chats.get(&sender.chat_id).ok_or(AgentError::NotFound)?;
                if cancel.is_cancelled()
                    || origin.active_run != Some(sender.run_id)
                    || (parent != sender.chat_id
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
                child.snapshot.pending_commands.clear();
                let discarded = std::mem::take(&mut child.snapshot.pending_inputs);
                self.settle_removed_inputs(
                    child,
                    &discarded,
                    SettledOutcome::Discarded {
                        reason: DiscardReason::Stopped,
                    },
                );
                child.ready_runs.clear();
                let parent = child.snapshot.delegation.as_ref().unwrap().parent_chat_id;
                let loops = child
                    .snapshot
                    .loops
                    .iter()
                    .map(|goal| goal.id)
                    .collect::<Vec<_>>();
                let commands = child
                    .snapshot
                    .commands
                    .iter()
                    .filter(|command| {
                        matches!(
                            command.state,
                            CommandState::Running | CommandState::Background
                        )
                    })
                    .map(|command| command.operation_id)
                    .collect::<Vec<_>>();
                let shells = child
                    .snapshot
                    .user_shells
                    .iter_mut()
                    .filter(|shell| {
                        matches!(
                            shell.state,
                            UserShellState::Running
                                | UserShellState::Background
                                | UserShellState::Queued
                                | UserShellState::WaitingPermission
                                | UserShellState::Cancelling
                        )
                    })
                    .map(|shell| {
                        shell.armed = false;
                        shell.id
                    })
                    .collect::<Vec<_>>();
                self.save(&child.snapshot);
                members.push((id, parent, loops, commands, shells));
            }
            members
        };
        let mut unknown = Vec::new();
        let mut errors = Vec::new();
        for (id, parent, loops, commands, shells) in &members {
            for loop_id in loops {
                match self.end_goal(*id, *loop_id, LoopState::Stopped, None).await {
                    Ok(operations) => unknown.extend(operations),
                    Err(error) => errors.push(SubagentStopFailure {
                        chat_id: *id,
                        operation_id: Some(*loop_id),
                        stage: "loop",
                        error: error.to_string(),
                    }),
                }
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
                child
                    .snapshot
                    .loops
                    .iter()
                    .filter_map(|goal| {
                        let run_id = goal.run_id?;
                        (child.active_run != Some(run_id)
                            && !child.parked.contains_key(&run_id)
                            && !child.family_finishing.contains(&run_id)
                            && matches!(goal.state, LoopState::Stopped | LoopState::Expired))
                        .then(|| {
                            (
                                Receipt {
                                    chat_id: *id,
                                    loop_id: goal.id,
                                    run_id,
                                },
                                child.snapshot.clone(),
                            )
                        })
                    })
                    .collect::<Vec<_>>()
            };
            for (receipt, snapshot) in settled {
                self.record_family_completion(&receipt, &snapshot).await;
            }
            let hook = self.inner.stopped_child_hook.read().unwrap().clone();
            if let Some(hook) = hook
                && let Err(error) = hook.child_stopped(*parent, *id).await
            {
                errors.push(SubagentStopFailure {
                    chat_id: *id,
                    operation_id: None,
                    stage: "hosted_ticks",
                    error,
                });
            }
        }
        let mut chats = self.inner.chats.lock().await;
        for (id, ..) in members {
            if let Some(child) = chats.get_mut(&id) {
                child.family_stopping = false;
            }
        }
        Ok(SubagentStopResult {
            operations: unknown,
            failures: errors,
        })
    }

    pub(super) async fn wait_for_subagents(
        &self,
        receipt: &Receipt,
        cancel: &CancellationToken,
    ) -> Result<bool, String> {
        let mut events = self.subscribe();
        let mut waited = false;
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
            let (pending, parent_cancel) = {
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
                    let live = child.active_run.is_some() || !child.parked.is_empty();
                    pending |= live;
                    for run in child.snapshot.loops.iter().filter_map(|goal| goal.run_id) {
                        if foreground.contains(&run) {
                            continue;
                        }
                        let completion =
                            parent
                                .snapshot
                                .family_completions
                                .iter()
                                .find(|completion| {
                                    completion.child_chat_id == child.snapshot.id
                                        && completion.child_run_id == run
                                });
                        if completion.is_some_and(|completion| {
                            !matches!(
                                completion.notification,
                                tickloop_agent_protocol::SubagentNotificationDelivery::Pending
                            )
                        }) {
                            continue;
                        }
                        if !live && !child.family_finishing.contains(&run) {
                            return Err(format!(
                                "Subagent {} has an interrupted or unknown run without a live executor; explicitly continue or stop that child before reporting completion",
                                child.snapshot.id
                            ));
                        }
                        pending = true;
                    }
                }
                (
                    pending,
                    parent.active.clone().unwrap_or_else(|| cancel.clone()),
                )
            };
            if !pending {
                return Ok(waited);
            }
            waited = true;
            tokio::select! {
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

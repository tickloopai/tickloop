use super::*;
use tickloop_agent_tools::permissions::PermissionPolicyExt;

pub(crate) fn summary_state(
    loops: &[crate::runtime::LoopSnapshot],
    user_shells: &[crate::runtime::UserShellRecord],
) -> crate::runtime::LoopState {
    use crate::runtime::{LoopState, UserShellState};
    let any_loop = |state: LoopState| loops.iter().any(|goal| goal.state == state);
    let any_shell = |states: &[UserShellState]| {
        user_shells
            .iter()
            .any(|shell| states.contains(&shell.state))
    };
    if any_shell(&[UserShellState::WaitingPermission]) {
        return LoopState::WaitingPermission;
    }
    if let Some(state) = [
        LoopState::WaitingPermission,
        LoopState::WaitingQuestion,
        LoopState::WaitingPlan,
    ]
    .into_iter()
    .find(|state| any_loop(state.clone()))
    {
        return state;
    }
    if any_loop(LoopState::Running)
        || any_shell(&[
            UserShellState::Running,
            UserShellState::Background,
            UserShellState::Cancelling,
        ])
    {
        return LoopState::Running;
    }
    if any_loop(LoopState::Queued) || any_shell(&[UserShellState::Queued]) {
        return LoopState::Queued;
    }
    if any_loop(LoopState::WaitingTrigger) {
        return LoopState::WaitingTrigger;
    }
    if let Some(goal) = loops.last() {
        return goal.state.clone();
    }
    match user_shells.last().map(|shell| &shell.state) {
        Some(UserShellState::Failed) => LoopState::Failed,
        _ => LoopState::Idle,
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FamilyRole {
    Root,
    Child,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FamilyAvailability {
    Working,
    NeedsInput,
    Waiting,
    Idle,
    Stopped,
    Interrupted,
    Failed,
    Unloaded,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FamilyAgent {
    pub chat_id: ChatId,
    pub role: FamilyRole,
    pub handle: String,
    pub display_name: String,
    pub description: Option<String>,
    pub availability: FamilyAvailability,
    pub run_state: Option<LoopState>,
    pub created_at: Option<u64>,
    pub run_started_at: Option<u64>,
    pub idle_since: Option<u64>,
}

impl Agent {
    pub async fn family_agents(&self, member: ChatId) -> Result<Vec<FamilyAgent>, AgentError> {
        let snapshot = self.snapshot(member).await?;
        let root = snapshot
            .delegation
            .as_ref()
            .map_or(member, |delegation| delegation.parent_chat_id);
        let registrations = self.subagents(root).await?;
        let chats = self.inner.chats.lock().await;
        if chats
            .get(&root)
            .is_some_and(|chat| chat.snapshot.delegation.is_some())
        {
            return Err(AgentError::Invalid(
                "nested families are unsupported".into(),
            ));
        }
        let mut members = vec![(root, None)];
        members.extend(
            registrations
                .iter()
                .map(|registration| (registration.child_chat_id, Some(registration))),
        );
        Ok(members
            .into_iter()
            .map(|(id, registration)| {
                let chat = chats.get(&id);
                let state = chat
                    .map(|chat| summary_state(&chat.snapshot.loops, &chat.snapshot.user_shells));
                let goal = chat.and_then(|chat| {
                    chat.snapshot
                        .loops
                        .iter()
                        .rev()
                        .find(|goal| Some(&goal.state) == state.as_ref())
                });
                let shell_started_at = chat.and_then(|chat| {
                    chat.snapshot.user_shells.iter().rev().find_map(|shell| {
                        let matches_state = match state.as_ref() {
                            Some(LoopState::Running) => matches!(
                                shell.state,
                                UserShellState::Running
                                    | UserShellState::Background
                                    | UserShellState::Cancelling
                            ),
                            Some(LoopState::Queued) => shell.state == UserShellState::Queued,
                            _ => false,
                        };
                        matches_state.then_some(shell.state_since).flatten()
                    })
                });
                let availability = match state.as_ref() {
                    None if chat.is_none() => FamilyAvailability::Unloaded,
                    Some(LoopState::Failed) => FamilyAvailability::Failed,
                    Some(LoopState::Stopped | LoopState::Expired) => FamilyAvailability::Stopped,
                    Some(LoopState::Interrupted) => FamilyAvailability::Interrupted,
                    Some(
                        LoopState::WaitingPermission
                        | LoopState::WaitingQuestion
                        | LoopState::WaitingPlan,
                    ) => FamilyAvailability::NeedsInput,
                    Some(LoopState::WaitingTrigger) => FamilyAvailability::Waiting,
                    Some(LoopState::Running | LoopState::Queued) => FamilyAvailability::Working,
                    _ => FamilyAvailability::Idle,
                };
                let display_name = registration
                    .map(|registration| registration.description.clone())
                    .unwrap_or_else(|| {
                        chat.map(|chat| {
                            crate::runtime::chat_display_title(
                                &chat.snapshot,
                                &chat.snapshot.user_shells,
                            )
                        })
                        .unwrap_or_else(|| "Family root".into())
                    });
                FamilyAgent {
                    chat_id: id,
                    role: if registration.is_some() {
                        FamilyRole::Child
                    } else {
                        FamilyRole::Root
                    },
                    handle: registration
                        .map_or("main", |registration| registration.handle.as_str())
                        .into(),
                    display_name,
                    description: registration.map(|registration| registration.description.clone()),
                    availability,
                    run_state: state,
                    created_at: registration.map(|registration| registration.created_at),
                    run_started_at: goal
                        .and_then(|goal| goal.run_started_at)
                        .or(shell_started_at),
                    idle_since: goal
                        .filter(|goal| {
                            matches!(
                                goal.state,
                                LoopState::Completed | LoopState::Idle | LoopState::WaitingTrigger
                            )
                        })
                        .and_then(|goal| goal.state_since),
                }
            })
            .collect())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentNotificationDelivery {
    Foreground,
    Pending,
    Queued,
    Rejected(FamilyMessageRejection),
}

impl SubagentNotificationDelivery {
    pub fn is_background(&self) -> bool {
        !matches!(self, Self::Foreground)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FamilyCompletion {
    pub child_chat_id: ChatId,
    pub child_loop_id: LoopId,
    pub child_run_id: RunId,
    pub handle: String,
    pub state: LoopState,
    pub result: String,
    pub error: Option<String>,
    pub started_at: Option<u64>,
    pub finished_at: u64,
    pub triggering_message_ids: Vec<Uuid>,
    pub notification: SubagentNotificationDelivery,
    pub usage: tickloop_agent_protocol::Usage,
}

impl Agent {
    pub(super) fn flush_subagent_notifications(
        &self,
        parent: ChatId,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            let pending = match self.snapshot(parent).await {
                Ok(snapshot) => snapshot
                    .family_completions
                    .into_iter()
                    .filter(|completion| {
                        matches!(
                            completion.notification,
                            SubagentNotificationDelivery::Pending
                        )
                    })
                    .collect::<Vec<_>>(),
                Err(_) => return,
            };
            for completion in pending {
                let child = Receipt {
                    chat_id: completion.child_chat_id,
                    loop_id: completion.child_loop_id,
                    run_id: completion.child_run_id,
                };
                let Ok(snapshot) = self.snapshot(child.chat_id).await else {
                    continue;
                };
                let Some(delegation) = snapshot.delegation else {
                    continue;
                };
                let target = Receipt {
                    chat_id: parent,
                    loop_id: delegation.parent_loop_id,
                    run_id: delegation.parent_run_id,
                };
                let outcome = self
                    .deliver_subagent_notification(&child, &target, &completion)
                    .await;
                let delivery = match outcome {
                    Ok(result) if !result.is_error => SubagentNotificationDelivery::Queued,
                    Ok(result) => {
                        let rejection = serde_json::from_str::<serde_json::Value>(&result.content)
                            .ok()
                            .and_then(|value| {
                                serde_json::from_value::<FamilyMessageRejection>(
                                    value["rejection"].clone(),
                                )
                                .ok()
                            })
                            .unwrap_or(FamilyMessageRejection {
                                code: FamilyMessageRejectionCode::StaleTarget,
                                message: "completion delivery rejected".into(),
                            });
                        if matches!(rejection.code, FamilyMessageRejectionCode::InboxFull) {
                            break;
                        }
                        SubagentNotificationDelivery::Rejected(rejection)
                    }
                    Err(message) => {
                        SubagentNotificationDelivery::Rejected(FamilyMessageRejection {
                            code: FamilyMessageRejectionCode::StaleTarget,
                            message,
                        })
                    }
                };
                let mut chats = self.inner.chats.lock().await;
                let Some(chat) = chats.get_mut(&parent) else {
                    return;
                };
                if let Some(record) = chat
                    .snapshot
                    .family_completions
                    .iter_mut()
                    .find(|record| record.child_run_id == completion.child_run_id)
                {
                    record.notification = delivery;
                    if matches!(
                        record.notification,
                        SubagentNotificationDelivery::Rejected(_)
                    ) {
                        let updated = record.clone();
                        self.emit(
                            chat,
                            &target,
                            EventKind::FamilyRunCompleted {
                                completion: updated,
                            },
                        );
                    }
                }
                if let Some(recorder) = &self.inner.recorder {
                    recorder.record_family_completions(
                        parent,
                        chat.snapshot.family_completions.clone(),
                    );
                }
                self.save(&chat.snapshot);
            }
        })
    }

    pub(super) async fn record_family_completion(&self, child: &Receipt, snapshot: &ChatSnapshot) {
        let Some(delegation) = snapshot.delegation.as_ref() else {
            return;
        };
        let Ok(Some(registration)) = self.subagent_registration(child.chat_id).await else {
            return;
        };
        let Some(goal) = snapshot.loops.iter().find(|goal| goal.id == child.loop_id) else {
            return;
        };
        let result = snapshot.run_response_text(child.run_id).unwrap_or_default();
        let error = snapshot.events.iter().rev().find_map(|event| {
            if event.run_id == child.run_id
                && let EventKind::Failed(error) = &event.kind
            {
                Some(error.clone())
            } else {
                None
            }
        });
        let background = self
            .inner
            .subagent_invocations
            .lock()
            .await
            .values()
            .find(|invocation| invocation.receipt.run_id == child.run_id)
            .map_or_else(
                || {
                    snapshot
                        .messages
                        .iter()
                        .enumerate()
                        .find(|(index, message)| {
                            message.role == Role::User
                                && snapshot.message_runs.get(index) == Some(&child.run_id)
                        })
                        .is_none_or(|(index, _)| {
                            !snapshot.prompt_execution_modes.contains_key(&index)
                        })
                },
                |invocation| invocation.run_in_background,
            );
        let completion = FamilyCompletion {
            notification: if background {
                SubagentNotificationDelivery::Pending
            } else {
                SubagentNotificationDelivery::Foreground
            },
            usage: snapshot
                .request_usage
                .run_conversation_usage(child.run_id)
                .unwrap_or_default(),
            child_chat_id: child.chat_id,
            child_loop_id: child.loop_id,
            child_run_id: child.run_id,
            handle: registration.handle,
            state: goal.state.clone(),
            result,
            error,
            started_at: goal.run_started_at,
            finished_at: tickloop_agent_base::private_fs::unix_time_milliseconds(),
            triggering_message_ids: snapshot
                .message_runs
                .iter()
                .filter(|(_, run)| **run == child.run_id)
                .filter_map(|(index, _)| snapshot.message_ids.get(index).copied())
                .collect(),
        };
        {
            let mut chats = self.inner.chats.lock().await;
            let Some(parent) = chats.get_mut(&delegation.parent_chat_id) else {
                return;
            };
            if parent.snapshot.family_completions.iter().any(|record| {
                record.child_chat_id == child.chat_id && record.child_run_id == child.run_id
            }) {
                return;
            }
            {
                parent.snapshot.family_completions.push(completion.clone());
                if let Some(recorder) = &self.inner.recorder {
                    recorder.record_family_completions(
                        parent.snapshot.id,
                        parent.snapshot.family_completions.clone(),
                    );
                }
                let parent_receipt = Receipt {
                    chat_id: delegation.parent_chat_id,
                    loop_id: delegation.parent_loop_id,
                    run_id: delegation.parent_run_id,
                };
                self.emit(
                    parent,
                    &parent_receipt,
                    EventKind::FamilyRunCompleted {
                        completion: completion.clone(),
                    },
                );
                self.save(&parent.snapshot);
            }
        }
        if completion.notification.is_background() {
            self.flush_subagent_notifications(delegation.parent_chat_id)
                .await;
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DirectedHumanSource {
    pub message_id: Uuid,
    pub source_chat_id: ChatId,
    pub source_handle: String,
    pub original_submission: String,
    pub original_contexts: Vec<InputContext>,
    pub original_paths: Vec<PathBuf>,
    pub source_session_id: Uuid,
    pub source_view_id: Uuid,
    pub source_display_version: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FamilyInputRecord {
    pub message_id: Uuid,
    pub target_chat_id: ChatId,
    pub target_handle: String,
    pub target_run_id: RunId,
    pub submitted_at: u64,
    pub response: Option<String>,
    pub state: Option<LoopState>,
}

pub(super) struct AttachmentAdmission {
    pub workspace: tickloop_agent_tools::Workspace,
    pub reads: Vec<tickloop_agent_tools::PreparedTool>,
}

impl Agent {
    pub(super) fn validate_attachment_admission(
        chats: &HashMap<ChatId, Chat>,
        sessions: &session_permissions::SessionPermissionContext,
        chat_id: ChatId,
        goal: Option<LoopId>,
        admission: &AttachmentAdmission,
    ) -> Result<(), AgentError> {
        let snapshot = &chats.get(&chat_id).ok_or(AgentError::NotFound)?.snapshot;
        let workspace = tickloop_agent_tools::Workspace {
            workdir: snapshot.workdir.clone(),
            additional_directories: snapshot.additional_directories.clone(),
        };
        if workspace.workdir != admission.workspace.workdir
            || workspace.additional_directories != admission.workspace.additional_directories
        {
            return Err(AgentError::Invalid(
                "recipient workspace changed before attachment admission".into(),
            ));
        }
        let loop_id = goal.unwrap_or(Uuid::nil());
        let receipt = Receipt {
            chat_id,
            loop_id,
            run_id: Uuid::nil(),
        };
        let owner = goal
            .map(|id| PermissionOwner::new(chat_id, id))
            .transpose()
            .map_err(|error| AgentError::Invalid(error.to_string()))?;
        for prepared in &admission.reads {
            if !prepared.in_workspace {
                return Err(AgentError::Invalid(
                    "referenced path is outside the configured workspace".into(),
                ));
            }
            delegation::check_call(
                chats,
                sessions,
                &receipt,
                &prepared.name,
                &prepared.input,
                Some(prepared),
                None,
            )
            .map_err(AgentError::Invalid)?;
            let decision = if let Some(owner) = owner {
                let operation = SessionPermissionOperation::from_prepared(prepared, &workspace)
                    .map_err(|error| AgentError::Invalid(error.to_string()))?;
                sessions.evaluate(owner, &operation).decision
            } else {
                None
            };
            if snapshot
                .policy
                .decide_with_session(prepared, &workspace, loop_id, decision)
                != Decision::Allow
            {
                return Err(AgentError::Invalid(
                    "referenced path is denied or requires separate recipient file approval".into(),
                ));
            }
        }
        Ok(())
    }
}

pub(super) struct ContextSubmission {
    pub chat_id: ChatId,
    pub goal: Option<LoopId>,
    pub input: String,
    pub contexts: Vec<InputContext>,
    pub paths: Vec<PathBuf>,
    pub host_context: Option<crate::host_tools::HostCallContext>,
    pub directed_source: Option<DirectedHumanSource>,
    pub target_generation: Option<u64>,
}

impl Agent {
    pub(crate) async fn submit_family_input(
        &self,
        target: ChatId,
        target_loop: Option<LoopId>,
        source: DirectedHumanSource,
        contexts: Vec<InputContext>,
        paths: Vec<PathBuf>,
        host_context: Option<crate::host_tools::HostCallContext>,
    ) -> Result<Receipt, AgentError> {
        let sender_snapshot = self.snapshot(source.source_chat_id).await?;
        let target_snapshot = self.snapshot(target).await?;
        let sender_root = sender_snapshot
            .delegation
            .as_ref()
            .map_or(sender_snapshot.id, |delegation| delegation.parent_chat_id);
        let target_root = target_snapshot
            .delegation
            .as_ref()
            .map_or(target_snapshot.id, |delegation| delegation.parent_chat_id);
        if sender_root != target_root || source.source_chat_id == target {
            return Err(AgentError::Invalid(
                "directed input targets another member of the same family".into(),
            ));
        }
        let handle = self
            .subagent_registration(target)
            .await?
            .map(|registration| registration.handle)
            .unwrap_or_else(|| "main".into());
        let original = source.original_submission.trim_start();
        let (token, body) = original.split_once(char::is_whitespace).ok_or_else(|| {
            AgentError::Invalid("directed input needs a target and nonempty body".into())
        })?;
        if token != format!("@{handle}") || body.trim().is_empty() {
            return Err(AgentError::Invalid(
                "directed target token changed or body is empty".into(),
            ));
        }
        let target_goal = target_loop.or_else(|| target_snapshot.default_input_loop());
        if target_snapshot
            .loops
            .iter()
            .find(|goal| Some(goal.id) == target_goal)
            .is_some_and(|goal| {
                matches!(
                    goal.state,
                    LoopState::Stopped
                        | LoopState::Interrupted
                        | LoopState::Failed
                        | LoopState::Expired
                )
            })
        {
            return Err(AgentError::Invalid(
                "target requires explicit human Continue".into(),
            ));
        }
        let body = body.to_owned();
        let target_generation = self
            .inner
            .chats
            .lock()
            .await
            .get(&target)
            .ok_or(AgentError::NotFound)?
            .family_generation;
        self.submit_contexts(ContextSubmission {
            chat_id: target,
            goal: target_goal,
            input: body,
            contexts,
            paths,
            host_context,
            directed_source: Some(source),
            target_generation: Some(target_generation),
        })
        .await
    }
}

impl Agent {
    pub(super) fn persist_family_inputs(&self, chat: &Chat) {
        if let Some(recorder) = &self.inner.recorder {
            recorder.record_family_inputs(chat.snapshot.id, chat.snapshot.family_inputs.clone());
        }
    }

    pub(super) fn record_family_delivery(
        &self,
        source_chat: &mut Chat,
        source: &DirectedHumanSource,
        receipt: &Receipt,
    ) {
        let record = FamilyInputRecord {
            message_id: source.message_id,
            target_chat_id: receipt.chat_id,
            target_handle: source
                .original_submission
                .split_whitespace()
                .next()
                .unwrap()
                .trim_start_matches('@')
                .into(),
            target_run_id: receipt.run_id,
            submitted_at: tickloop_agent_base::private_fs::unix_time_milliseconds(),
            response: None,
            state: None,
        };
        source_chat
            .snapshot
            .family_inputs
            .insert(source.message_id, record.clone());
        self.persist_family_inputs(source_chat);
        self.emit(
            source_chat,
            &notification_receipt(&source_chat.snapshot),
            EventKind::FamilyInputUpdated { record },
        );
    }

    pub(super) async fn record_directed_replies(&self, receipt: &Receipt, snapshot: &ChatSnapshot) {
        let sources = snapshot.message_runs.iter()
            .filter(|(_, run)| **run == receipt.run_id || snapshot.events.iter().any(|event| event.run_id == **run && matches!(event.kind, EventKind::InputMerged { into_run_id } if into_run_id == receipt.run_id)))
            .filter_map(|(index, _)| snapshot.message_ids.get(index))
            .filter_map(|id| snapshot.human_family_origins.get(id));
        let response = snapshot.run_response_text(receipt.run_id);
        let state = snapshot
            .loops
            .iter()
            .find(|goal| goal.id == receipt.loop_id)
            .map(|goal| goal.state.clone());
        let mut chats = self.inner.chats.lock().await;
        for source in sources {
            let Some(origin) = chats.get_mut(&source.source_chat_id) else {
                continue;
            };
            let Some(record) = origin.snapshot.family_inputs.get_mut(&source.message_id) else {
                continue;
            };
            if record.state.is_some() {
                continue;
            }
            record.response.clone_from(&response);
            record.state.clone_from(&state);
            let record = record.clone();
            self.persist_family_inputs(origin);
            self.emit(
                origin,
                &notification_receipt(&origin.snapshot),
                EventKind::FamilyInputUpdated { record },
            );
        }
    }
}

fn notification_receipt(snapshot: &ChatSnapshot) -> Receipt {
    let goal = snapshot.loops.last();
    Receipt {
        chat_id: snapshot.id,
        loop_id: goal.map_or(Uuid::nil(), |goal| goal.id),
        run_id: goal.and_then(|goal| goal.run_id).unwrap_or(Uuid::nil()),
    }
}

#[cfg(test)]
mod state_tests {
    use super::*;

    fn goal(state: LoopState) -> LoopSnapshot {
        LoopSnapshot {
            id: Uuid::new_v4(),
            goal: "State fixture".into(),
            state,
            run_id: None,
            run_started_at: Some(100),
            trigger: None,
            state_since: Some(200),
            event_sources: Vec::new(),
        }
    }

    #[test]
    fn older_actionable_loops_survive_a_newer_completed_loop() {
        for state in [
            LoopState::WaitingPermission,
            LoopState::WaitingQuestion,
            LoopState::WaitingPlan,
            LoopState::Running,
            LoopState::Queued,
            LoopState::WaitingTrigger,
        ] {
            let loops = [goal(state.clone()), goal(LoopState::Completed)];
            assert_eq!(summary_state(&loops, &[]), state);
        }
    }

    #[test]
    fn input_attention_precedes_other_running_loops() {
        let loops = [goal(LoopState::WaitingQuestion), goal(LoopState::Running)];
        assert_eq!(summary_state(&loops, &[]), LoopState::WaitingQuestion);
    }
}

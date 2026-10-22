use super::*;
use tickloop_agent_protocol::{
    ChatSnapshot, DirectedHumanSource, EventKind, FamilyCompletion, FamilyInputRecord,
    FamilyMessageRejection, FamilyMessageRejectionCode, HostCallContext, InputContext,
    LoopSnapshot, LoopState, Receipt, SubagentNotificationDelivery, UserShellRecord,
    UserShellState, chat_display_title,
};
use tickloop_agent_tools::permissions::PermissionPolicyExt;

/// The Loop state an Agent row reports: a user Shell waiting for permission or still working
/// keeps the Agent in need of input or working, as the Loop's own waits and runs do. A Loop that
/// never ran reports its latest user Shell: a confirmed failure, or Idle; an unknown Shell outcome
/// is not reported as a failure.
pub fn summary_state(chat_loop: &LoopSnapshot, user_shells: &[UserShellRecord]) -> LoopState {
    use tickloop_agent_protocol::UserShellState;
    let any_shell = |states: &[UserShellState]| {
        user_shells
            .iter()
            .any(|shell| states.contains(&shell.state))
    };
    if any_shell(&[UserShellState::WaitingPermission]) {
        return LoopState::WaitingPermission;
    }
    match chat_loop.state {
        LoopState::WaitingPermission | LoopState::WaitingQuestion | LoopState::WaitingPlan => {
            return chat_loop.state.clone();
        }
        LoopState::Running => return LoopState::Running,
        LoopState::Idle | LoopState::Queued | LoopState::Failed | LoopState::Interrupted => {}
    }
    if any_shell(&[
        UserShellState::Running,
        UserShellState::Background,
        UserShellState::Cancelling,
    ]) {
        return LoopState::Running;
    }
    if chat_loop.state == LoopState::Queued || any_shell(&[UserShellState::Queued]) {
        return LoopState::Queued;
    }
    if chat_loop.state != LoopState::Idle || chat_loop.run_id.is_some() {
        return chat_loop.state.clone();
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
    Idle,
    Interrupted,
    Failed,
    Unloaded,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FamilyAgent {
    pub chat_id: ChatId,
    pub parent_chat_id: Option<ChatId>,
    pub depth: usize,
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

pub(super) fn resume_messages(chat: &mut Chat) {
    for pending in &mut chat.snapshot.pending_inputs {
        if pending.agent_message.is_some() {
            pending.armed = true;
            chat.bookkeeping
                .lifecycle
                .set_queued_armed(pending.receipt.run_id, true);
        }
    }
}

impl Agent {
    pub(super) fn persist_family_message_hold(
        &self,
        chat_id: ChatId,
        mailbox: &family_mailbox::FamilyMailbox,
    ) {
        if let Some(recorder) = &self.inner.recorder {
            recorder.record_family_message_hold(
                chat_id,
                mailbox
                    .stopped_runs
                    .get(&chat_id)
                    .cloned()
                    .unwrap_or_default(),
            );
        }
    }

    pub(super) async fn resume_family_messages(&self, chat_id: ChatId) {
        let mut mailbox = self.inner.family_mailbox.lock().await;
        let mut chats = self.inner.chats.lock().await;
        mailbox.stopped_runs.remove(&chat_id);
        self.persist_family_message_hold(chat_id, &mailbox);
        if let Some(chat) = chats.get_mut(&chat_id) {
            resume_messages(chat);
            self.save(&chat.snapshot);
        }
    }

    pub async fn family_agents(&self, member: ChatId) -> Result<Vec<FamilyAgent>, AgentError> {
        let snapshot = self.snapshot(member).await?;
        let root = snapshot
            .delegation
            .as_ref()
            .map_or(member, |delegation| delegation.root_chat_id());
        let registrations = self.family_registrations(root).await?;
        let chats = self.inner.chats.lock().await;
        super::family_tree::root(&chats, member).map_err(AgentError::Invalid)?;
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
                let state = chat.map(|chat| {
                    summary_state(&chat.snapshot.chat_loop, &chat.snapshot.user_shells)
                });
                let chat_loop = chat
                    .map(|chat| &chat.snapshot.chat_loop)
                    .filter(|chat_loop| Some(&chat_loop.state) == state.as_ref());
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
                    None | Some(LoopState::Idle) => FamilyAvailability::Idle,
                    Some(LoopState::Failed) => FamilyAvailability::Failed,
                    Some(LoopState::Interrupted) => FamilyAvailability::Interrupted,
                    Some(
                        LoopState::WaitingPermission
                        | LoopState::WaitingQuestion
                        | LoopState::WaitingPlan,
                    ) => FamilyAvailability::NeedsInput,
                    Some(LoopState::Running | LoopState::Queued) => FamilyAvailability::Working,
                };
                let display_name = registration
                    .map(|registration| registration.description.clone())
                    .unwrap_or_else(|| {
                        chat.map(|chat| {
                            chat_display_title(&chat.snapshot, &chat.snapshot.user_shells)
                        })
                        .unwrap_or_else(|| "Family root".into())
                    });
                FamilyAgent {
                    chat_id: id,
                    parent_chat_id: chat.and_then(|chat| {
                        chat.snapshot
                            .delegation
                            .as_ref()
                            .map(|delegation| delegation.parent_chat_id)
                    }),
                    depth: chat
                        .and_then(|chat| chat.snapshot.delegation.as_ref())
                        .map_or(0, |delegation| delegation.depth()),
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
                    run_started_at: chat_loop
                        .and_then(|chat_loop| chat_loop.run_started_at)
                        .or(shell_started_at),
                    idle_since: chat_loop
                        .filter(|chat_loop| chat_loop.state == LoopState::Idle)
                        .and_then(|chat_loop| chat_loop.state_since),
                }
            })
            .collect())
    }
}

impl Agent {
    pub(super) fn flush_subagent_notifications(
        &self,
        parent: ChatId,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            // A durable host that shuts down leaves its notifications pending for the next one;
            // nothing more is delivered here.
            if self.inner.waits == WaitHosting::Durable && self.inner.closed.load(Ordering::SeqCst)
            {
                return;
            }
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
                if self
                    .returned_by_foreground_call(parent, &completion, &delegation)
                    .await
                {
                    continue;
                }
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
                    let updated = record.clone();
                    self.emit(
                        chat,
                        &target,
                        EventKind::FamilyRunCompleted {
                            completion: updated,
                        },
                    );
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
        if let Some(parent) = self.record_completion(child, snapshot).await {
            self.flush_subagent_notifications(parent).await;
        }
    }

    /// Records the end of the child run `child` with its parent; gives the parent when the
    /// completion is a notification still to deliver.
    pub(super) async fn record_completion(
        &self,
        child: &Receipt,
        snapshot: &ChatSnapshot,
    ) -> Option<ChatId> {
        let delegation = snapshot.delegation.as_ref()?;
        let Ok(Some(registration)) = self.subagent_registration(child.chat_id).await else {
            return None;
        };
        let chat_loop = &snapshot.chat_loop;
        let result = crate::handback::completion_text(snapshot, child.run_id);
        let error = snapshot.events.iter().rev().find_map(|event| {
            if event.run_id == child.run_id
                && let EventKind::Failed(error) = &event.kind
            {
                Some(error.clone())
            } else {
                None
            }
        });
        let run_status =
            agent_messages::child_run_status(snapshot, child.run_id).map(|(status, _)| status);
        // Held until the record is in place, so an interrupted foreground observation either
        // returns this ended run or releases its call before the run is classified.
        let invocations = self.inner.subagent_invocations.lock().await;
        let is_returned_by_foreground_call = invocations.values().any(|invocation| {
            invocation.receipt.run_id == child.run_id && !invocation.run_in_background
        });
        let completion = FamilyCompletion {
            notification: if is_returned_by_foreground_call {
                SubagentNotificationDelivery::Foreground
            } else {
                SubagentNotificationDelivery::Pending
            },
            usage: snapshot
                .request_usage
                .run_conversation_usage(child.run_id)
                .unwrap_or_default(),
            child_chat_id: child.chat_id,
            child_loop_id: child.loop_id,
            child_run_id: child.run_id,
            handle: registration.handle.clone(),
            state: chat_loop.state.clone(),
            result,
            error,
            started_at: chat_loop.run_started_at,
            finished_at: tickloop_agent_base::private_fs::unix_time_milliseconds(),
            triggering_message_ids: snapshot
                .message_runs
                .iter()
                .filter(|(_, run)| **run == child.run_id)
                .filter_map(|(index, _)| snapshot.message_ids.get(index).copied())
                .collect(),
            run_status,
        };
        {
            let mut chats = self.inner.chats.lock().await;
            let parent = chats.get_mut(&delegation.parent_chat_id)?;
            if parent.snapshot.family_completions.iter().any(|record| {
                record.child_chat_id == child.chat_id && record.child_run_id == child.run_id
            }) {
                return None;
            }
            parent.snapshot.family_completions.push(completion.clone());
            // The foreground call's wait ends in the change that records what it returns.
            if is_returned_by_foreground_call {
                resolve_subagent_wait(parent, &completion, delegation, &registration);
            }
            prune_family_completions(&mut parent.snapshot);
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
        drop(invocations);
        completion
            .notification
            .is_background()
            .then_some(delegation.parent_chat_id)
    }

    /// A pending completion whose run a foreground call waits on, such as one recorded before a
    /// load rebuilt that call: the call returns it, so it is no notification. `false` otherwise.
    async fn returned_by_foreground_call(
        &self,
        parent: ChatId,
        completion: &FamilyCompletion,
        delegation: &tickloop_agent_protocol::DelegationMetadata,
    ) -> bool {
        let invocations = self.inner.subagent_invocations.lock().await;
        if !invocations.values().any(|invocation| {
            invocation.receipt.run_id == completion.child_run_id && !invocation.run_in_background
        }) {
            return false;
        }
        let Ok(Some(registration)) = self.subagent_registration(completion.child_chat_id).await
        else {
            return false;
        };
        let mut chats = self.inner.chats.lock().await;
        let Some(chat) = chats.get_mut(&parent) else {
            return false;
        };
        let Some(record) = chat
            .snapshot
            .family_completions
            .iter_mut()
            .find(|record| record.child_run_id == completion.child_run_id)
        else {
            return false;
        };
        record.notification = SubagentNotificationDelivery::Foreground;
        let record = record.clone();
        resolve_subagent_wait(chat, &record, delegation, &registration);
        if let Some(recorder) = &self.inner.recorder {
            recorder.record_family_completions(parent, chat.snapshot.family_completions.clone());
        }
        self.save(&chat.snapshot);
        drop(chats);
        drop(invocations);
        true
    }

    /// What the foreground call on `child`'s run returns once the family records its end;
    /// `None` while the run has not ended.
    pub(super) async fn recorded_child_result(
        &self,
        parent: ChatId,
        child: &Receipt,
    ) -> Result<Option<tickloop_agent_protocol::ToolResult>, String> {
        let (completion, delegation) = {
            let chats = self.inner.chats.lock().await;
            let Some(completion) = chats
                .get(&parent)
                .ok_or("parent is unavailable")?
                .snapshot
                .family_completions
                .iter()
                .find(|completion| {
                    completion.child_chat_id == child.chat_id
                        && completion.child_run_id == child.run_id
                })
                .cloned()
            else {
                return Ok(None);
            };
            let delegation = chats
                .get(&child.chat_id)
                .and_then(|chat| chat.snapshot.delegation.clone())
                .ok_or("child Agent is unavailable")?;
            (completion, delegation)
        };
        let registration = self
            .subagent_registration(child.chat_id)
            .await
            .map_err(|error| error.to_string())?;
        Ok(agent_messages::child_run_result(
            &completion,
            &delegation,
            registration.as_ref(),
            parent,
        ))
    }
}

/// Ends the open wait of the foreground call that delegated `completion`'s run with what that
/// call returns.
pub(super) fn resolve_subagent_wait(
    parent: &mut Chat,
    completion: &FamilyCompletion,
    delegation: &tickloop_agent_protocol::DelegationMetadata,
    registration: &tickloop_agent_protocol::SubagentRegistration,
) {
    let Some(wait_id) = parent
        .bookkeeping
        .lifecycle
        .open_subagent_wait(completion.child_run_id)
        .map(|wait| wait.wait_id)
    else {
        return;
    };
    let Some(tool) = agent_messages::child_run_result(
        completion,
        delegation,
        Some(registration),
        parent.snapshot.id,
    ) else {
        return;
    };
    let tool = crate::tool_results::waited_call_result(
        parent,
        tickloop_agent_protocol::SUBAGENT_TOOL_NAME,
        tool,
    );
    parent.bookkeeping.lifecycle.resolve_wait(
        wait_id,
        tickloop_agent_transcript::ModelVisibleResult {
            content: tool.content.clone().into(),
            tool,
            ends_run: false,
        },
        None,
    );
}

/// How many of a Chat's family completions and sent messages it keeps whatever else holds them.
pub(super) const KEPT_FAMILY_RECORDS: usize = 64;

/// Keeps the completions a Chat still needs: those whose notification has not reached the
/// conversation, the latest of each child Loop (which a waiting member and a subtree stop look
/// up), and the newest [`KEPT_FAMILY_RECORDS`]. Every other completion is final: a child run
/// records one completion, once, when its input settles for good.
pub(super) fn prune_family_completions(snapshot: &mut ChatSnapshot) {
    let count = snapshot.family_completions.len();
    if count <= KEPT_FAMILY_RECORDS {
        return;
    }
    let latest_of_loop = snapshot
        .family_completions
        .iter()
        .map(|completion| {
            (
                (completion.child_chat_id, completion.child_loop_id),
                completion.child_run_id,
            )
        })
        .collect::<HashMap<_, _>>()
        .into_values()
        .collect::<HashSet<_>>();
    let queued = snapshot
        .pending_inputs
        .iter()
        .filter_map(|pending| pending.agent_message.as_ref())
        .map(|source| source.message_id)
        .collect::<HashSet<_>>();
    let newest_from = count - KEPT_FAMILY_RECORDS;
    let mut index = 0;
    snapshot.family_completions.retain(|completion| {
        let kept = index >= newest_from
            || latest_of_loop.contains(&completion.child_run_id)
            || match completion.notification {
                SubagentNotificationDelivery::Pending => true,
                SubagentNotificationDelivery::Queued => queued.contains(&completion.child_run_id),
                _ => false,
            };
        index += 1;
        kept
    });
}

/// Keeps the newest [`KEPT_FAMILY_RECORDS`] messages a Chat sent: its receivers' ledgers keep
/// their ids, so an older one is never sent again.
pub(super) fn prune_sent_agent_messages(snapshot: &mut ChatSnapshot) {
    let count = snapshot.sent_agent_messages.len();
    if count <= KEPT_FAMILY_RECORDS {
        return;
    }
    let mut by_age = snapshot
        .sent_agent_messages
        .values()
        .map(|source| (source.created_at, source.message_id))
        .collect::<Vec<_>>();
    by_age.sort();
    for (_, id) in by_age.into_iter().take(count - KEPT_FAMILY_RECORDS) {
        snapshot.sent_agent_messages.remove(&id);
    }
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
        admission: &AttachmentAdmission,
    ) -> Result<(), AgentError> {
        let snapshot = &chats.get(&chat_id).ok_or(AgentError::NotFound)?.snapshot;
        let workspace = tickloop_agent_tools::Workspace {
            folder: snapshot.folder.clone(),
            additional_directories: snapshot.additional_directories.clone(),
        };
        if workspace.folder != admission.workspace.folder
            || workspace.additional_directories != admission.workspace.additional_directories
        {
            return Err(AgentError::Invalid(
                "recipient workspace changed before attachment admission".into(),
            ));
        }
        let loop_id = snapshot.chat_loop.id;
        let receipt = Receipt {
            chat_id,
            loop_id,
            run_id: Uuid::nil(),
        };
        let owner = PermissionOwner::new(chat_id, loop_id)
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
            let operation = SessionPermissionOperation::from_prepared(prepared, &workspace)
                .map_err(|error| AgentError::Invalid(error.to_string()))?;
            let decision = sessions.evaluate(owner, &operation).decision;
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
    pub input: String,
    pub contexts: Vec<InputContext>,
    pub paths: Vec<PathBuf>,
    pub host_context: Option<HostCallContext>,
    pub directed_source: Option<DirectedHumanSource>,
    pub target_generation: Option<u64>,
    pub ledger: Option<crate::lifecycle::LedgerInput>,
    pub turn: crate::TurnAdmission,
}

impl Agent {
    pub async fn submit_family_input(
        &self,
        target: ChatId,
        source: DirectedHumanSource,
        contexts: Vec<InputContext>,
        paths: Vec<PathBuf>,
        host_context: Option<HostCallContext>,
    ) -> Result<Receipt, AgentError> {
        let sender_snapshot = self.snapshot(source.source_chat_id).await?;
        let target_snapshot = self.snapshot(target).await?;
        let sender_root = sender_snapshot
            .delegation
            .as_ref()
            .map_or(sender_snapshot.id, |delegation| delegation.root_chat_id());
        let target_root = target_snapshot
            .delegation
            .as_ref()
            .map_or(target_snapshot.id, |delegation| delegation.root_chat_id());
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
            input: body,
            contexts,
            paths,
            host_context,
            directed_source: Some(source),
            target_generation: Some(target_generation),
            ledger: None,
            turn: Default::default(),
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
        let state = Some(snapshot.chat_loop.state.clone());
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
    Receipt {
        chat_id: snapshot.id,
        loop_id: snapshot.chat_loop.id,
        run_id: snapshot.chat_loop.run_id.unwrap_or(Uuid::nil()),
    }
}

#[cfg(test)]
mod state_tests {
    use super::*;

    fn chat_loop(state: LoopState, run_id: Option<RunId>) -> LoopSnapshot {
        LoopSnapshot {
            state,
            run_id,
            run_started_at: Some(100),
            state_since: Some(200),
            ..LoopSnapshot::new(Uuid::new_v4())
        }
    }

    fn shell(state: UserShellState) -> UserShellRecord {
        serde_json::from_value(serde_json::json!({
            "id": Uuid::new_v4(), "chat_id": Uuid::new_v4(), "command": "true", "cwd": "/",
            "state": state, "pid": null, "exit_code": null, "signal": null, "reason": null,
            "output_ref": null, "output_bytes": 0, "submitted_at_message": 0,
        }))
        .unwrap()
    }

    #[test]
    fn the_loops_own_waits_and_runs_come_before_its_shells() {
        for state in [
            LoopState::WaitingPermission,
            LoopState::WaitingQuestion,
            LoopState::WaitingPlan,
            LoopState::Running,
        ] {
            let shells = [shell(UserShellState::Queued)];
            assert_eq!(
                summary_state(&chat_loop(state.clone(), None), &shells),
                state
            );
        }
        let shells = [shell(UserShellState::WaitingPermission)];
        assert_eq!(
            summary_state(&chat_loop(LoopState::Running, None), &shells),
            LoopState::WaitingPermission
        );
    }

    #[test]
    fn a_loop_that_never_ran_reports_its_latest_shell() {
        let idle = chat_loop(LoopState::Idle, None);
        assert_eq!(
            summary_state(&idle, &[shell(UserShellState::Failed)]),
            LoopState::Failed
        );
        assert_eq!(
            summary_state(&idle, &[shell(UserShellState::Unknown)]),
            LoopState::Idle
        );
        assert_eq!(
            summary_state(&idle, &[shell(UserShellState::Running)]),
            LoopState::Running
        );
        let ran = chat_loop(LoopState::Idle, Some(Uuid::new_v4()));
        assert_eq!(
            summary_state(&ran, &[shell(UserShellState::Failed)]),
            LoopState::Idle
        );
    }
}

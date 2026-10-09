use std::{
    collections::{BTreeMap, HashSet},
    path::PathBuf,
    sync::Arc,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Notify;
use uuid::Uuid;

use super::{
    ActiveConversationItem, Agent, AgentError, Chat, ChatSnapshot, InputContext, LoopId, LoopState,
    OperationState, RequestUsageLedger, RunId, SummaryDirection, UserShellState,
    tool_results::ContentReplacementState,
};
use crate::{
    model::{Content, Message, Protocol, Role},
    permissions::PermissionPolicy,
    protocol::ChatId,
    tools::CommandState,
};

const MAX_BRANCH_NAME_CHARACTERS: usize = 100;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct BranchRequest {
    pub request_id: Uuid,
    pub source_chat_id: ChatId,
    pub source_loop_id: Option<LoopId>,
    pub name: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct BranchSessionPermissionRegistration {
    pub source_chat_id: ChatId,
    pub source_loop_id: Option<LoopId>,
    pub child_chat_id: ChatId,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BranchPublicSource {
    pub chat_id: ChatId,
    pub run_id: Option<RunId>,
    pub message_id: Option<Uuid>,
    #[serde(default)]
    pub is_agent_message: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BranchPublicItem {
    Message {
        source: BranchPublicSource,
        message: Message,
        contexts: Vec<InputContext>,
        #[serde(default)]
        has_thinking: bool,
    },
    Summary {
        source: BranchPublicSource,
        direction: Option<SummaryDirection>,
        summary: String,
    },
}

/// Branch 的来源。来源 Chat 与名称随 Transcript 保存（`branch-origin` 与 `custom-title` 元数据）；
/// 创建请求与复制种子只在创建 Branch 的进程内存中。
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct BranchOrigin {
    pub source_chat_id: ChatId,
    pub name: String,
    /// 从 Transcript 恢复的 Branch 没有这部分，公开对话按子 Chat 自身的消息投影。
    #[serde(default)]
    pub creation: Option<BranchCreation>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct BranchCreation {
    pub source_loop_id: Option<LoopId>,
    pub request_id: Uuid,
    pub requested_name: Option<String>,
    pub source_raw_boundary: usize,
    pub source_public_boundary: usize,
    pub source_fingerprint: String,
    pub created_at_ms: u64,
    pub model_seed_message_count: usize,
    /// 复制对话在子 Chat 中的起始位置。向前保留的定向摘要在保留消息之前插入压缩边界，
    /// 复制对话随之后移。
    #[serde(default)]
    pub model_seed_start: usize,
    pub public_seed: Vec<BranchPublicItem>,
    pub session_permissions: BranchSessionPermissionRegistration,
}

impl BranchCreation {
    /// 截断或压缩拆散了复制种子：之后公开对话按子 Chat 自身的消息投影。
    pub(super) fn clear_seed(&mut self) {
        self.model_seed_message_count = 0;
        self.model_seed_start = 0;
        self.source_public_boundary = 0;
        self.public_seed.clear();
    }
}

#[derive(Clone, Debug)]
pub struct BranchOutcome {
    pub source_chat_id: ChatId,
    pub child_chat_id: ChatId,
    pub name: String,
    pub snapshot: ChatSnapshot,
    pub session_permissions: BranchSessionPermissionRegistration,
}

impl ChatSnapshot {
    pub fn branch_name(&self) -> Option<&str> {
        self.branch_origin
            .as_ref()
            .map(|origin| origin.name.as_str())
    }

    pub fn public_conversation_items(&self) -> Result<Vec<BranchPublicItem>, AgentError> {
        let active = self.active_conversation_projection()?;
        let Some(origin) = self
            .branch_origin
            .as_ref()
            .and_then(|origin| origin.creation.as_ref())
        else {
            return materialize_active_items(self, active);
        };
        let seed =
            origin.model_seed_start..origin.model_seed_start + origin.model_seed_message_count;
        if seed.end > self.messages.len()
            || origin.source_public_boundary != origin.public_seed.len()
        {
            return Err(AgentError::Invalid(
                "branch conversation seed exceeds the child archive".into(),
            ));
        }

        let visible_seed_messages = active
            .iter()
            .filter(|item| seed.contains(&item_message_index(item)))
            .count();
        let seed_is_visible = if origin.model_seed_message_count == 0
            || visible_seed_messages == origin.model_seed_message_count
        {
            true
        } else if visible_seed_messages == 0 {
            false
        } else {
            return Err(AgentError::Invalid(
                "active conversation exposes only part of its branch seed".into(),
            ));
        };

        if !seed_is_visible {
            return materialize_active_items(self, active);
        }
        let (before, after): (Vec<_>, Vec<_>) = active
            .into_iter()
            .filter(|item| !seed.contains(&item_message_index(item)))
            .partition(|item| item_message_index(item) < seed.start);
        let mut projected = materialize_active_items(self, before)?;
        projected.extend(origin.public_seed.iter().cloned());
        projected.extend(materialize_active_items(self, after)?);
        Ok(projected)
    }
}

fn item_message_index(item: &ActiveConversationItem) -> usize {
    match item {
        ActiveConversationItem::RawMessage { message_index }
        | ActiveConversationItem::Summary { message_index, .. } => *message_index,
    }
}

impl Agent {
    pub async fn branch_chat(&self, request: BranchRequest) -> Result<BranchOutcome, AgentError> {
        if self.inner.closed.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(AgentError::Invalid("agent host has shut down".into()));
        }
        if request.request_id.is_nil() || request.source_chat_id.is_nil() {
            return Err(AgentError::Invalid(
                "branch request and source Chat IDs must be non-nil".into(),
            ));
        }
        self.require_root_chat(request.source_chat_id, "create a Branch")
            .await?;
        let requested_name = normalize_requested_name(request.name.as_deref())?;
        let saved_titles = self.saved_titles_near(request.source_chat_id).await;
        let _settings_gate = self.inner.settings_writes.lock().await;
        let mut chats = self.inner.chats.lock().await;
        if self.inner.closed.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(AgentError::Invalid("agent host has shut down".into()));
        }
        if let Some(existing) = idempotent_branch(&chats, &request, requested_name.as_deref())? {
            return Ok(existing);
        }

        let source = chats
            .get(&request.source_chat_id)
            .ok_or(AgentError::NotFound)?;
        if request.source_loop_id.is_some_and(|source_loop_id| {
            !source
                .snapshot
                .loops
                .iter()
                .any(|source_loop| source_loop.id == source_loop_id)
        }) {
            return Err(AgentError::Invalid(
                "source Loop does not belong to the source Chat".into(),
            ));
        }
        validate_snapshot(&source.snapshot)?;
        ensure_branchable(source)?;
        let workdir = canonical_directory(&source.snapshot.workdir)?;
        self.require_working_directory_trust(&workdir)?;
        let additional_directories = source
            .snapshot
            .additional_directories
            .iter()
            .map(|directory| canonical_directory(directory))
            .collect::<Result<Vec<_>, _>>()?;
        let seed = public_seed_with_sources(&source.snapshot)?;
        let model_seed = seed
            .iter()
            .map(|(message, _)| message.clone())
            .collect::<Vec<_>>();
        let public_seed = source.snapshot.public_conversation_items()?;
        if model_seed.is_empty() && public_seed.is_empty() {
            return Err(AgentError::Invalid(
                "there is no conversation to branch".into(),
            ));
        }

        let child_chat_id = unused_chat_id(&chats);
        let retention = self.retain_chat(child_chat_id)?;
        let name = branch_title(
            requested_name.as_deref(),
            &public_seed,
            chats
                .values()
                .filter_map(|chat| chat.snapshot.branch_name())
                .chain(saved_titles.iter().map(String::as_str)),
        );
        let source_fingerprint = source_fingerprint(
            &source.snapshot,
            &model_seed,
            &public_seed,
            request.source_loop_id,
        )?;
        let session_permissions = BranchSessionPermissionRegistration {
            source_chat_id: request.source_chat_id,
            source_loop_id: request.source_loop_id,
            child_chat_id,
        };
        let origin = BranchOrigin {
            source_chat_id: request.source_chat_id,
            name: name.clone(),
            creation: Some(BranchCreation {
                source_loop_id: request.source_loop_id,
                request_id: request.request_id,
                requested_name,
                source_raw_boundary: source.snapshot.messages.len(),
                source_public_boundary: public_seed.len(),
                source_fingerprint,
                created_at_ms: crate::private_fs::unix_time_milliseconds(),
                model_seed_message_count: model_seed.len(),
                model_seed_start: 0,
                public_seed,
                session_permissions: session_permissions.clone(),
            }),
        };
        let client = source.client.clone();
        let mut snapshot = ChatSnapshot {
            failure_events: Vec::new(),
            provider_failures: Default::default(),
            tool_call_recoveries: Vec::new(),
            artifact_ticks: source.snapshot.artifact_ticks.clone(),
            removed_artifact_ticks: source.snapshot.removed_artifact_ticks.clone(),
            monitor: source.snapshot.monitor.for_new_chat(),
            request_usage: RequestUsageLedger::new(),
            id: child_chat_id,
            branch_origin: Some(origin),
            delegation: None,
            family_completions: Vec::new(),
            family_inputs: BTreeMap::new(),
            sent_agent_messages: BTreeMap::new(),
            generated_title: None,
            workdir,
            model: source.snapshot.model.clone(),
            reasoning_effort: source.snapshot.reasoning_effort,
            thinking_enabled: source.snapshot.thinking_enabled,
            messages: model_seed.clone(),
            invoked_skills: source.snapshot.invoked_skills.clone(),
            message_runs: BTreeMap::new(),
            message_contexts: BTreeMap::new(),
            denial_references: BTreeMap::new(),
            message_ids: BTreeMap::new(),
            agent_message_origins: source.snapshot.agent_message_origins.clone(),
            human_family_origins: source.snapshot.human_family_origins.clone(),
            original_user_message_ids: BTreeMap::new(),
            prompt_execution_modes: BTreeMap::new(),
            inherited_prompts: HashSet::new(),
            message_compactions: BTreeMap::new(),
            message_usage: BTreeMap::new(),
            message_sources: BTreeMap::new(),
            message_models: BTreeMap::new(),
            loops: Vec::new(),
            loop_starts: BTreeMap::new(),
            resume_triggers: Vec::new(),
            last_sequence: 0,
            permissions: Vec::new(),
            questions: Vec::new(),
            pending_inputs: Vec::new(),
            pending_commands: Vec::new(),
            last_queue_sequence: 0,
            operations: Vec::new(),
            policy: PermissionPolicy::default(),
            persistent_rule_projects: BTreeMap::new(),
            additional_directories,
            events: Vec::new(),
            pending_thinking: BTreeMap::new(),
            context_window_tokens: None,
            pending_model: None,
            plans: Vec::new(),
            progress: Vec::new(),
            plan_slug: source.snapshot.plan_slug.clone(),
            unavailable_plan_versions: BTreeMap::new(),
            run_loops: BTreeMap::new(),
            commands: Vec::new(),
            user_shells: Vec::new(),
            last_model_run: None,
        };
        let output_style = self.apply_new_chat_settings(&mut snapshot);
        carry_rewind_points(&mut snapshot, &source.snapshot, &seed);
        carry_compaction_summaries(&mut snapshot, &source.snapshot, &seed);
        carry_loops(&mut snapshot, &source.snapshot, &seed);
        carry_plans_and_progress(&mut snapshot, &source.snapshot);
        // 照 Claude Code `copyPlanForFork`：Branch 使用新的 slug 并复制计划文件，之后双方各自修改。
        let plans_directory = self.plans_directory(&snapshot.workdir);
        super::plan::copy_plans_to_new_slug(
            &mut snapshot,
            plans_directory
                .create()
                .is_ok()
                .then(|| plans_directory.path()),
        );
        validate_child(&snapshot, &model_seed)?;
        let child_prompts = snapshot
            .message_ids
            .values()
            .copied()
            .collect::<HashSet<_>>();
        let mut source_file_history = source.file_history.clone();
        source_file_history.retain_messages(&child_prompts);
        let child_file_history_directory =
            self.file_history_directory(child_chat_id, &snapshot.workdir);
        let file_history = source_file_history.relocated(child_file_history_directory.clone());

        let mut permission_context = self.inner.session_permissions.lock().await;
        let mut permission_candidate = permission_context.clone();
        permission_candidate
            .register_branch(
                request.source_chat_id,
                request.source_loop_id,
                child_chat_id,
            )
            .map_err(|error| AgentError::Invalid(error.to_string()))?;
        if let Some(recorder) = &self.inner.recorder
            && let Some(source) = chats.get_mut(&request.source_chat_id)
        {
            recorder.register_branch(&mut source.snapshot, child_chat_id, &name);
        }
        let tools = self.create_chat_tools(&snapshot);
        let skills = crate::skills::SkillRegistry::discover(
            &snapshot.workdir,
            self.inner.user_skills_directory.as_deref(),
        );
        // 照 Claude Code `/branch`：以子 Chat 的 id 重新记录来源中仍在复制对话里的替换决定。
        let source_replacements = chats
            .get(&request.source_chat_id)
            .map(|source| source.content_replacements.records())
            .unwrap_or_default();
        let content_replacements =
            ContentReplacementState::reconstruct(&snapshot, &source_replacements);
        let child_replacements = content_replacements.records();
        if !child_replacements.is_empty() {
            self.record_content_replacements(&snapshot, child_replacements);
        }
        self.inner
            .subagent_registry
            .lock()
            .await
            .insert(child_chat_id, Vec::new());
        chats.insert(
            child_chat_id,
            Chat {
                family_generation: 0,
                snapshot: snapshot.clone(),
                todo_completion_graces: Default::default(),
                start_new_tool_result_message: false,
                retention,
                // 备份在锁外复制完成之前占住子 Chat，不能开始 Run 或 rewind。
                active: Some(tokio_util::sync::CancellationToken::new()),
                active_run: None,
                parked: Default::default(),
                ready_runs: Default::default(),
                client,
                pending_client: None,
                pending_model_thinking: None,
                tools,
                skills,
                content_replacements,
                file_history,
                watching_commands: false,
                watching_user_shells: false,
                user_shell_queue: Arc::new(Notify::new()),
                trigger_cancellations: Default::default(),
                event_cancellations: Default::default(),
                disabled_event_source_runs: Default::default(),
                active_event_claims: Default::default(),
                deferred_messages: Vec::new(),
                user_cancelled_runs: std::collections::HashSet::new(),
                output_style,
                auto_denials: super::auto_mode::DenialTracking::default(),
                denial_window: super::denials::DenialWindow::default(),
            },
        );
        self.save(&snapshot);
        *permission_context = permission_candidate;
        drop(permission_context);
        drop(chats);
        // 照 Claude Code `copyFileHistoryForResume` 把备份硬链接到子 Chat 的目录（失败时复制），
        // 复制可能涉及大文件，放在锁外；备份全部复制成功的快照以子 Chat 的 id 写入 Transcript。
        let copied_snapshots = tokio::task::spawn_blocking(move || {
            source_file_history.copy_backups_to(&child_file_history_directory)
        })
        .await
        .unwrap_or_default();
        for copied in &copied_snapshots {
            self.record_file_history_snapshot(child_chat_id, copied.message_id, copied, false);
        }
        let schedule = {
            let mut chats = self.inner.chats.lock().await;
            if let Some(child) = chats.get_mut(&child_chat_id) {
                child.active = None;
                true
            } else {
                false
            }
        };
        let next = if schedule {
            self.next_run(child_chat_id).await?
        } else {
            None
        };
        if let Some(next) = next {
            self.inner.tasks.lock().await.push(self.run_task(next));
        }
        self.start_restored_todo_graces(child_chat_id).await;
        self.flush_transcripts().await;
        Ok(BranchOutcome {
            source_chat_id: request.source_chat_id,
            child_chat_id,
            name,
            snapshot,
            session_permissions,
        })
    }
}

impl Agent {
    /// 源 Chat 所在项目中已保存的标题，供 Branch 标题查重。
    async fn saved_titles_near(&self, source_chat_id: ChatId) -> Vec<String> {
        let Some(directory) = self
            .inner
            .recorder
            .as_ref()
            .and_then(|recorder| recorder.project_directory_of(source_chat_id))
        else {
            return Vec::new();
        };
        tokio::task::spawn_blocking(move || crate::transcript::list_project(&directory))
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|summary| summary.title)
            .collect()
    }
}

fn materialize_active_items(
    snapshot: &ChatSnapshot,
    items: impl IntoIterator<Item = ActiveConversationItem>,
) -> Result<Vec<BranchPublicItem>, AgentError> {
    let mut projected = Vec::new();
    for item in items {
        match item {
            ActiveConversationItem::RawMessage { message_index } => {
                let message = snapshot.messages.get(message_index).ok_or_else(|| {
                    AgentError::Invalid(
                        "active conversation projection exceeds the raw archive".into(),
                    )
                })?;
                if message.role == Role::System {
                    continue;
                }
                let has_thinking = message.role == Role::Assistant
                    && message.content.iter().any(|content| match content {
                        Content::Thinking { thinking, .. } => !thinking.is_empty(),
                        Content::ProtocolData {
                            protocol: Protocol::Responses,
                            value,
                        } if value["type"] == "reasoning" => {
                            ["content", "summary"].iter().any(|field| {
                                value[*field].as_array().is_some_and(|parts| {
                                    parts.iter().any(|part| {
                                        part["text"].as_str().is_some_and(|text| !text.is_empty())
                                    })
                                })
                            })
                        }
                        _ => false,
                    });
                let content = message
                    .content
                    .iter()
                    .filter(|content| {
                        !matches!(
                            content,
                            Content::Thinking { .. } | Content::ProtocolData { .. }
                        )
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                let contexts = snapshot
                    .message_contexts
                    .get(&message_index)
                    .cloned()
                    .unwrap_or_default();
                if !content.is_empty() || !contexts.is_empty() || has_thinking {
                    let run_id = snapshot.message_runs.get(&message_index).copied();
                    projected.push(BranchPublicItem::Message {
                        source: BranchPublicSource {
                            chat_id: snapshot.id,
                            run_id,
                            message_id: snapshot.message_ids.get(&message_index).copied(),
                            is_agent_message: snapshot
                                .message_ids
                                .get(&message_index)
                                .is_some_and(|id| snapshot.agent_message_origins.contains_key(id)),
                        },
                        message: Message {
                            role: message.role,
                            content,
                        },
                        contexts,
                        has_thinking,
                    });
                }
            }
            ActiveConversationItem::Summary {
                direction, summary, ..
            } => projected.push(BranchPublicItem::Summary {
                source: BranchPublicSource {
                    chat_id: snapshot.id,
                    run_id: None,
                    message_id: None,
                    is_agent_message: false,
                },
                direction,
                summary,
            }),
        }
    }
    Ok(projected)
}

/// 源 Chat 发给模型的公开对话（去掉私有推理与空消息），以及每条消息投影自的源消息下标。
fn public_seed_with_sources(
    source: &ChatSnapshot,
) -> Result<Vec<(Message, Option<usize>)>, AgentError> {
    let mut seed = super::context::projected_messages_with_sources(source)?;
    for (message, _) in &mut seed {
        message.content.retain(|content| {
            !matches!(
                content,
                Content::Thinking { .. } | Content::ProtocolData { .. }
            )
        });
    }
    seed.retain(|(message, _)| message.role != Role::System && !message.content.is_empty());
    Ok(seed)
}

/// 照 Claude Code 复制对话时保留消息 uuid：来自源 Chat 用户提示的种子消息仍是 rewind 的
/// 可选位置。这些消息换回源提示的原文与附带上下文，投影结果与复制的种子一致；文件回滚
/// 快照按同一 uuid 关联，Branch 中 rewind 仍能恢复分出之前的文件状态。
fn carry_rewind_points(
    child: &mut ChatSnapshot,
    source: &ChatSnapshot,
    seed: &[(Message, Option<usize>)],
) {
    for (child_index, (_, source_index)) in seed.iter().enumerate() {
        if let Some(index) = source_index {
            if let Some(model) = source.message_models.get(index) {
                child.message_models.insert(child_index, model.clone());
            }
            if let Some(origin) = source.message_sources.get(index) {
                child.message_sources.insert(child_index, origin.clone());
            }
        }
        let Some(point) = source_index.and_then(|index| source.rewind_point_at(index)) else {
            continue;
        };
        child.messages[child_index] = Message {
            role: Role::User,
            content: vec![Content::Text { text: point.prompt }],
        };
        if !point.contexts.is_empty() {
            child.message_contexts.insert(child_index, point.contexts);
        }
        child.message_ids.insert(child_index, point.message_id);
        child
            .prompt_execution_modes
            .insert(child_index, point.execution_mode);
        child.inherited_prompts.insert(child_index);
    }
}

/// Branch 按当前对话链复制，分出点之前创建的 Loop 就是源 Chat 当前的全部 Loop。它们照重启恢复
/// 的规则进入子 Chat：运行或等待中的转为已中断，武装中的 Trigger 解除武装，由用户向该 Loop
/// 提交输入时重新武装。创建位置换算到复制对话中第一条晚于创建位置的消息之前。
fn carry_loops(child: &mut ChatSnapshot, source: &ChatSnapshot, seed: &[(Message, Option<usize>)]) {
    for goal in &source.loops {
        let source_start = source.loop_starts.get(&goal.id).copied().unwrap_or(0);
        let child_start = seed
            .iter()
            .position(|(_, source_index)| source_index.is_some_and(|index| index >= source_start))
            .unwrap_or(seed.len());
        child.loop_starts.insert(goal.id, child_start);
        child.loops.push(goal.clone());
    }
    child.resume_triggers = source
        .resume_triggers
        .iter()
        .copied()
        .filter(|loop_id| child.loops.iter().any(|goal| goal.id == *loop_id))
        .collect();
    super::triggers::recover_loops(child);
}

/// Copies Loop plans and progress without process-local approvals or execution modes.
fn carry_plans_and_progress(child: &mut ChatSnapshot, source: &ChatSnapshot) {
    let carries = |loop_id: &LoopId| child.loops.iter().any(|goal| goal.id == *loop_id);
    let plans = source
        .plans
        .iter()
        .filter(|plan| carries(&plan.loop_id))
        .map(|plan| super::Plan {
            approved_execution_mode: None,
            ..plan.clone()
        })
        .collect();
    let progress = source
        .progress
        .iter()
        .filter(|progress| carries(&progress.loop_id))
        .cloned()
        .collect();
    child.plans = plans;
    child.unavailable_plan_versions = source
        .unavailable_plan_versions
        .iter()
        .filter(|(loop_id, _)| carries(loop_id))
        .map(|(loop_id, version)| (*loop_id, *version))
        .collect();
    child.progress = progress;
}

/// 复制对话中的压缩摘要仍标记为压缩摘要。Branch 按当前链复制边界之后的可见对话，边界本身不再
/// 需要：子 Chat 的对话链从复制的第一条消息开始（Claude Code `/branch` 也把边界接成直线链）。
fn carry_compaction_summaries(
    child: &mut ChatSnapshot,
    source: &ChatSnapshot,
    seed: &[(Message, Option<usize>)],
) {
    for (child_index, (_, source_index)) in seed.iter().enumerate() {
        if let Some(marker @ super::CompactionMarker::Summary { .. }) =
            source_index.and_then(|index| source.message_compactions.get(&index))
        {
            child
                .message_compactions
                .insert(child_index, marker.clone());
        }
    }
}

fn valid_public_source(child_chat_id: ChatId, source: BranchPublicSource) -> bool {
    !source.chat_id.is_nil()
        && source.chat_id != child_chat_id
        && source.run_id.is_none_or(|run_id| !run_id.is_nil())
}

fn validate_child(
    snapshot: &ChatSnapshot,
    expected_model_seed: &[Message],
) -> Result<(), AgentError> {
    validate_snapshot(snapshot)?;
    if snapshot.conversation_messages()? != expected_model_seed {
        return Err(AgentError::Invalid(
            "branch model seed is not stable under child projection".into(),
        ));
    }
    let origin = snapshot
        .branch_origin
        .as_ref()
        .and_then(|origin| origin.creation.as_ref())
        .ok_or_else(|| AgentError::Invalid("branch child is missing its origin".into()))?;
    if snapshot.public_conversation_items()? != origin.public_seed {
        return Err(AgentError::Invalid(
            "branch public seed is not stable under child projection".into(),
        ));
    }
    Ok(())
}

pub(super) fn validate_snapshot(snapshot: &ChatSnapshot) -> Result<(), AgentError> {
    let Some(origin) = &snapshot.branch_origin else {
        return Ok(());
    };
    if snapshot.id.is_nil()
        || origin.source_chat_id.is_nil()
        || origin.source_chat_id == snapshot.id
    {
        return Err(AgentError::Invalid(
            "branch origin identity is invalid".into(),
        ));
    }
    if !is_valid_branch_name(&origin.name) {
        return Err(AgentError::Invalid(
            "branch origin contains an invalid display name".into(),
        ));
    }
    let Some(creation) = &origin.creation else {
        return Ok(());
    };
    if creation.request_id.is_nil()
        || creation.created_at_ms == 0
        || creation.source_fingerprint.len() != 64
        || !creation
            .source_fingerprint
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
        || creation.source_public_boundary != creation.public_seed.len()
        || creation.model_seed_start + creation.model_seed_message_count > snapshot.messages.len()
    {
        return Err(AgentError::Invalid(
            "branch origin identity, boundary or fingerprint is invalid".into(),
        ));
    }
    if normalize_requested_name(creation.requested_name.as_deref())?.as_deref()
        != creation.requested_name.as_deref()
    {
        return Err(AgentError::Invalid(
            "branch origin contains an invalid display name".into(),
        ));
    }
    if creation.session_permissions.source_chat_id != origin.source_chat_id
        || creation.session_permissions.source_loop_id != creation.source_loop_id
        || creation.session_permissions.child_chat_id != snapshot.id
    {
        return Err(AgentError::Invalid(
            "branch session permission provenance does not match its origin".into(),
        ));
    }
    for item in &creation.public_seed {
        match item {
            BranchPublicItem::Message {
                source, message, ..
            } if !valid_public_source(snapshot.id, *source)
                || message.role == Role::System
                || message.content.iter().any(|content| {
                    matches!(
                        content,
                        Content::Thinking { .. } | Content::ProtocolData { .. }
                    )
                }) =>
            {
                return Err(AgentError::Invalid(
                    "branch public seed contains private model history".into(),
                ));
            }
            BranchPublicItem::Summary {
                source, summary, ..
            } if !valid_public_source(snapshot.id, *source)
                || source.run_id.is_some()
                || summary.trim().is_empty() =>
            {
                return Err(AgentError::Invalid(
                    "branch public seed contains invalid summary provenance or content".into(),
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

fn ensure_branchable(chat: &Chat) -> Result<(), AgentError> {
    let has_pending_runtime = chat.active.is_some()
        || chat.active_run.is_some()
        || !chat.parked.is_empty()
        || !chat.ready_runs.is_empty()
        || chat.pending_client.is_some()
        || chat.snapshot.pending_model.is_some()
        || !chat.snapshot.pending_inputs.is_empty()
        || !chat.snapshot.pending_commands.is_empty()
        || !chat.snapshot.permissions.is_empty()
        || !chat.snapshot.questions.is_empty();
    let has_pending_loop = chat.snapshot.loops.iter().any(|source_loop| {
        matches!(
            source_loop.state,
            LoopState::Queued
                | LoopState::WaitingPermission
                | LoopState::WaitingQuestion
                | LoopState::WaitingPlan
                | LoopState::Running
        )
    });
    let has_foreground_command = chat.snapshot.commands.iter().any(|command| {
        matches!(
            command.state,
            CommandState::Running | CommandState::Cancelling
        )
    });
    let has_foreground_user_shell = chat.snapshot.user_shells.iter().any(|record| {
        matches!(
            record.state,
            UserShellState::Queued | UserShellState::Running | UserShellState::Cancelling
        )
    });
    let background_operations = chat
        .snapshot
        .commands
        .iter()
        .filter(|command| command.state == CommandState::Background)
        .map(|command| command.operation_id)
        .collect::<std::collections::HashSet<_>>();
    let has_unresolved_operation = chat
        .snapshot
        .operations
        .iter()
        .any(|operation| match operation.state {
            OperationState::Proposed => true,
            OperationState::Admitted => !background_operations.contains(&operation.id),
            OperationState::Completed | OperationState::Failed | OperationState::Unknown => false,
        });
    if has_pending_runtime
        || has_pending_loop
        || has_foreground_command
        || has_foreground_user_shell
        || has_unresolved_operation
    {
        return Err(AgentError::Busy);
    }
    Ok(())
}

fn idempotent_branch(
    chats: &std::collections::HashMap<ChatId, Chat>,
    request: &BranchRequest,
    requested_name: Option<&str>,
) -> Result<Option<BranchOutcome>, AgentError> {
    fn creation_of(chat: &Chat) -> Option<(&BranchOrigin, &BranchCreation)> {
        let origin = chat.snapshot.branch_origin.as_ref()?;
        Some((origin, origin.creation.as_ref()?))
    }
    let mut matching = chats.values().filter(|chat| {
        creation_of(chat).is_some_and(|(_, creation)| creation.request_id == request.request_id)
    });
    let Some(chat) = matching.next() else {
        return Ok(None);
    };
    if matching.next().is_some() {
        return Err(AgentError::Invalid(
            "branch request identity resolves to more than one child Chat".into(),
        ));
    }
    let (origin, creation) = creation_of(chat).unwrap();
    if origin.source_chat_id != request.source_chat_id
        || creation.source_loop_id != request.source_loop_id
        || creation.requested_name.as_deref() != requested_name
    {
        return Err(AgentError::Invalid(
            "branch request identity was already used for different input".into(),
        ));
    }
    Ok(Some(BranchOutcome {
        source_chat_id: origin.source_chat_id,
        child_chat_id: chat.snapshot.id,
        name: origin.name.clone(),
        snapshot: chat.snapshot.clone(),
        session_permissions: creation.session_permissions.clone(),
    }))
}

pub(super) fn is_valid_branch_name(name: &str) -> bool {
    name.trim() == name && !name.is_empty() && !name.chars().any(char::is_control)
}

fn normalize_requested_name(name: Option<&str>) -> Result<Option<String>, AgentError> {
    let Some(name) = name else {
        return Ok(None);
    };
    let name = name.trim();
    if name.is_empty() {
        return Err(AgentError::Invalid("branch name cannot be empty".into()));
    }
    if name.chars().any(char::is_control) {
        return Err(AgentError::Invalid(
            "branch name cannot contain control characters".into(),
        ));
    }
    if name.chars().count() > MAX_BRANCH_NAME_CHARACTERS {
        return Err(AgentError::Invalid(format!(
            "branch name cannot exceed {MAX_BRANCH_NAME_CHARACTERS} characters"
        )));
    }
    Ok(Some(name.into()))
}

/// 照 Claude Code `getUniqueForkName`：标题基于指定名称或首条提示（压缩空白后取前 100 个字符），
/// 总是追加 " (Branch)"；与已有标题（不区分大小写）重名时依次使用 " (Branch 2)"、" (Branch 3)"。
fn branch_title<'a>(
    requested_name: Option<&str>,
    public_seed: &[BranchPublicItem],
    existing_titles: impl Iterator<Item = &'a str>,
) -> String {
    let base = requested_name.map(str::to_owned).unwrap_or_else(|| {
        public_seed
            .iter()
            .find_map(|item| match item {
                BranchPublicItem::Message { message, .. } if message.role == Role::User => {
                    message.content.iter().find_map(|content| match content {
                        Content::Text { text } => Some(
                            collapse_whitespace(text)
                                .chars()
                                .take(MAX_BRANCH_NAME_CHARACTERS)
                                .collect::<String>(),
                        ),
                        _ => None,
                    })
                }
                _ => None,
            })
            .filter(|text| !text.is_empty())
            .unwrap_or_else(|| "Branched conversation".into())
    });
    let existing = existing_titles
        .map(|title| title.trim().to_lowercase())
        .collect::<std::collections::HashSet<_>>();
    let candidate = format!("{base} (Branch)");
    if !existing.contains(&candidate.to_lowercase()) {
        return candidate;
    }
    (2_u64..)
        .map(|ordinal| format!("{base} (Branch {ordinal})"))
        .find(|candidate| !existing.contains(&candidate.to_lowercase()))
        .expect("an unused Branch ordinal exists")
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn source_fingerprint(
    snapshot: &ChatSnapshot,
    model_seed: &[Message],
    public_seed: &[BranchPublicItem],
    source_loop_id: Option<LoopId>,
) -> Result<String, AgentError> {
    let encoded = serde_json::to_vec(&(
        snapshot.id,
        source_loop_id,
        snapshot.messages.len(),
        snapshot.last_sequence,
        &snapshot.model,
        &snapshot.workdir,
        &snapshot.additional_directories,
        model_seed,
        public_seed,
    ))
    .map_err(|error| AgentError::Invalid(format!("branch source cannot be encoded: {error}")))?;
    Ok(format!("{:x}", Sha256::digest(encoded)))
}

fn canonical_directory(path: &std::path::Path) -> Result<PathBuf, AgentError> {
    let canonical = path.canonicalize().map_err(|_| {
        AgentError::Invalid(format!("directory is unavailable: {}", path.display()))
    })?;
    if canonical != path || !canonical.is_dir() {
        return Err(AgentError::Invalid(format!(
            "directory changed after it was configured: {}",
            path.display()
        )));
    }
    Ok(canonical)
}

fn unused_chat_id(chats: &std::collections::HashMap<ChatId, Chat>) -> ChatId {
    loop {
        let id = Uuid::new_v4();
        if !chats.contains_key(&id) {
            return id;
        }
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::model::{
        InputModality, ModelClient, ModelConfig, ModelDelta, ModelError, ModelIdentity,
        ModelOutput, ModelRequest, Protocol,
    };

    struct UnusedModel;

    #[async_trait]
    impl ModelClient for UnusedModel {
        async fn generate_title(
            &self,
            request: crate::model::ModelRequest,
            _: tokio::sync::mpsc::UnboundedSender<crate::model::ModelDelta>,
            _: tokio_util::sync::CancellationToken,
        ) -> Result<crate::model::ModelOutput, crate::model::ModelError> {
            Ok(crate::model::fixture_title(request))
        }

        fn identity(&self) -> ModelIdentity {
            ModelIdentity {
                provider: crate::model::ModelProvider::Api,
                profile: "branch-test".into(),
                protocol: Protocol::Messages,
                requested_model: "branch-test".into(),
            }
        }

        async fn complete(
            &self,
            _: ModelRequest,
            _: mpsc::UnboundedSender<ModelDelta>,
            _: CancellationToken,
        ) -> Result<ModelOutput, ModelError> {
            panic!("branch creation must not call the model")
        }
    }

    fn model_config() -> ModelConfig {
        ModelConfig {
            provider: crate::model::ModelProvider::Api,
            auth_source: crate::model::ModelAuthSource::ApiKeyEnv,
            name: "branch-test".into(),
            protocol: Protocol::Messages,
            base_url: "http://127.0.0.1".into(),
            model: "branch-test".into(),
            api_key_env: String::new(),
            max_output_tokens: 128,
            provider_id: String::new(),
            provider_name: String::new(),
            model_id: String::new(),
            connection: None,
            card: None,
            input_modalities: vec![InputModality::Text],
        }
    }

    #[derive(Debug)]
    enum BusySourceState {
        ActiveModel,
        RunningDirectedSummary,
        ActiveRun,
        ParkedRun,
        ReadyRun,
        PendingClient,
        PendingModel,
        PendingInput,
        PendingCommand,
        Permission,
        Question,
        Loop(LoopState),
        AgentShell(CommandState),
        UserShell(UserShellState),
        Operation(OperationState),
    }

    impl BusySourceState {
        fn apply(&self, chat: &mut Chat) {
            let loop_id = Uuid::new_v4();
            let run_id = Uuid::new_v4();
            match self {
                Self::ActiveModel => chat.active = Some(CancellationToken::new()),
                Self::RunningDirectedSummary => chat.active = Some(CancellationToken::new()),
                Self::ActiveRun => chat.active_run = Some(run_id),
                Self::ParkedRun => {
                    chat.parked.insert(
                        run_id,
                        super::super::ParkedRun {
                            receipt: super::super::Receipt {
                                chat_id: chat.snapshot.id,
                                loop_id,
                                run_id,
                            },
                            cancel: CancellationToken::new(),
                            resume: Arc::new(Notify::new()),
                            sequence: 0,
                        },
                    );
                }
                Self::ReadyRun => chat.ready_runs.push_back(run_id),
                Self::PendingClient => chat.pending_client = Some(Arc::new(UnusedModel)),
                Self::PendingModel => chat.snapshot.pending_model = Some(model_config()),
                Self::PendingInput => {
                    chat.snapshot
                        .pending_inputs
                        .push(super::super::PendingInput {
                            receipt: super::super::Receipt {
                                chat_id: chat.snapshot.id,
                                loop_id,
                                run_id,
                            },
                            input: "queued work".into(),
                            contexts: Vec::new(),
                            agent_message: None,
                            directed_human: None,
                            denial_reference: None,
                            trigger_source: None,
                            armed: true,
                            queued_sequence: 1,
                            task_continuation: false,
                            is_skill_invocation: false,
                            attached_file_reads: Vec::new(),
                        });
                }
                Self::PendingCommand => {
                    chat.snapshot
                        .pending_commands
                        .push(super::super::PendingCommand {
                            id: Uuid::new_v4(),
                            chat_id: chat.snapshot.id,
                            loop_id: Some(loop_id),
                            command: "/status".into(),
                            queued_sequence: 1,
                            armed: true,
                            dispatched: false,
                        });
                }
                Self::Permission => {
                    chat.snapshot
                        .permissions
                        .push(super::super::PermissionRequest {
                            id: Uuid::new_v4(),
                            loop_id,
                            execution: crate::runtime::PermissionExecution::ToolCall {
                                run_id,
                                call_id: "busy-permission".into(),
                            },
                            name: "Bash".into(),
                            input: serde_json::json!({"command": "true"}),
                            file_change: None,
                            approval_reason: None,
                            approval_source: super::super::PermissionApprovalSource::default(),
                        });
                }
                Self::Question => {
                    chat.snapshot.questions.push(super::super::QuestionRequest {
                        id: Uuid::new_v4(),
                        chat_id: chat.snapshot.id,
                        loop_id,
                        run_id,
                        call_id: "busy-question".into(),
                        questions: vec![super::super::Question {
                            question: "Continue?".into(),
                            header: "Continue".into(),
                            options: ["yes", "no"]
                                .map(|label| super::super::QuestionOption {
                                    label: label.into(),
                                    description: String::new(),
                                    preview: None,
                                })
                                .to_vec(),
                            multi_select: false,
                        }],
                    });
                }
                Self::Loop(state) => chat.snapshot.loops.push(super::super::LoopSnapshot {
                    id: loop_id,
                    goal: "busy goal".into(),
                    state: state.clone(),
                    run_id: Some(run_id),
                    run_started_at: None,
                    trigger: None,
                    event_sources: Vec::new(),
                    state_since: None,
                }),
                Self::AgentShell(state) => {
                    chat.snapshot.commands.push(crate::tools::CommandSnapshot {
                        updated_at_ms: None,
                        operation_id: Uuid::new_v4(),
                        loop_id,
                        command: "sleep 30".into(),
                        description: None,
                        cwd: chat.snapshot.workdir.clone(),
                        pid: Some(42),
                        state: state.clone(),
                        was_background: false,
                        backgrounded_on_timeout: false,
                        exit_code: None,
                        signal: None,
                        reason: None,
                        output_ref: chat
                            .tools
                            .chat_directory()
                            .path()
                            .join("busy-agent-shell.log"),
                        output_bytes: 0,
                    });
                }
                Self::UserShell(state) => {
                    chat.snapshot
                        .user_shells
                        .push(super::super::UserShellRecord {
                            updated_at_ms: None,
                            id: Uuid::new_v4(),
                            chat_id: chat.snapshot.id,
                            loop_id: Some(loop_id),
                            command: "sleep 30".into(),
                            cwd: chat.snapshot.workdir.clone(),
                            starts_in_background: false,
                            queued_sequence: 1,
                            armed: true,
                            preceding_model_runs: Vec::new(),
                            state: state.clone(),
                            pid: Some(42),
                            exit_code: None,
                            signal: None,
                            reason: None,
                            output_ref: Some(
                                chat.tools
                                    .chat_directory()
                                    .path()
                                    .join("busy-user-shell.log"),
                            ),
                            output_bytes: 0,
                            submitted_at_message: 1,
                            state_since: None,
                        });
                }
                Self::Operation(state) => {
                    chat.snapshot
                        .operations
                        .push(super::super::OperationRecord {
                            id: Uuid::new_v4(),
                            loop_id,
                            run_id,
                            call_id: "busy-operation".into(),
                            name: "Bash".into(),
                            input: serde_json::json!({"command": "true"}),
                            file_change: None,
                            state: state.clone(),
                            result: None,
                        });
                }
            }
        }
    }

    #[test]
    fn automatic_branch_title_uses_the_first_hundred_prompt_characters_before_its_suffix() {
        let public_seed = vec![BranchPublicItem::Message {
            source: BranchPublicSource {
                chat_id: Uuid::from_u128(1),
                run_id: Some(Uuid::from_u128(2)),
                message_id: None,
                is_agent_message: false,
            },
            message: Message {
                role: Role::User,
                content: vec![Content::Text {
                    text: "x".repeat(MAX_BRANCH_NAME_CHARACTERS * 2),
                }],
            },
            contexts: Vec::new(),
            has_thinking: false,
        }];
        let name = branch_title(None, &public_seed, std::iter::empty());
        assert_eq!(
            name,
            format!("{} (Branch)", "x".repeat(MAX_BRANCH_NAME_CHARACTERS))
        );
        let collided = branch_title(
            Some("Value Path"),
            &public_seed,
            ["value path (branch)", "Value Path (Branch 3)"].into_iter(),
        );
        assert_eq!(collided, "Value Path (Branch 2)");
    }

    #[tokio::test]
    async fn every_busy_source_state_rejects_without_publishing_or_mutating() {
        let cases = vec![
            BusySourceState::ActiveModel,
            BusySourceState::RunningDirectedSummary,
            BusySourceState::ActiveRun,
            BusySourceState::ParkedRun,
            BusySourceState::ReadyRun,
            BusySourceState::PendingClient,
            BusySourceState::PendingModel,
            BusySourceState::PendingInput,
            BusySourceState::PendingCommand,
            BusySourceState::Permission,
            BusySourceState::Question,
            BusySourceState::Loop(LoopState::Queued),
            BusySourceState::Loop(LoopState::WaitingPermission),
            BusySourceState::Loop(LoopState::WaitingQuestion),
            BusySourceState::Loop(LoopState::WaitingPlan),
            BusySourceState::Loop(LoopState::Running),
            BusySourceState::AgentShell(CommandState::Running),
            BusySourceState::AgentShell(CommandState::Cancelling),
            BusySourceState::UserShell(UserShellState::Queued),
            BusySourceState::UserShell(UserShellState::Running),
            BusySourceState::UserShell(UserShellState::Cancelling),
            BusySourceState::Operation(OperationState::Proposed),
            BusySourceState::Operation(OperationState::Admitted),
        ];

        for state in cases {
            let label = format!("{state:?}");
            let directory = tempfile::tempdir().unwrap();
            let agent = Agent::new_embedded(model_config(), Arc::new(UnusedModel));
            let source_chat_id = agent.create_chat(directory.path().into()).await.unwrap();
            {
                let mut chats = agent.inner.chats.lock().await;
                let source = chats.get_mut(&source_chat_id).unwrap();
                source.snapshot.messages.push(Message {
                    role: Role::User,
                    content: vec![Content::Text {
                        text: "source history".into(),
                    }],
                });
                state.apply(source);
            }
            let source_before =
                serde_json::to_value(agent.snapshot(source_chat_id).await.unwrap()).unwrap();
            let chat_count_before = agent.list_chats().await.len();

            let result = agent
                .branch_chat(BranchRequest {
                    request_id: Uuid::new_v4(),
                    source_chat_id,
                    source_loop_id: None,
                    name: Some(format!("busy {label}")),
                })
                .await;

            assert!(
                matches!(result, Err(AgentError::Busy)),
                "{label} returned {result:?}"
            );
            assert_eq!(agent.list_chats().await.len(), chat_count_before, "{label}");
            assert_eq!(
                serde_json::to_value(agent.snapshot(source_chat_id).await.unwrap()).unwrap(),
                source_before,
                "{label}"
            );
            agent.shutdown().await;
        }
    }
}

use std::path::PathBuf;

use tickloop_agent_protocol::{
    ChatSnapshot, CommandSnapshot, Content, Event, EventKind, FamilyCompletion, FamilyInputRecord,
    PermissionRequest, Plan, Protocol, QuestionRequest, Role, Usage,
};
use uuid::Uuid;

/// 与 `runtime/persistence.rs` 恢复对话时追加的消息文本一致，取自 Claude Code
/// `conversationRecovery.ts`。
const CONTINUE_FROM_INTERRUPTION: &str = "Continue from where you left off.";
const NO_RESPONSE_REQUESTED: &str = "No response requested.";

/// 照 Claude Code：续跑 meta 消息不进入 `shouldShowUserMessage`，`AssistantTextMessage`
/// 对 `NO_RESPONSE_REQUESTED` 不渲染；两者只留在模型请求中。Transcript 的 meta 标记不在
/// 快照中，续跑消息按原文识别。
pub(super) fn is_interruption_placeholder(role: Role, text: &str) -> bool {
    match role {
        Role::User => text == CONTINUE_FROM_INTERRUPTION,
        Role::Assistant => text == NO_RESPONSE_REQUESTED,
        Role::System => false,
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) enum ConversationBlockId {
    MessageContent {
        chat_id: Uuid,
        message_index: usize,
        content_index: usize,
    },
    StreamingAssistant {
        chat_id: Uuid,
        run_id: Option<Uuid>,
    },
    StreamingThinking {
        chat_id: Uuid,
        run_id: Uuid,
    },
    Permission {
        chat_id: Uuid,
        request_id: Uuid,
    },
    Question {
        chat_id: Uuid,
        request_id: Uuid,
    },
    Plan {
        chat_id: Uuid,
        loop_id: Uuid,
        version: u64,
    },
    FamilyInput {
        chat_id: Uuid,
        message_id: Uuid,
    },
    FamilyCompletion {
        chat_id: Uuid,
        child_run_id: Uuid,
    },
    Outcome {
        chat_id: Uuid,
        sequence: u64,
    },
    Command {
        chat_id: Uuid,
        operation_id: Uuid,
    },
    ToolCall {
        exchange: ToolExchangeId,
    },
    ToolResult {
        exchange: ToolExchangeId,
    },
    Event {
        chat_id: Uuid,
        sequence: u64,
    },
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) struct ToolExchangeId {
    pub(super) chat_id: Uuid,
    pub(super) run_id: Option<Uuid>,
    pub(super) call_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ConversationModelMetadata {
    pub(super) model: String,
    pub(super) protocol: Protocol,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ConversationBlockMetadata {
    pub(super) run_id: Option<Uuid>,
    pub(super) loop_id: Option<Uuid>,
    pub(super) model: Option<ConversationModelMetadata>,
    pub(super) timestamp_ms: Option<u64>,
    pub(super) event_sequence: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AssistantState {
    Complete,
    Streaming,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ThinkingSource {
    ModelText,
    Summary,
}

#[derive(Clone, Debug)]
pub(super) enum ConversationOutcome {
    Completed { usage: Usage },
    Failed { error: String },
    Interrupted,
    GoalEnded { reason: String },
}

#[derive(Clone, Debug)]
pub(super) enum ConversationBlockKind {
    User {
        text: String,
    },
    /// 照 Claude Code 的压缩边界：之前的消息不再发给模型。
    CompactBoundary,
    CompactSummary {
        summary: String,
    },
    Assistant {
        text: String,
        state: AssistantState,
    },
    Thinking {
        text: String,
        source: ThinkingSource,
    },
    ToolCall {
        exchange: ToolExchangeId,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        exchange: ToolExchangeId,
        content: String,
        is_error: bool,
        output_ref: Option<PathBuf>,
    },
    Permission {
        request: PermissionRequest,
        exchange: ToolExchangeId,
    },
    Question {
        request: QuestionRequest,
        exchange: ToolExchangeId,
    },
    Plan {
        plan: Plan,
    },
    Outcome(ConversationOutcome),
    FamilyInput {
        record: FamilyInputRecord,
    },
    FamilyCompletion {
        completion: FamilyCompletion,
    },
    Command {
        command: CommandSnapshot,
    },
}

#[derive(Clone, Debug)]
pub(super) struct ConversationBlock {
    pub(super) id: ConversationBlockId,
    pub(super) metadata: ConversationBlockMetadata,
    pub(super) kind: ConversationBlockKind,
}

#[derive(Clone, Debug)]
pub(super) struct ConversationProjection {
    pub(super) current_model: ConversationModelMetadata,
    pub(super) blocks: Vec<ConversationBlock>,
}

pub(super) fn project(snapshot: &ChatSnapshot) -> ConversationProjection {
    let mut blocks = Vec::new();
    for (message_index, message) in snapshot.messages.iter().enumerate() {
        let compaction = if snapshot.is_compact_boundary(message_index) {
            Some(ConversationBlockKind::CompactBoundary)
        } else {
            snapshot
                .compact_summary_text(message_index)
                .map(|summary| ConversationBlockKind::CompactSummary { summary })
        };
        if let Some(kind) = compaction {
            blocks.push(ConversationBlock {
                id: ConversationBlockId::MessageContent {
                    chat_id: snapshot.id,
                    message_index,
                    content_index: 0,
                },
                metadata: metadata(None, None, None),
                kind,
            });
            continue;
        }
        if message.role == Role::System || snapshot.host_notes.contains_key(&message_index) {
            continue;
        }
        let run_id = snapshot.message_runs.get(&message_index).copied();
        for (content_index, content) in message.content.iter().enumerate() {
            if let Some(block) = message_block(
                snapshot.id,
                message_index,
                content_index,
                run_id,
                message.role,
                content,
            ) {
                blocks.push(block);
            }
        }
    }
    for goal in &snapshot.loops {
        let Some(run_id) = goal.run_id else {
            continue;
        };
        let Some(thinking) = snapshot.pending_thinking.get(&run_id) else {
            continue;
        };
        if !thinking.is_empty() {
            blocks.push(ConversationBlock {
                id: ConversationBlockId::StreamingThinking {
                    chat_id: snapshot.id,
                    run_id,
                },
                metadata: metadata(Some(run_id), Some(goal.id), None),
                kind: ConversationBlockKind::Thinking {
                    text: thinking.clone(),
                    source: ThinkingSource::ModelText,
                },
            });
        }
    }
    let pending = super::workflows::pending_reply(snapshot);
    if !pending.is_empty() {
        let run_id = pending_run(snapshot);
        blocks.push(ConversationBlock {
            id: ConversationBlockId::StreamingAssistant {
                chat_id: snapshot.id,
                run_id,
            },
            metadata: metadata(run_id, None, None),
            kind: ConversationBlockKind::Assistant {
                text: pending,
                state: AssistantState::Streaming,
            },
        });
    }

    blocks.extend(snapshot.permissions.iter().cloned().map(|request| {
        let exchange = exchange(
            snapshot.id,
            request.execution.run_id(),
            &request.execution.call_key(),
        );
        ConversationBlock {
            id: ConversationBlockId::Permission {
                chat_id: snapshot.id,
                request_id: request.id,
            },
            metadata: metadata(request.execution.run_id(), Some(request.loop_id), None),
            kind: ConversationBlockKind::Permission { request, exchange },
        }
    }));
    blocks.extend(snapshot.questions.iter().cloned().map(|request| {
        let exchange = exchange(snapshot.id, Some(request.run_id), &request.call_id);
        ConversationBlock {
            id: ConversationBlockId::Question {
                chat_id: snapshot.id,
                request_id: request.id,
            },
            metadata: metadata(Some(request.run_id), Some(request.loop_id), None),
            kind: ConversationBlockKind::Question { request, exchange },
        }
    }));
    blocks.extend(
        snapshot
            .plans
            .iter()
            .cloned()
            .map(|plan| ConversationBlock {
                id: ConversationBlockId::Plan {
                    chat_id: snapshot.id,
                    loop_id: plan.loop_id,
                    version: plan.version,
                },
                metadata: metadata(None, Some(plan.loop_id), None),
                kind: ConversationBlockKind::Plan { plan },
            }),
    );
    let events = snapshot
        .events
        .iter()
        .chain(snapshot.failure_events.iter().filter(|failure| {
            !snapshot
                .events
                .iter()
                .any(|event| event.run_id == failure.run_id && event.sequence == failure.sequence)
        }));
    for outcome in events.filter_map(|event| {
        matches!(
            event.kind,
            EventKind::Completed { .. }
                | EventKind::Failed(_)
                | EventKind::Interrupted
                | EventKind::GoalEnded { .. }
        )
        .then(|| project_event(event))
        .flatten()
    }) {
        let insertion_index = blocks
            .iter()
            .rposition(|block| block.metadata.run_id == outcome.metadata.run_id)
            .map_or(blocks.len(), |index| index + 1);
        blocks.insert(insertion_index, outcome);
    }
    blocks.extend(
        snapshot
            .commands
            .iter()
            .cloned()
            .map(|command| ConversationBlock {
                id: ConversationBlockId::Command {
                    chat_id: snapshot.id,
                    operation_id: command.operation_id,
                },
                metadata: metadata(None, Some(command.loop_id), None),
                kind: ConversationBlockKind::Command { command },
            }),
    );
    blocks.extend(
        snapshot
            .family_inputs
            .values()
            .cloned()
            .map(|record| ConversationBlock {
                id: ConversationBlockId::FamilyInput {
                    chat_id: snapshot.id,
                    message_id: record.message_id,
                },
                metadata: metadata(None, None, None),
                kind: ConversationBlockKind::FamilyInput { record },
            }),
    );
    blocks.extend(
        snapshot
            .family_completions
            .iter()
            .filter(|completion| {
                completion.notification.is_background()
                    && !snapshot
                        .agent_message_origins
                        .contains_key(&completion.child_run_id)
            })
            .cloned()
            .map(|completion| ConversationBlock {
                id: ConversationBlockId::FamilyCompletion {
                    chat_id: snapshot.id,
                    child_run_id: completion.child_run_id,
                },
                metadata: metadata(None, None, None),
                kind: ConversationBlockKind::FamilyCompletion { completion },
            }),
    );
    ConversationProjection {
        current_model: ConversationModelMetadata {
            model: snapshot.model.model.clone(),
            protocol: snapshot.model.protocol,
        },
        blocks,
    }
}

pub(super) fn project_event(event: &Event) -> Option<ConversationBlock> {
    let event_metadata = metadata(
        Some(event.run_id),
        Some(event.loop_id),
        Some(event.sequence),
    );
    let block = match &event.kind {
        EventKind::FamilyInputUpdated { record } => ConversationBlock {
            id: ConversationBlockId::FamilyInput {
                chat_id: event.chat_id,
                message_id: record.message_id,
            },
            metadata: event_metadata,
            kind: ConversationBlockKind::FamilyInput {
                record: record.clone(),
            },
        },
        EventKind::FamilyRunCompleted { completion } => {
            if !completion.notification.is_background() {
                return None;
            }
            ConversationBlock {
                id: ConversationBlockId::FamilyCompletion {
                    chat_id: event.chat_id,
                    child_run_id: completion.child_run_id,
                },
                metadata: event_metadata,
                kind: ConversationBlockKind::FamilyCompletion {
                    completion: completion.clone(),
                },
            }
        }
        EventKind::Text(text) => ConversationBlock {
            id: ConversationBlockId::Event {
                chat_id: event.chat_id,
                sequence: event.sequence,
            },
            metadata: event_metadata,
            kind: ConversationBlockKind::Assistant {
                text: text.clone(),
                state: AssistantState::Streaming,
            },
        },
        EventKind::Thinking(text) => ConversationBlock {
            id: ConversationBlockId::Event {
                chat_id: event.chat_id,
                sequence: event.sequence,
            },
            metadata: event_metadata,
            kind: ConversationBlockKind::Thinking {
                text: text.clone(),
                source: ThinkingSource::ModelText,
            },
        },
        EventKind::ToolStarted {
            call_id,
            name,
            input,
        } => {
            let exchange = exchange(event.chat_id, Some(event.run_id), call_id);
            ConversationBlock {
                id: tool_call_id(&exchange),
                metadata: event_metadata,
                kind: ConversationBlockKind::ToolCall {
                    exchange,
                    name: name.clone(),
                    input: input.clone(),
                },
            }
        }
        EventKind::ToolCompleted { call_id, result } => {
            let exchange = exchange(event.chat_id, Some(event.run_id), call_id);
            ConversationBlock {
                id: tool_result_id(&exchange),
                metadata: event_metadata,
                kind: ConversationBlockKind::ToolResult {
                    exchange,
                    content: result.content.clone(),
                    is_error: result.is_error,
                    output_ref: result.output_ref.clone(),
                },
            }
        }
        EventKind::PermissionRequested { request } => {
            let exchange = exchange(
                event.chat_id,
                request.execution.run_id(),
                &request.execution.call_key(),
            );
            ConversationBlock {
                id: ConversationBlockId::Permission {
                    chat_id: event.chat_id,
                    request_id: request.id,
                },
                metadata: ConversationBlockMetadata {
                    run_id: request.execution.run_id(),
                    ..event_metadata
                },
                kind: ConversationBlockKind::Permission {
                    request: request.clone(),
                    exchange,
                },
            }
        }
        EventKind::QuestionAsked { request } => {
            let exchange = exchange(event.chat_id, Some(event.run_id), &request.call_id);
            ConversationBlock {
                id: ConversationBlockId::Question {
                    chat_id: event.chat_id,
                    request_id: request.id,
                },
                metadata: event_metadata,
                kind: ConversationBlockKind::Question {
                    request: request.clone(),
                    exchange,
                },
            }
        }
        EventKind::PlanUpdated { plan } => ConversationBlock {
            id: ConversationBlockId::Plan {
                chat_id: event.chat_id,
                loop_id: plan.loop_id,
                version: plan.version,
            },
            metadata: event_metadata,
            kind: ConversationBlockKind::Plan { plan: plan.clone() },
        },
        EventKind::Completed { usage } => outcome_block(
            event,
            ConversationOutcome::Completed {
                usage: usage.clone(),
            },
        ),
        EventKind::Failed(error) => outcome_block(
            event,
            ConversationOutcome::Failed {
                error: error.clone(),
            },
        ),
        EventKind::Interrupted => outcome_block(event, ConversationOutcome::Interrupted),
        EventKind::GoalEnded { reason, .. } => outcome_block(
            event,
            ConversationOutcome::GoalEnded {
                reason: reason.clone(),
            },
        ),
        EventKind::CommandUpdated { command } => ConversationBlock {
            id: ConversationBlockId::Command {
                chat_id: event.chat_id,
                operation_id: command.operation_id,
            },
            metadata: event_metadata,
            kind: ConversationBlockKind::Command {
                command: command.clone(),
            },
        },
        EventKind::AssistantMessageCommitted { .. }
        | EventKind::Started
        | EventKind::ModelRetry { .. }
        | EventKind::InputMerged { .. }
        | EventKind::InputTakenBack
        | EventKind::AgentMessageQueued { .. }
        | EventKind::ConversationRewound { .. }
        | EventKind::ApprovalAssessed { .. }
        | EventKind::AutoReviewerNotice { .. }
        | EventKind::PermissionChecked { .. }
        | EventKind::ContextCompaction { .. }
        | EventKind::ProgressUpdated { .. } => return None,
    };
    Some(block)
}

fn message_block(
    chat_id: Uuid,
    message_index: usize,
    content_index: usize,
    run_id: Option<Uuid>,
    role: Role,
    content: &Content,
) -> Option<ConversationBlock> {
    let id = ConversationBlockId::MessageContent {
        chat_id,
        message_index,
        content_index,
    };
    let kind = match content {
        Content::Text { text } if is_interruption_placeholder(role, text) => return None,
        Content::Text { text } => match role {
            Role::User => ConversationBlockKind::User { text: text.clone() },
            Role::Assistant => ConversationBlockKind::Assistant {
                text: text.clone(),
                state: AssistantState::Complete,
            },
            Role::System => return None,
        },
        Content::ToolUse {
            id: call_id,
            name,
            input,
        } => {
            let exchange = exchange(chat_id, run_id, call_id);
            return Some(ConversationBlock {
                id: tool_call_id(&exchange),
                metadata: metadata(run_id, None, None),
                kind: ConversationBlockKind::ToolCall {
                    exchange,
                    name: name.clone(),
                    input: input.clone(),
                },
            });
        }
        Content::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => {
            let exchange = exchange(chat_id, run_id, tool_use_id);
            return Some(ConversationBlock {
                id: tool_result_id(&exchange),
                metadata: metadata(run_id, None, None),
                kind: ConversationBlockKind::ToolResult {
                    exchange,
                    content: content.text().into_owned(),
                    is_error: *is_error,
                    output_ref: None,
                },
            });
        }
        Content::Thinking { thinking, .. } if role == Role::Assistant && !thinking.is_empty() => {
            ConversationBlockKind::Thinking {
                text: thinking.clone(),
                source: ThinkingSource::ModelText,
            }
        }
        Content::ProtocolData {
            protocol: Protocol::Responses,
            value,
        } if role == Role::Assistant && value["type"] == "reasoning" => {
            let summary = value["summary"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|part| part["type"] == "summary_text")
                .filter_map(|part| part["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n");
            let (text, source) = if summary.is_empty() {
                let reasoning = value["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|part| part["type"] == "reasoning_text")
                    .filter_map(|part| part["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                (reasoning, ThinkingSource::ModelText)
            } else {
                (summary, ThinkingSource::Summary)
            };
            if text.is_empty() {
                return None;
            }
            ConversationBlockKind::Thinking { text, source }
        }
        Content::Thinking { .. } | Content::ProtocolData { .. } | Content::Image { .. } => {
            return None;
        }
    };
    Some(ConversationBlock {
        id,
        metadata: metadata(run_id, None, None),
        kind,
    })
}

fn outcome_block(event: &Event, outcome: ConversationOutcome) -> ConversationBlock {
    ConversationBlock {
        id: ConversationBlockId::Outcome {
            chat_id: event.chat_id,
            sequence: event.sequence,
        },
        metadata: metadata(
            Some(event.run_id),
            Some(event.loop_id),
            Some(event.sequence),
        ),
        kind: ConversationBlockKind::Outcome(outcome),
    }
}

fn pending_run(snapshot: &ChatSnapshot) -> Option<Uuid> {
    snapshot
        .events
        .iter()
        .rev()
        .find_map(|event| matches!(event.kind, EventKind::Text(_)).then_some(event.run_id))
        .or(snapshot.last_model_run)
}

fn exchange(chat_id: Uuid, run_id: Option<Uuid>, call_id: &str) -> ToolExchangeId {
    ToolExchangeId {
        chat_id,
        run_id,
        call_id: call_id.to_owned(),
    }
}

fn tool_call_id(exchange: &ToolExchangeId) -> ConversationBlockId {
    ConversationBlockId::ToolCall {
        exchange: exchange.clone(),
    }
}

fn tool_result_id(exchange: &ToolExchangeId) -> ConversationBlockId {
    ConversationBlockId::ToolResult {
        exchange: exchange.clone(),
    }
}

fn metadata(
    run_id: Option<Uuid>,
    loop_id: Option<Uuid>,
    event_sequence: Option<u64>,
) -> ConversationBlockMetadata {
    ConversationBlockMetadata {
        run_id,
        loop_id,
        model: None,
        timestamp_ms: None,
        event_sequence,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;
    use tickloop_agent_protocol::Message;
    use tickloop_agent_protocol::ToolResult;
    use tickloop_agent_protocol::{Question, QuestionOption};

    use super::*;

    fn snapshot(chat_id: Uuid) -> ChatSnapshot {
        let mut snapshot: ChatSnapshot = serde_json::from_value(json!({
            "id": chat_id,
            "workdir": "/work",
            "messages": [],
            "loops": []
        }))
        .unwrap();
        snapshot.model = tickloop_agent_protocol::ModelConfig {
            provider: tickloop_agent_protocol::ModelProvider::Api,
            auth_source: tickloop_agent_protocol::ModelAuthSource::ApiKeyEnv,
            name: "fixture".into(),
            protocol: Protocol::Messages,
            base_url: "http://127.0.0.1".into(),
            model: "fixture-model".into(),
            api_key_env: String::new(),
            max_output_tokens: 1024,
            provider_id: String::new(),
            provider_name: String::new(),
            model_id: String::new(),
            connection: None,
            card: None,
            input_modalities: vec![tickloop_agent_protocol::InputModality::Text],
        };
        snapshot
    }

    fn event(chat_id: Uuid, loop_id: Uuid, run_id: Uuid, sequence: u64, kind: EventKind) -> Event {
        Event {
            sequence,
            chat_id,
            loop_id,
            run_id,
            kind,
        }
    }

    #[test]
    fn projection_hides_interruption_placeholders_like_claude_code() {
        let chat_id = Uuid::new_v4();
        let mut snapshot = snapshot(chat_id);
        let text = |role, text: &str| Message {
            role,
            content: vec![Content::Text { text: text.into() }],
        };
        snapshot.messages = vec![
            text(Role::User, "interrupted prompt"),
            text(Role::User, "Continue from where you left off."),
            text(Role::Assistant, "No response requested."),
        ];

        let texts = project(&snapshot)
            .blocks
            .into_iter()
            .filter_map(|block| match block.kind {
                ConversationBlockKind::User { text } => Some(text),
                ConversationBlockKind::Assistant { text, .. } => Some(text),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(texts, vec!["interrupted prompt".to_owned()]);
    }

    #[test]
    fn projection_hides_host_notes() {
        let mut snapshot = snapshot(Uuid::new_v4());
        snapshot.messages = vec![
            Message {
                role: Role::User,
                content: vec![Content::Text {
                    text: "Make a doc".into(),
                }],
            },
            Message {
                role: Role::User,
                content: vec![Content::Text {
                    text: "Host notice: the host tool set changed.".into(),
                }],
            },
        ];
        snapshot
            .host_notes
            .insert(1, ["ArtifactRead".to_owned()].into());

        let texts = project(&snapshot)
            .blocks
            .into_iter()
            .filter_map(|block| match block.kind {
                ConversationBlockKind::User { text } => Some(text),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(texts, vec!["Make a doc".to_owned()]);
    }

    #[test]
    fn projection_has_typed_blocks_and_future_metadata_slots() {
        let chat_id = Uuid::new_v4();
        let loop_id = Uuid::new_v4();
        let run_id = Uuid::new_v4();
        let permission_id = Uuid::new_v4();
        let question_id = Uuid::new_v4();
        let mut snapshot = snapshot(chat_id);
        snapshot.messages = vec![
            Message {
                role: Role::User,
                content: vec![Content::Text {
                    text: "prompt".into(),
                }],
            },
            Message {
                role: Role::Assistant,
                content: vec![Content::Text {
                    text: "answer".into(),
                }],
            },
        ];
        snapshot.message_runs = BTreeMap::from([(0, run_id), (1, run_id)]);
        snapshot.permissions.push(PermissionRequest {
            id: permission_id,
            loop_id,
            execution: tickloop_agent_protocol::PermissionExecution::ToolCall {
                run_id,
                call_id: "call".into(),
            },
            name: "Bash".into(),
            input: json!({"command":"pwd"}),
            file_change: None,
            approval_reason: None,
            approval_source: tickloop_agent_protocol::PermissionApprovalSource::default(),
        });
        snapshot.questions.push(QuestionRequest {
            id: question_id,
            chat_id,
            loop_id,
            run_id,
            call_id: "question".into(),
            questions: vec![Question {
                question: "Continue?".into(),
                header: "Continue".into(),
                options: vec![QuestionOption {
                    label: "Yes".into(),
                    description: String::new(),
                    preview: None,
                }],
                multi_select: false,
            }],
        });
        snapshot.plans.push(Plan {
            loop_id,
            version: 2,
            markdown: "Plan".into(),
            approved_execution_mode: None,
            approval_pending: true,
        });
        snapshot.events.push(event(
            chat_id,
            loop_id,
            run_id,
            7,
            EventKind::Failed("failed".into()),
        ));

        let projection = project(&snapshot);
        assert_eq!(projection.current_model.model, "fixture-model");
        assert_eq!(projection.current_model.protocol, Protocol::Messages);
        assert!(
            projection
                .blocks
                .iter()
                .any(|block| matches!(block.kind, ConversationBlockKind::User { .. }))
        );
        assert!(projection.blocks.iter().any(|block| matches!(
            block.kind,
            ConversationBlockKind::Assistant {
                state: AssistantState::Complete,
                ..
            }
        )));
        assert!(
            projection
                .blocks
                .iter()
                .any(|block| matches!(block.kind, ConversationBlockKind::Permission { .. }))
        );
        assert!(
            projection
                .blocks
                .iter()
                .any(|block| matches!(block.kind, ConversationBlockKind::Question { .. }))
        );
        assert!(
            projection
                .blocks
                .iter()
                .any(|block| matches!(block.kind, ConversationBlockKind::Plan { .. }))
        );
        let outcome = projection
            .blocks
            .iter()
            .find(|block| matches!(block.kind, ConversationBlockKind::Outcome(_)))
            .unwrap();
        assert_eq!(outcome.metadata.run_id, Some(run_id));
        assert_eq!(outcome.metadata.loop_id, Some(loop_id));
        assert_eq!(outcome.metadata.event_sequence, Some(7));
        assert_eq!(outcome.metadata.model, None);
        assert_eq!(outcome.metadata.timestamp_ms, None);
    }

    #[test]
    fn tool_exchange_identity_pairs_messages_and_events() {
        let chat_id = Uuid::new_v4();
        let loop_id = Uuid::new_v4();
        let run_id = Uuid::new_v4();
        let mut snapshot = snapshot(chat_id);
        snapshot.messages = vec![
            Message {
                role: Role::Assistant,
                content: vec![Content::ToolUse {
                    id: "call-1".into(),
                    name: "Read".into(),
                    input: json!({"file_path":"notes.md"}),
                }],
            },
            Message {
                role: Role::User,
                content: vec![Content::ToolResult {
                    tool_use_id: "call-1".into(),
                    content: "contents".into(),
                    is_error: false,
                }],
            },
        ];
        snapshot.message_runs = BTreeMap::from([(0, run_id), (1, run_id)]);

        let projection = project(&snapshot);
        let message_exchanges = projection
            .blocks
            .iter()
            .filter_map(|block| match &block.kind {
                ConversationBlockKind::ToolCall { exchange, .. }
                | ConversationBlockKind::ToolResult { exchange, .. } => Some(exchange.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(message_exchanges.len(), 2);
        assert_eq!(message_exchanges[0], message_exchanges[1]);

        let started = event(
            chat_id,
            loop_id,
            run_id,
            90,
            EventKind::ToolStarted {
                call_id: "call-1".into(),
                name: "Read".into(),
                input: json!({"file_path":"notes.md"}),
            },
        );
        let completed = event(
            chat_id,
            loop_id,
            run_id,
            91,
            EventKind::ToolCompleted {
                call_id: "call-1".into(),
                result: ToolResult {
                    content: "contents".into(),
                    is_error: false,
                    output_ref: None,
                },
            },
        );
        let event_exchanges = [
            project_event(&started).unwrap(),
            project_event(&completed).unwrap(),
        ]
        .map(|block| match block.kind {
            ConversationBlockKind::ToolCall { exchange, .. }
            | ConversationBlockKind::ToolResult { exchange, .. } => exchange,
            _ => unreachable!(),
        });
        assert_eq!(event_exchanges[0], event_exchanges[1]);
        assert_eq!(event_exchanges[0], message_exchanges[0]);
    }

    #[test]
    fn stable_ids_do_not_depend_on_retained_event_prefix() {
        let chat_id = Uuid::new_v4();
        let loop_id = Uuid::new_v4();
        let run_id = Uuid::new_v4();
        let mut full = snapshot(chat_id);
        full.messages.push(Message {
            role: Role::User,
            content: vec![Content::Text {
                text: "saved prompt".into(),
            }],
        });
        full.message_runs.insert(0, run_id);
        full.events = vec![
            event(chat_id, loop_id, run_id, 10, EventKind::Started),
            event(
                chat_id,
                loop_id,
                run_id,
                11,
                EventKind::Text("partial".into()),
            ),
            event(
                chat_id,
                loop_id,
                run_id,
                12,
                EventKind::Failed("network".into()),
            ),
        ];
        let mut trimmed = full.clone();
        trimmed.events.remove(0);
        let reopened: ChatSnapshot =
            serde_json::from_value(serde_json::to_value(&trimmed).unwrap()).unwrap();

        let full_projection = project(&full);
        let trimmed_projection = project(&trimmed);
        let reopened_projection = project(&reopened);
        assert_eq!(
            full_projection.blocks[0].id,
            trimmed_projection.blocks[0].id
        );
        assert_eq!(
            trimmed_projection.blocks[0].id,
            reopened_projection.blocks[0].id
        );
        let surviving = &full.events[1];
        assert_eq!(
            project_event(surviving).unwrap().id,
            project_event(&trimmed.events[0]).unwrap().id
        );
    }

    #[test]
    fn historical_outcome_stays_with_its_run_before_later_messages() {
        let chat_id = Uuid::new_v4();
        let loop_id = Uuid::new_v4();
        let failed_run = Uuid::new_v4();
        let later_run = Uuid::new_v4();
        let mut snapshot = snapshot(chat_id);
        snapshot.messages = vec![
            Message {
                role: Role::User,
                content: vec![Content::Text {
                    text: "first prompt".into(),
                }],
            },
            Message {
                role: Role::User,
                content: vec![Content::Text {
                    text: "second prompt".into(),
                }],
            },
            Message {
                role: Role::Assistant,
                content: vec![Content::Text {
                    text: "second answer".into(),
                }],
            },
        ];
        snapshot.message_runs = BTreeMap::from([(0, failed_run), (1, later_run), (2, later_run)]);
        snapshot.events.push(event(
            chat_id,
            loop_id,
            failed_run,
            8,
            EventKind::Failed("first failed".into()),
        ));

        let projection = project(&snapshot);
        let outcome_index = projection
            .blocks
            .iter()
            .position(|block| matches!(block.kind, ConversationBlockKind::Outcome(_)))
            .unwrap();
        let later_prompt_index = projection
            .blocks
            .iter()
            .position(|block| {
                matches!(
                    block.kind,
                    ConversationBlockKind::User { ref text } if text == "second prompt"
                )
            })
            .unwrap();

        assert!(outcome_index < later_prompt_index);
        assert_eq!(
            projection.blocks[outcome_index].metadata.run_id,
            Some(failed_run)
        );
    }
}

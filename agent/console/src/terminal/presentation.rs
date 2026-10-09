use std::collections::{HashMap, HashSet, VecDeque};
use uuid::Uuid;

use serde_json::Value;
use tickloop_agent_tools::{
    ASK_USER_QUESTION_TOOL_NAME, BASH_TOOL_NAME, EDIT_TOOL_NAME, TASK_STOP_TOOL_NAME,
    TODO_WRITE_TOOL_NAME, WEB_FETCH_TOOL_NAME, WEB_SEARCH_TOOL_NAME, WRITE_TOOL_NAME,
};
use tickloop_ui::markdown;

use super::{
    console::Console,
    conversation::{
        AssistantState, ConversationBlock, ConversationBlockKind, ConversationOutcome,
        ThinkingSource, project as project_conversation,
    },
    directed_summary,
    display::{DisplayProfile, Glyph},
    image_cache::ImageLinks,
    safe_text,
    scrollback::ToolCallState,
    style::Role,
    transcript::{LineLink, TranscriptDocument},
};
use tickloop_agent_protocol::{
    ChatSnapshot, CommandState, Event, EventKind, FamilyCompletion, FamilyInputRecord,
    LocalMessage, LoopState, PermissionApprovalSource, PermissionRequest,
    SEND_AGENT_MESSAGE_TOOL_NAME, SUBAGENT_TOOL_NAME, SubagentNotificationDelivery,
    TickDeliveryLine, ToolResult,
};

const RETAINED_TOOL_EVENTS: usize = 128;
const PREVIEW_LINES: usize = 5;
const PREVIEW_LINE_CHARACTERS: usize = 200;
const TOOL_SUMMARY_CHARACTERS: usize = 160;
const BACKGROUND_RUNNING_STATUS: &str = "Running in background";

pub(super) fn is_model_only_tool(name: &str) -> bool {
    name == TODO_WRITE_TOOL_NAME
}
const BACKGROUND_LAUNCH_PREFIXES: [&str; 2] = [
    "Command running in background with ID: ",
    "Command was manually backgrounded by user with ID: ",
];
/// 照 Claude Code 的 `InterruptedByUser` 呈现中断标记。
const RESPONSE_CONTINUATION: &str = "     ";
#[cfg(test)]
const INTERRUPTED_NOTICE: &str = "  ⎿  Interrupted · What should the Agent do instead?";

#[derive(Clone, Copy)]
pub(super) enum OutputDetail {
    Preview,
    Full,
}

#[derive(Default)]
pub(super) struct MessageGroups {
    replies: HashMap<Uuid, (u64, bool)>,
    pub(super) displayed: HashSet<String>,
}

impl MessageGroups {
    pub(super) fn message_id(&mut self, event: &Event) -> Option<String> {
        let run = event.run_id;
        let key = match &event.kind {
            EventKind::Text(_) | EventKind::Thinking(_) => {
                let (index, after_tool) = self.replies.entry(run).or_default();
                if *after_tool {
                    *index += 1;
                    *after_tool = false;
                }
                if matches!(event.kind, EventKind::Thinking(_)) {
                    format!("thinking:{index}")
                } else {
                    format!("assistant:{index}")
                }
            }
            EventKind::ToolStarted { call_id, .. } | EventKind::ToolCompleted { call_id, .. } => {
                self.replies.entry(run).or_default().1 = true;
                format!("tool:{call_id}")
            }
            EventKind::PermissionRequested { request } => {
                format!("tool:{}", request.execution.call_key())
            }
            EventKind::QuestionAsked { request } => format!("question:{}", request.id),
            EventKind::PlanUpdated { plan } => format!("plan:{}:{}", plan.loop_id, plan.version),
            EventKind::ProgressUpdated { progress } => format!("progress:{}", progress.loop_id),
            EventKind::CommandUpdated { command } => {
                format!("command:{}:{:?}", command.operation_id, command.state)
            }
            EventKind::Failed(_) | EventKind::Interrupted | EventKind::GoalEnded { .. } => {
                "outcome".into()
            }
            _ => return None,
        };
        Some(format!("{run}:{key}"))
    }
}

pub(super) fn transcript_document(
    snapshot: &ChatSnapshot,
    display: DisplayProfile,
    image_links: &ImageLinks,
) -> TranscriptDocument {
    let projection = project_conversation(snapshot);
    let mut document = TranscriptDocument {
        lines: vec![format!(
            "Full archive · Conversation · {} · {:?}",
            safe_text(&projection.current_model.model),
            projection.current_model.protocol
        )],
        ..TranscriptDocument::default()
    };
    let mut interaction_notice_rendered = false;
    let mut command_heading_rendered = false;
    let mut tool_names = HashMap::new();
    for block in projection.blocks {
        let ConversationBlock {
            id: block_id,
            metadata,
            kind,
        } = block;
        let mut text = String::new();
        let mut is_prompt = false;
        let is_failure = matches!(
            &kind,
            ConversationBlockKind::Outcome(ConversationOutcome::Failed { .. })
        );
        match kind {
            ConversationBlockKind::User { text: content } => {
                let is_agent_message = match block_id {
                    super::conversation::ConversationBlockId::MessageContent {
                        message_index,
                        ..
                    } => snapshot
                        .message_ids
                        .get(&message_index)
                        .is_some_and(|id| snapshot.agent_message_origins.contains_key(id)),
                    _ => false,
                };
                if let Some(message) = typed_local_message(&content, is_agent_message) {
                    text.push_str(&local_message_text(&message, OutputDetail::Full, display));
                } else {
                    document.prompts.push(document.lines.len() + 1);
                    if let super::conversation::ConversationBlockId::MessageContent {
                        message_index,
                        ..
                    } = block_id
                        && let Some(origin) = snapshot
                            .message_ids
                            .get(&message_index)
                            .and_then(|id| snapshot.human_family_origins.get(id))
                    {
                        text.push_str(&format!(
                            "\nUser message · from @{} View\nOriginal input: {}\n",
                            safe_text(&origin.source_handle),
                            safe_text(&origin.original_submission)
                        ));
                    }
                    text.push_str(&format!("\nYou: {}\n", safe_text(&content)));
                    is_prompt = true;
                }
            }
            ConversationBlockKind::CompactBoundary => {
                text.push_str("\nConversation compacted\n");
            }
            ConversationBlockKind::CompactSummary { summary } => {
                text.push_str(&directed_summary::history_marker(&summary));
            }
            ConversationBlockKind::Assistant {
                text: content,
                state,
            } => match state {
                AssistantState::Complete => {
                    text.push_str(&format!(
                        "\nAssistant: {}\n",
                        markdown::plain_text(&content)
                    ));
                }
                AssistantState::Streaming => {
                    text.push_str(&format!("\nAssistant: {}", markdown::plain_text(&content)));
                }
            },
            ConversationBlockKind::Thinking {
                text: content,
                source,
            } => {
                let heading = match source {
                    ThinkingSource::ModelText => "∴ Thinking…",
                    ThinkingSource::Summary => "∴ Thinking summary",
                };
                text.push_str(&format!("\n{heading}\n\n"));
                for line in markdown::plain_text(&safe_text(&content)).lines() {
                    text.push_str("  ");
                    text.push_str(line);
                    text.push('\n');
                }
            }
            ConversationBlockKind::ToolCall {
                exchange,
                name,
                input,
            } => {
                tool_names.insert(exchange.clone(), name.clone());
                if is_model_only_tool(&name) {
                    continue;
                }
                text.push_str(&format!(
                    "\nTool {} · {}\n",
                    safe_text(&exchange.call_id),
                    tool_name(&name)
                ));
                append_transcript_value(&mut text, &input);
            }
            ConversationBlockKind::ToolResult {
                exchange,
                content,
                is_error,
                output_ref,
            } => {
                let _output_ref = output_ref;
                if !is_error
                    && tool_names
                        .get(&exchange)
                        .is_some_and(|name| is_model_only_tool(name))
                {
                    continue;
                }
                let result = ToolResult {
                    content: content.clone(),
                    is_error,
                    output_ref: None,
                };
                let status = tool_result_status(
                    tool_names
                        .get(&exchange)
                        .map(String::as_str)
                        .unwrap_or("tool"),
                    &result,
                );
                text.push_str(&format!(
                    "\nResult {} · {}\n",
                    safe_text(&exchange.call_id),
                    status
                ));
                if let Ok(value) = serde_json::from_str::<Value>(&content) {
                    append_transcript_value(&mut text, &value);
                } else {
                    text.push_str(&safe_text(&content));
                    text.push('\n');
                }
            }
            ConversationBlockKind::Permission { request, exchange } => {
                let (_request, _exchange) = (request, exchange);
                if !interaction_notice_rendered {
                    text.push_str(
                        "\n\nA response is waiting. Return to the conversation to answer.\n",
                    );
                    interaction_notice_rendered = true;
                }
            }
            ConversationBlockKind::Question { request, exchange } => {
                let (_request, _exchange) = (request, exchange);
                if !interaction_notice_rendered {
                    text.push_str(
                        "\n\nA response is waiting. Return to the conversation to answer.\n",
                    );
                    interaction_notice_rendered = true;
                }
            }
            ConversationBlockKind::Plan { plan } => {
                let _plan = plan;
            }
            ConversationBlockKind::FamilyInput { record } => {
                text.push_str(&family_input_text(&record, OutputDetail::Full))
            }
            ConversationBlockKind::FamilyCompletion { completion } => {
                text.push_str(&family_completion_text(&completion, OutputDetail::Full))
            }
            ConversationBlockKind::Outcome(outcome) => match outcome {
                ConversationOutcome::Failed { error } => text.push_str(&format!(
                    "\nFailed · goal {}: {}\n",
                    metadata.loop_id.unwrap_or_default(),
                    safe_text(&error)
                )),
                ConversationOutcome::Interrupted => text.push_str(&format!(
                    "\nInterrupted · goal {}\n",
                    metadata.loop_id.unwrap_or_default()
                )),
                ConversationOutcome::Completed { usage } => {
                    let _usage = usage;
                }
                ConversationOutcome::GoalEnded { reason } => {
                    let _reason = reason;
                }
            },
            ConversationBlockKind::Command { command } => {
                if !command_heading_rendered {
                    text.push_str("\nShell tasks:\n");
                    command_heading_rendered = true;
                }
                text.push_str(&format!(
                    "\n{} · {:?}{}\n{}\nDirectory: {}\nOutput: {} bytes\nFull output: {}\n",
                    command.operation_id,
                    command.state,
                    command_status(command.exit_code, command.signal),
                    safe_text(&command.command),
                    safe_text(&command.cwd.to_string_lossy()),
                    command.output_bytes,
                    safe_text(&command.output_ref.to_string_lossy()),
                ));
                if let Some(reason) = &command.reason {
                    text.push_str(&format!("Reason: {}\n", safe_text(reason)));
                }
            }
        }
        let first_line = document.lines.len();
        document.lines.extend(text.lines().map(str::to_owned));
        if is_failure {
            for (line, text) in document.lines.iter().enumerate().skip(first_line) {
                document.links.extend(
                    super::hyperlink::web_links(text)
                        .into_iter()
                        .map(|(range, link)| LineLink { line, range, link }),
                );
            }
        }
        if is_prompt {
            for (line, text) in document.lines.iter().enumerate().skip(first_line) {
                let links = image_links.in_text(snapshot.id, text);
                document
                    .links
                    .extend(
                        links
                            .into_iter()
                            .map(|(range, link)| LineLink { line, range, link }),
                    );
            }
        }
    }
    document
}

fn append_transcript_value(text: &mut String, value: &Value) {
    if let Value::Object(fields) = value {
        for (key, value) in fields {
            text.push_str(&format!("{}:\n{}\n", safe_text(key), display_value(value)));
        }
    } else {
        text.push_str(&display_value(value));
        text.push('\n');
    }
}

pub(super) struct ToolPresentation {
    detailed: bool,
    events: VecDeque<Event>,
}

impl ToolPresentation {
    pub(super) fn present_event(
        &mut self,
        console: &Console,
        event: &Event,
    ) -> Result<bool, String> {
        self.record(event);
        match &event.kind {
            EventKind::ToolCompleted { call_id, .. } => console
                .finish_tool_call(event.run_id, Some(&tool_call_key(event.run_id, call_id)))?,
            EventKind::Completed { .. } | EventKind::Failed(_) | EventKind::Interrupted => {
                console.finish_tool_call(event.run_id, None)?
            }
            EventKind::AssistantMessageCommitted { tool_calls, .. } if console.is_interactive() => {
                let _frame_hold = console.hold_frames()?;
                for call in tool_calls {
                    if is_model_only_tool(&call.name) {
                        continue;
                    }
                    let key = tool_call_key(event.run_id, &call.call_id);
                    console.for_message(Some(key.clone())).write_tool_call(
                        event.run_id,
                        &key,
                        &tool_call_roles(&call.name, &call.input, console.display_profile()?),
                        ToolCallState::Queued,
                    )?;
                }
            }
            _ => {}
        }
        let Some(rendered) = self.render_event_roles(event, console.display_profile()?) else {
            return Ok(false);
        };
        match &event.kind {
            EventKind::ToolStarted {
                call_id,
                name,
                input,
            } if console.is_interactive() && !is_model_only_tool(name) => {
                console.write_tool_call(
                    event.run_id,
                    &tool_call_key(event.run_id, call_id),
                    &tool_call_roles(name, input, console.display_profile()?),
                    ToolCallState::Running,
                )?;
                if self.is_detailed() {
                    console.write(&format!(
                        "{}\n",
                        safe_text(
                            &serde_json::to_string_pretty(input)
                                .map_err(|error| error.to_string())?
                        )
                    ))?;
                }
            }
            _ => console.write_roles(&rendered)?,
        }
        Ok(true)
    }

    pub(super) fn new(detailed: bool) -> Self {
        Self {
            detailed,
            events: VecDeque::new(),
        }
    }

    pub(super) fn is_detailed(&self) -> bool {
        self.detailed
    }

    pub(super) fn restore(&mut self, events: &[Event]) {
        self.events.clear();
        for event in events {
            self.record(event);
        }
    }

    pub(super) fn record(&mut self, event: &Event) {
        if matches!(
            event.kind,
            EventKind::AssistantMessageCommitted { .. }
                | EventKind::ToolStarted { .. }
                | EventKind::ToolCompleted { .. }
                | EventKind::CommandUpdated { .. }
        ) {
            if self.events.len() == RETAINED_TOOL_EVENTS {
                self.events.pop_front();
            }
            self.events.push_back(event.clone());
        }
    }

    pub(super) fn render_event(&self, event: &Event) -> Option<String> {
        match &event.kind {
            EventKind::ToolStarted { name, .. } if is_model_only_tool(name) => {
                return Some(String::new());
            }
            EventKind::ToolCompleted { call_id, result }
                if !result.is_error && is_model_only_tool(self.started_call(event, call_id).0) =>
            {
                return Some(String::new());
            }
            _ => {}
        }
        if self.detailed {
            let name = match &event.kind {
                EventKind::ToolCompleted { call_id, .. } => self.started_call(event, call_id).0,
                _ => "tool",
            };
            return detailed_event(event, name);
        }
        match &event.kind {
            EventKind::ToolStarted { name, input, .. } => {
                let summary = tool_summary(name, input);
                Some(format!("\n● {}{summary}\n", tool_name(name)))
            }
            EventKind::ToolCompleted { call_id, result } => {
                let (name, input) = self.started_call(event, call_id);
                Some(compact_result(name, input, result))
            }
            EventKind::CommandUpdated { command } => {
                // The Bash result already reports a foreground command's outcome and the start of
                // a background one; only what happens to a background command afterwards is new.
                if !command.was_background
                    || matches!(
                        command.state,
                        CommandState::Running | CommandState::Background
                    )
                {
                    return Some(String::new());
                }
                let mut text = format!(
                    "\n  ↳ Shell · {:?}{} · {}\n",
                    command.state,
                    command_status(command.exit_code, command.signal),
                    clipped(&command.command, TOOL_SUMMARY_CHARACTERS).0,
                );
                if let Some(reason) = &command.reason {
                    text.push_str(&format!("  Reason: {}\n", safe_text(reason)));
                }
                if command.output_bytes > 0 {
                    text.push_str(&format!(
                        "  {} output bytes · Ctrl+O for details\n",
                        command.output_bytes
                    ));
                }
                Some(text)
            }
            _ => None,
        }
    }

    /// The same output as [`Self::render_event`], split into the roles its
    /// parts play: a dim bullet, a bold tool name and a plain argument summary
    /// for a call; a dim result block, or an error-coloured one on failure.
    pub(super) fn render_event_roles(
        &self,
        event: &Event,
        display: DisplayProfile,
    ) -> Option<Vec<(String, Option<Role>)>> {
        if matches!(&event.kind, EventKind::ToolStarted { name, .. } if is_model_only_tool(name))
            || matches!(&event.kind, EventKind::ToolCompleted { call_id, result } if !result.is_error && is_model_only_tool(self.started_call(event, call_id).0))
        {
            return Some(Vec::new());
        }
        if self.detailed {
            return self
                .render_event(event)
                .map(|rendered| vec![(rendered, None)]);
        }
        let rendered = match &event.kind {
            EventKind::ToolCompleted { call_id, result } => {
                let (name, input) = self.started_call(event, call_id);
                compact_result_with_prefix(name, input, result, display.glyph(Glyph::ToolResult))
            }
            _ => self.render_event(event)?,
        };
        Some(match &event.kind {
            EventKind::ToolStarted { name, input, .. } => tool_call_roles(name, input, display),
            EventKind::ToolCompleted { result, .. } if result.is_error => {
                vec![(rendered, Some(Role::Error))]
            }
            _ => vec![(rendered, Some(Role::MutedText))],
        })
    }

    /// The name and input of the recorded call a result of `event`'s Run answers; `tool` without
    /// input once the call has left the retained events.
    fn started_call<'a>(&'a self, event: &Event, call_id: &str) -> (&'a str, Option<&'a Value>) {
        self.events
            .iter()
            .rev()
            .find_map(|recorded| match &recorded.kind {
                EventKind::AssistantMessageCommitted { tool_calls, .. }
                    if recorded.run_id == event.run_id =>
                {
                    tool_calls
                        .iter()
                        .find(|call| call.call_id == call_id)
                        .map(|call| (call.name.as_str(), Some(&call.input)))
                }
                EventKind::ToolStarted {
                    call_id: recorded_call,
                    name,
                    input,
                } if recorded.run_id == event.run_id && recorded_call == call_id => {
                    Some((name.as_str(), Some(input)))
                }
                _ => None,
            })
            .unwrap_or(("tool", None))
    }

    pub(super) fn toggle(&mut self) -> String {
        self.detailed = !self.detailed;
        if !self.detailed {
            return "\nCompact tool output enabled. Ctrl+O shows details.\n".into();
        }
        let mut text =
            "\nDetailed tool output enabled. Ctrl+O returns to compact output.\n".to_owned();
        if self.events.is_empty() {
            text.push_str("No tool activity in this view yet.\n");
        }
        for event in &self.events {
            if let Some(detail) = self.render_event(event) {
                text.push_str(&detail);
            }
        }
        text
    }
}

pub(super) fn tool_call_key(run: Uuid, call_id: &str) -> String {
    format!("{run}:tool:{call_id}")
}

pub(super) fn tool_call_roles(
    name: &str,
    input: &Value,
    display: DisplayProfile,
) -> Vec<(String, Option<Role>)> {
    vec![
        ("\n".into(), None),
        (
            format!("{} ", display.glyph(Glyph::ToolCall)),
            Some(Role::MutedText),
        ),
        (super::safe_line(&tool_name(name)), Some(Role::ToolName)),
        (super::safe_line(&tool_summary(name, input)), None),
        ("\n".into(), None),
    ]
}

pub(super) fn tool_call_states(snapshot: &ChatSnapshot) -> HashMap<(Uuid, String), ToolCallState> {
    use tickloop_agent_protocol::Content;
    use tickloop_agent_protocol::{LoopState, OperationState};
    let active: HashSet<_> = snapshot
        .loops
        .iter()
        .filter(|goal| {
            matches!(
                goal.state,
                LoopState::Running
                    | LoopState::Queued
                    | LoopState::WaitingPermission
                    | LoopState::WaitingQuestion
                    | LoopState::WaitingPlan
            )
        })
        .filter_map(|goal| goal.run_id)
        .collect();
    let mut states = HashMap::new();
    for operation in &snapshot.operations {
        let state = match operation.state {
            OperationState::Proposed if active.contains(&operation.run_id) => ToolCallState::Queued,
            OperationState::Proposed => ToolCallState::Stopped,
            OperationState::Admitted => ToolCallState::Running,
            _ => ToolCallState::Finished,
        };
        states.insert((operation.run_id, operation.call_id.clone()), state);
    }
    for (index, message) in snapshot.messages.iter().enumerate() {
        let Some(run) = snapshot.message_runs.get(&index).copied() else {
            continue;
        };
        for content in &message.content {
            match content {
                Content::ToolUse { id, .. } => {
                    states
                        .entry((run, id.clone()))
                        .or_insert(if active.contains(&run) {
                            ToolCallState::Queued
                        } else {
                            ToolCallState::Finished
                        });
                }
                Content::ToolResult { tool_use_id, .. } => {
                    states
                        .entry((run, tool_use_id.clone()))
                        .and_modify(|state| {
                            if *state != ToolCallState::Stopped {
                                *state = ToolCallState::Finished;
                            }
                        })
                        .or_insert(ToolCallState::Finished);
                }
                _ => {}
            }
        }
    }
    for recovery in &snapshot.tool_call_recoveries {
        if let Some(run) = recovery.run_id {
            states.insert(
                (run, recovery.call_id.clone()),
                if recovery.was_admitted == Some(true) {
                    ToolCallState::Finished
                } else {
                    ToolCallState::Stopped
                },
            );
        }
    }
    // Recorded recovery facts survive eviction of the event replay. ToolResults
    // synthesized for interruption are not evidence of admission/completion.
    for event in &snapshot.events {
        match &event.kind {
            EventKind::AssistantMessageCommitted { tool_calls, .. } => {
                for call in tool_calls {
                    states.insert((event.run_id, call.call_id.clone()), ToolCallState::Queued);
                }
            }
            EventKind::ToolStarted { call_id, .. } => {
                states.insert((event.run_id, call_id.clone()), ToolCallState::Running);
            }
            EventKind::ToolCompleted { call_id, .. } => {
                states.insert((event.run_id, call_id.clone()), ToolCallState::Finished);
            }
            EventKind::Completed { .. } | EventKind::Failed(_) | EventKind::Interrupted => {
                for ((run, _), state) in &mut states {
                    if *run == event.run_id {
                        *state = state.after_run();
                    }
                }
            }
            _ => {}
        }
    }
    states
}

pub(super) fn tool_name(name: &str) -> String {
    safe_text(match name {
        "tool" => "Tool",
        other => other,
    })
}

pub(super) fn permission_context(request: &PermissionRequest) -> Vec<String> {
    let mut lines = vec![format!(
        "{} is waiting for your approval.",
        tool_name(&request.name)
    )];
    if let Some(reason) = request.approval_reason.as_deref() {
        lines.push(format!("Reason: {}", safe_text(reason)));
        lines.push(match request.approval_source {
            PermissionApprovalSource::SafetyCheck => {
                "Enforced by: safety check · saved rules cannot bypass".into()
            }
            PermissionApprovalSource::AutoReviewer => "Enforced by: Auto reviewer".into(),
            PermissionApprovalSource::ExplicitAsk => {
                "Enforced by: explicit Ask rule · approval applies once".into()
            }
            PermissionApprovalSource::PermissionPolicy => "Enforced by: permission policy".into(),
        });
    }
    if let Some(prepared) = request.file_change.as_deref() {
        let summary = &prepared.summary;
        lines.extend([
            format!("Action: {:?}", summary.action),
            format!("Path: {}", safe_text(&summary.path.to_string_lossy())),
            format!(
                "Changes: +{} -{} in {} hunks",
                summary.added_lines, summary.removed_lines, summary.hunk_count
            ),
            format!(
                "Bytes: {} -> {} · lines: {} -> {} · EOF newline: {} -> {}",
                summary.before.byte_len,
                summary.after.byte_len,
                summary.before.line_count,
                summary.after.line_count,
                summary.before.ends_with_newline,
                summary.after.ends_with_newline
            ),
            format!(
                "Review artifact: {}",
                safe_text(&prepared.artifact.path.to_string_lossy())
            ),
        ]);
        return lines;
    }
    if let Value::Object(fields) = &request.input {
        let first_fields = [
            "command",
            "cwd",
            "file_path",
            "path",
            "old_string",
            "new_string",
            "content",
        ];
        for key in first_fields
            .iter()
            .copied()
            .filter(|key| fields.contains_key(*key))
            .chain(
                fields
                    .keys()
                    .map(String::as_str)
                    .filter(|key| !first_fields.contains(key)),
            )
        {
            let label = match key {
                "command" => "Command",
                "cwd" => "Working directory",
                "file_path" | "path" => "Path",
                "old_string" => "Old text (replace)",
                "new_string" => "New text (replacement)",
                "content" => "New complete file content",
                other => other,
            };
            let value = display_value(&fields[key]);
            lines.push(format!("{}:", safe_text(label)));
            lines.extend(value.split('\n').map(str::to_owned));
        }
    } else {
        lines.push("Input:".into());
        lines.extend(display_value(&request.input).split('\n').map(str::to_owned));
    }
    lines
}

fn detailed_event(event: &Event, name: &str) -> Option<String> {
    match &event.kind {
        EventKind::ToolStarted {
            call_id,
            name,
            input,
        } => Some(format!(
            "\nTool {} · {} · {}\n",
            safe_text(call_id),
            safe_text(name),
            safe_text(&input.to_string()),
        )),
        EventKind::ToolCompleted { call_id, result } => {
            let mut text = format!(
                "\nTool {} · {}\n{}\n",
                safe_text(call_id),
                tool_result_status(name, result),
                safe_text(&result.content),
            );
            if let Some(path) = &result.output_ref {
                text.push_str(&format!(
                    "Full output: {}\n",
                    safe_text(&path.to_string_lossy()),
                ));
            }
            Some(text)
        }
        EventKind::CommandUpdated { command } => {
            let mut text = format!(
                "\nCommand: {} · {:?} · PID {:?} · goal {}\n{}\nDirectory: {}\nExit code: {:?} · Signal: {:?} · Output bytes: {}\nFull output: {}\n",
                command.operation_id,
                command.state,
                command.pid,
                command.loop_id,
                safe_text(&command.command),
                safe_text(&command.cwd.to_string_lossy()),
                command.exit_code,
                command.signal,
                command.output_bytes,
                safe_text(&command.output_ref.to_string_lossy()),
            );
            if let Some(reason) = &command.reason {
                text.push_str(&format!("Reason: {}\n", safe_text(reason)));
            }
            Some(text)
        }
        _ => None,
    }
}

fn tool_summary(name: &str, input: &Value) -> String {
    let asked_questions;
    let primary = match name {
        SEND_AGENT_MESSAGE_TOOL_NAME => input["message"].as_str(),
        BASH_TOOL_NAME => input["description"]
            .as_str()
            .filter(|description| !description.trim().is_empty())
            .or_else(|| input["command"].as_str()),
        TASK_STOP_TOOL_NAME => input["task_id"]
            .as_str()
            .or_else(|| input["shell_id"].as_str()),
        "Glob" | "Grep" => input["pattern"].as_str(),
        WEB_SEARCH_TOOL_NAME => input["query"].as_str(),
        WEB_FETCH_TOOL_NAME => input["url"].as_str(),
        ASK_USER_QUESTION_TOOL_NAME => {
            asked_questions = input["questions"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|question| question["question"].as_str())
                .collect::<Vec<_>>()
                .join(" · ");
            Some(asked_questions.as_str())
        }
        tickloop_agent_tools::START_GOAL_TOOL_NAME => input["goal"].as_str(),
        "Read" | "Write" | "Edit" => input["file_path"].as_str(),
        _ => input["path"].as_str(),
    };
    let Some(primary) = primary else {
        return String::new();
    };
    let summary = if matches!(name, "Glob" | "Grep") {
        format!("{primary} in {}", input["path"].as_str().unwrap_or("."))
    } else {
        primary.to_owned()
    };
    format!(
        " · {}",
        clipped(&summary.replace('\n', " ↵ "), TOOL_SUMMARY_CHARACTERS).0
    )
}

pub(super) fn archived_tool_call(name: &str, input: &Value, display: DisplayProfile) -> String {
    format!(
        "\n{} {}{}\n",
        display.glyph(Glyph::ToolCall),
        tool_name(name),
        tool_summary(name, input)
    )
}

pub(super) fn archived_tool_result(
    name: &str,
    input: Option<&Value>,
    result: &ToolResult,
    display: DisplayProfile,
) -> String {
    compact_result_with_prefix(name, input, result, display.glyph(Glyph::ToolResult))
}

/// 本地消息的对话呈现，照 Claude Code：`!` 命令、`⎿` 输出块、中断标记与任务通知。
pub(super) fn typed_local_message(text: &str, is_agent_message: bool) -> Option<LocalMessage> {
    LocalMessage::parse(text).filter(|message| {
        !matches!(message, LocalMessage::AgentCommunication { .. }) || is_agent_message
    })
}

pub(super) fn local_message_text(
    message: &LocalMessage,
    detail: OutputDetail,
    display: DisplayProfile,
) -> String {
    match message {
        LocalMessage::AgentCommunication {
            sender_handle,
            sender_label,
            relay,
            message,
            is_result,
        } => {
            if matches!(detail, OutputDetail::Preview) {
                return format!(
                    "\nMessage from @{} · Ctrl+O to expand\n",
                    safe_text(sender_handle)
                );
            }
            let body = if *is_result {
                serde_json::from_str::<Value>(message)
                    .ok()
                    .map(|value| {
                        format!(
                            "{}\n{}\n{}",
                            value["status"].as_str().unwrap_or("Result"),
                            value["result"].as_str().unwrap_or_default(),
                            value["error"].as_str().unwrap_or_default()
                        )
                    })
                    .unwrap_or_else(|| message.clone())
            } else {
                message.clone()
            };
            let label = if sender_label.is_empty() {
                String::new()
            } else {
                format!(" · {}", safe_text(sender_label))
            };
            let mut text = format!("\nMessage from @{}{label}:\n", safe_text(sender_handle));
            if let Some(relay) = relay {
                text.push_str(&format!(
                    "Verified user relay · {}\n{}\n",
                    if relay.forwarded_verbatim {
                        "verbatim"
                    } else {
                        "paraphrased"
                    },
                    safe_text(&relay.original_text)
                ));
            }
            append_response_block(&mut text, &body, detail, display);
            text
        }
        LocalMessage::BashInput { command } => format!("\n! {}\n", safe_text(command)),
        LocalMessage::BashOutput { stdout, stderr } => {
            let mut text = String::new();
            if matches!(detail, OutputDetail::Preview) && background_launch_id(stdout).is_some() {
                append_response_block(&mut text, BACKGROUND_RUNNING_STATUS, detail, display);
                return text;
            }
            for output in [stdout, stderr] {
                append_response_block(&mut text, output.trim_end(), detail, display);
            }
            if text.is_empty() {
                append_response_block(&mut text, "(No output)", detail, display);
            }
            text
        }
        LocalMessage::Interrupted { .. } => format!("{}\n", interrupted_notice(display)),
        LocalMessage::TaskNotification {
            summary,
            result,
            usage,
            ..
        } => {
            let mut text = format!(
                "\n{} {}\n",
                display.glyph(Glyph::ToolCall),
                safe_text(summary)
            );
            if matches!(detail, OutputDetail::Full) {
                if let Some(result) = result {
                    append_response_block(&mut text, result, detail, display);
                }
                if let Some(usage) = usage {
                    text.push_str(&format!("Usage{}\n", super::usage_text(usage)));
                }
            }
            text
        }
        LocalMessage::TickDelivery { items, truncated } => {
            tick_delivery_text(items, *truncated, display)
        }
    }
}

/// One line per Tick Instance in a delivery: the Tick Instance, the Tick Update sequence and how
/// many updates were dropped before it.
pub(super) fn tick_delivery_text(
    items: &[TickDeliveryLine],
    truncated: bool,
    display: DisplayProfile,
) -> String {
    let mut text = String::new();
    for item in items {
        text.push_str(&format!(
            "\n{} {} #{}",
            display.glyph(Glyph::TickDelivery),
            item.instance,
            item.sequence
        ));
        if item.dropped > 0 {
            text.push_str(&format!(" · dropped {}", item.dropped));
        }
        if truncated {
            text.push_str(" · truncated");
        }
        text.push('\n');
    }
    text
}

pub(super) fn interrupted_notice(display: DisplayProfile) -> String {
    format!(
        "  {}  Interrupted · What should the Agent do instead?",
        display.glyph(Glyph::ToolResult)
    )
}

fn append_response_block(
    output: &mut String,
    content: &str,
    detail: OutputDetail,
    display: DisplayProfile,
) {
    if content.is_empty() {
        return;
    }
    let sanitized = safe_text(content);
    let line_limit = match detail {
        OutputDetail::Preview => PREVIEW_LINES,
        OutputDetail::Full => usize::MAX,
    };
    let mut lines = sanitized.lines();
    for (index, line) in lines.by_ref().take(line_limit).enumerate() {
        let prefix = if index == 0 {
            format!("  {}  ", display.glyph(Glyph::ToolResult))
        } else {
            RESPONSE_CONTINUATION.into()
        };
        let line = match detail {
            OutputDetail::Preview => clipped(line, PREVIEW_LINE_CHARACTERS).0,
            OutputDetail::Full => line.to_owned(),
        };
        output.push_str(&format!("{prefix}{line}\n"));
    }
    if lines.next().is_some() {
        output.push_str(&format!("{RESPONSE_CONTINUATION}… Ctrl+O for details\n"));
    }
}

fn compact_result(name: &str, input: Option<&Value>, result: &ToolResult) -> String {
    compact_result_with_prefix(name, input, result, "↳")
}

fn compact_result_with_prefix(
    name: &str,
    input: Option<&Value>,
    result: &ToolResult,
    prefix: &str,
) -> String {
    if !result.is_error
        && let Some(input) = input
        && let Some(change) = file_change_summary(name, input)
    {
        let mut text = format!("  {prefix} {} · Completed · {change}\n", tool_name(name));
        if let Some(path) = input["file_path"].as_str() {
            text.push_str(&format!("  {}\n", clipped(path, PREVIEW_LINE_CHARACTERS).0));
        }
        return text;
    }
    if name == BASH_TOOL_NAME {
        let status = tool_result_status(name, result);
        let mut text = format!("  {prefix} {} · {status}\n", tool_name(name));
        if status == BACKGROUND_RUNNING_STATUS {
            if let Some(command) = input.and_then(|input| input["command"].as_str()) {
                text.push_str(&format!("  {}\n", background_shell_line(command)));
            }
            return text;
        }
        append_preview(&mut text, &result.content, false);
        return text;
    }
    let value = serde_json::from_str::<Value>(&result.content).ok();
    if matches!(name, SUBAGENT_TOOL_NAME | SEND_AGENT_MESSAGE_TOOL_NAME)
        && let Some(value) = &value
    {
        let status = match value["status"].as_str() {
            Some("async_launched") => "Child started in background",
            Some("accepted") => "Message sent",
            Some("completed") => "Child completed",
            Some("failed") => "Child failed",
            Some("interrupted") => "Child interrupted",
            other => other.unwrap_or("Completed"),
        };
        let mut text = format!("  {prefix} {} · {status}\n", tool_name(name));
        if name == SEND_AGENT_MESSAGE_TOOL_NAME
            && let Some(label) = value["target_label"].as_str()
        {
            let relationship = if value["target_relationship"] == "parent" {
                "Main Agent"
            } else {
                "Child Agent"
            };
            text.push_str(&format!(
                "  {relationship} · {}\n",
                clipped(label, PREVIEW_LINE_CHARACTERS).0
            ));
        }
        if let Some(body) = value["result"].as_str().filter(|body| !body.is_empty()) {
            append_preview(&mut text, body, false);
        }
        if let Some(error) = value["error"].as_str().filter(|error| !error.is_empty()) {
            append_preview(&mut text, error, false);
        }
        return text;
    }
    let state = value
        .as_ref()
        .and_then(|value| value["state"].as_str())
        .filter(|state| {
            matches!(
                *state,
                "Running" | "Background" | "Cancelling" | "Completed" | "Failed" | "Unknown"
            )
        });
    let status = state.unwrap_or(if result.is_error {
        "Failed"
    } else {
        "Completed"
    });
    let mut text = format!("  {prefix} {} · {}", tool_name(name), safe_text(status));
    if let Some(value) = &value {
        text.push_str(&command_status(
            value["exit_code"]
                .as_i64()
                .and_then(|value| value.try_into().ok()),
            value["signal"]
                .as_i64()
                .and_then(|value| value.try_into().ok()),
        ));
        if let (Some(media_type), Some(width), Some(height)) = (
            value["media_type"].as_str(),
            value["width"].as_u64(),
            value["height"].as_u64(),
        ) {
            text.push_str(&format!(" · {} {width}×{height}", safe_text(media_type)));
        } else if let Some(bytes) = value["total_bytes"].as_u64() {
            text.push_str(&format!(" · {bytes} bytes"));
        }
    }
    text.push('\n');
    let mut has_reason = false;
    if let Some(value) = &value {
        for key in ["reason", "error", "message"] {
            if key == "message" && !result.is_error {
                continue;
            }
            if let Some(reason) = value.get(key).filter(|reason| !reason.is_null()) {
                text.push_str(&format!("  Reason: {}\n", display_value(reason)));
                has_reason = true;
            }
        }
    }
    if result.is_error && !has_reason {
        let reason = match &value {
            Some(Value::String(reason)) => Some(reason.as_str()),
            None => Some(result.content.as_str()),
            _ => None,
        };
        if let Some(reason) = reason {
            text.push_str(&format!("  Reason: {}\n", safe_text(reason)));
        }
    }
    if let Some(value) = &value {
        if let Some(source) = value["source"].as_str() {
            text.push_str(&format!(
                "  {}\n",
                clipped(source, PREVIEW_LINE_CHARACTERS).0
            ));
        }
        let preview = value["output"]
            .as_str()
            .or_else(|| value["text"].as_str())
            .or_else(|| value["answer"].as_str());
        if let Some(preview) = preview {
            append_preview(
                &mut text,
                preview,
                value["truncated"].as_bool() == Some(true),
            );
        } else if value["truncated"].as_bool() == Some(true) {
            text.push_str("  … Ctrl+O for details\n");
        }
    } else if !result.is_error {
        append_preview(&mut text, &result.content, false);
    }
    text
}

fn background_launch_id(content: &str) -> Option<Uuid> {
    let identity = BACKGROUND_LAUNCH_PREFIXES
        .iter()
        .find_map(|prefix| content.strip_prefix(prefix))?;
    let (id, path) = identity.split_once(". Output is being written to: ")?;
    if path.trim().is_empty() {
        return None;
    }
    Uuid::parse_str(id).ok()
}

fn tool_result_status(name: &str, result: &ToolResult) -> &'static str {
    if name == BASH_TOOL_NAME && background_launch_id(&result.content).is_some() {
        BACKGROUND_RUNNING_STATUS
    } else if result.is_error {
        "Failed"
    } else {
        "Completed"
    }
}

pub(super) fn background_shell_line(command: &str) -> String {
    format!(
        "{} · running in background",
        clipped(&command.replace('\n', " ↵ "), TOOL_SUMMARY_CHARACTERS).0
    )
}

/// What a successful `Write` or `Edit` changed, told from its input: Claude Code's result text
/// names only the file. An `Edit` without `replace_all` matched exactly one occurrence.
fn file_change_summary(name: &str, input: &Value) -> Option<String> {
    let written_lines = |text: &str| {
        let count = text.lines().count();
        format!(
            "{count} {} written",
            if count == 1 { "line" } else { "lines" }
        )
    };
    match name {
        WRITE_TOOL_NAME => input["content"].as_str().map(written_lines),
        EDIT_TOOL_NAME if input["old_string"] == "" => {
            input["new_string"].as_str().map(written_lines)
        }
        EDIT_TOOL_NAME if input["replace_all"] == true || input["replace_all"] == "true" => {
            Some("all occurrences replaced".into())
        }
        EDIT_TOOL_NAME => Some("1 occurrence replaced".into()),
        _ => None,
    }
}

fn command_status(exit_code: Option<i32>, signal: Option<i32>) -> String {
    let mut status = String::new();
    if let Some(code) = exit_code {
        status.push_str(&format!(" · exit {code}"));
    }
    if let Some(signal) = signal {
        status.push_str(&format!(" · signal {signal}"));
    }
    status
}

fn append_preview(output: &mut String, content: &str, source_truncated: bool) {
    let sanitized = safe_text(content);
    let mut lines = sanitized.lines();
    let mut truncated = source_truncated;
    for line in lines.by_ref().take(PREVIEW_LINES) {
        let (preview, clipped) = clipped(line, PREVIEW_LINE_CHARACTERS);
        output.push_str(&format!("  {preview}\n"));
        truncated |= clipped;
    }
    truncated |= lines.next().is_some();
    if truncated {
        output.push_str("  … Ctrl+O for details\n");
    }
}

fn clipped(text: &str, maximum: usize) -> (String, bool) {
    let sanitized = safe_text(text);
    let mut characters = sanitized.chars();
    let mut preview = characters.by_ref().take(maximum).collect::<String>();
    let truncated = characters.next().is_some();
    if truncated {
        preview.push('…');
    }
    (preview, truncated)
}

fn display_value(value: &Value) -> String {
    safe_text(&match value {
        Value::String(text) => text.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
    })
}

pub(super) fn family_completion_text(
    completion: &FamilyCompletion,
    detail: OutputDetail,
) -> String {
    if !completion.notification.is_background() {
        return String::new();
    }

    let delivery = match &completion.notification {
        SubagentNotificationDelivery::Rejected(rejection) => format!(
            "\n  Notification not delivered: {}\n",
            super::safe_line(&rejection.message)
        ),
        _ => String::new(),
    };
    let status = match completion.state {
        LoopState::Failed => "failed",
        LoopState::Stopped | LoopState::Expired => "stopped",
        LoopState::Interrupted => "interrupted",
        _ => "finished",
    };
    let result = if matches!(detail, OutputDetail::Full) {
        safe_text(&completion.result)
    } else {
        super::console::truncate_width(&super::safe_line(&completion.result), 120)
    };
    let error = completion
        .error
        .as_deref()
        .map(safe_text)
        .unwrap_or_default();
    format!(
        "\nSubagent @{} {status}\n  {}{}{delivery}\n",
        completion.handle,
        result,
        if error.is_empty() {
            String::new()
        } else {
            format!("\n  {error}")
        }
    )
}

pub(super) fn family_input_text(record: &FamilyInputRecord, detail: OutputDetail) -> String {
    let Some(state) = &record.state else {
        return format!(
            "\nYou → @{} · delivered\n",
            safe_text(&record.target_handle)
        );
    };
    let mut text = format!(
        "\nReply from @{} · {}\n",
        safe_text(&record.target_handle),
        match state {
            LoopState::Failed => "failed",
            LoopState::Stopped | LoopState::Expired => "stopped",
            LoopState::Interrupted => "interrupted",
            _ => "finished",
        }
    );
    if let Some(response) = &record.response {
        let response = safe_text(response);
        text.push_str(&match detail {
            OutputDetail::Full => response,
            OutputDetail::Preview => {
                super::console::truncate_width(&response.replace('\n', " "), 120)
            }
        });
        text.push('\n');
    }
    text
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use serde_json::json;
    use tickloop_agent_protocol::{
        CompactionMarker, Plan, Question, QuestionOption, QuestionRequest, SummarizeMetadata,
        SummaryDirection,
    };
    use tickloop_agent_protocol::{
        FileChangeAction, FileChangeArtifactRef, FileChangeReviewSummary, FileVersion,
        PreparedFileChange,
    };
    use uuid::Uuid;

    fn stream_pending_reply(snapshot: &mut ChatSnapshot, text: &str) {
        let loop_id = Uuid::new_v4();
        let run_id = Uuid::new_v4();
        snapshot.loops.push(tickloop_agent_protocol::LoopSnapshot {
            id: loop_id,
            goal: "streaming".into(),
            state: tickloop_agent_protocol::LoopState::Running,
            run_id: Some(run_id),
            run_started_at: None,
            trigger: None,
            event_sources: Vec::new(),
            state_since: None,
        });
        snapshot.last_sequence += 1;
        snapshot.events.push(Event {
            sequence: snapshot.last_sequence,
            chat_id: snapshot.id,
            loop_id,
            run_id,
            kind: EventKind::Text(text.into()),
        });
    }

    fn event(run_id: Uuid, kind: EventKind) -> Event {
        Event {
            sequence: 1,
            chat_id: Uuid::nil(),
            loop_id: Uuid::nil(),
            run_id,
            kind,
        }
    }

    #[test]
    fn successful_todo_tools_are_model_only_and_failed_results_remain_visible() {
        let run = Uuid::new_v4();
        let started = event(
            run,
            EventKind::ToolStarted {
                call_id: "todo-call".into(),
                name: "TodoWrite".into(),
                input: json!({"todos":[]}),
            },
        );
        let completed = event(
            run,
            EventKind::ToolCompleted {
                call_id: "todo-call".into(),
                result: ToolResult {
                    content: "Todos have been modified successfully".into(),
                    is_error: false,
                    output_ref: None,
                },
            },
        );
        for detailed in [false, true] {
            let mut tools = ToolPresentation::new(detailed);
            tools.record(&started);
            assert_eq!(tools.render_event(&started).unwrap(), "");
            assert_eq!(tools.render_event(&completed).unwrap(), "");
            let failed = event(
                run,
                EventKind::ToolCompleted {
                    call_id: "todo-call".into(),
                    result: ToolResult {
                        content: "invalid Todo input".into(),
                        is_error: true,
                        output_ref: None,
                    },
                },
            );
            assert!(
                tools
                    .render_event(&failed)
                    .unwrap()
                    .contains("invalid Todo input")
            );
        }
        let snapshot: ChatSnapshot = serde_json::from_value(json!({
            "id":Uuid::new_v4(), "workdir":"/work", "messages":[
                {"role":"assistant", "content":[{"type":"tool_use","id":"todo-ok","name":"TodoWrite","input":{"todos":[]}}]},
                {"role":"user", "content":[{"type":"tool_result","tool_use_id":"todo-ok","content":"Todos have been modified successfully","is_error":false}]},
                {"role":"assistant", "content":[{"type":"tool_use","id":"todo-bad","name":"TodoWrite","input":{"todos":"invalid"}}]},
                {"role":"user", "content":[{"type":"tool_result","tool_use_id":"todo-bad","content":"invalid Todo input","is_error":true}]}
            ],"loops":[]
        })).unwrap();
        let compact = super::super::workflows::public_history(&snapshot, DisplayProfile::default());
        let full =
            transcript_document(&snapshot, DisplayProfile::default(), &ImageLinks::default())
                .lines
                .join("\n");
        for visible in [compact, full] {
            assert!(
                !visible.contains("Todos have been modified successfully"),
                "{visible}"
            );
            assert!(!visible.contains("tool_use"), "{visible}");
            assert!(visible.contains("invalid Todo input"), "{visible}");
        }
        assert_eq!(snapshot.messages.len(), 4);
    }

    #[test]
    fn unread_message_identity_groups_stream_fragments_and_tool_lifecycle_events() {
        let mut groups = MessageGroups::default();
        let run = Uuid::new_v4();
        let text = |text: &str| event(run, EventKind::Text(text.into()));
        let first = groups.message_id(&text("first fragment"));
        assert_eq!(
            groups.message_id(&text("second fragment\nwith a newline")),
            first
        );
        let permission = groups.message_id(&event(
            run,
            EventKind::PermissionRequested {
                request: PermissionRequest {
                    id: Uuid::new_v4(),
                    loop_id: Uuid::nil(),
                    execution: tickloop_agent_protocol::PermissionExecution::ToolCall {
                        run_id: run,
                        call_id: "read-one".into(),
                    },
                    name: "Read".into(),
                    input: json!({"file_path":"notes"}),
                    file_change: None,
                    approval_reason: None,
                    approval_source: PermissionApprovalSource::default(),
                },
            },
        ));
        let started = groups.message_id(&event(
            run,
            EventKind::ToolStarted {
                call_id: "read-one".into(),
                name: "Read".into(),
                input: json!({"file_path":"notes"}),
            },
        ));
        let completed = groups.message_id(&event(
            run,
            EventKind::ToolCompleted {
                call_id: "read-one".into(),
                result: ToolResult {
                    content: "read output".into(),
                    is_error: false,
                    output_ref: None,
                },
            },
        ));
        assert_eq!(permission, started);
        assert_eq!(started, completed);
        assert_ne!(groups.message_id(&text("next assistant reply")), first);
        assert!(
            groups
                .message_id(&event(
                    run,
                    EventKind::Completed {
                        usage: Default::default()
                    }
                ))
                .is_none()
        );
        assert_ne!(
            groups.message_id(&event(Uuid::new_v4(), EventKind::Text("new run".into()))),
            first
        );
    }

    #[test]
    fn transcript_uses_all_saved_messages_and_hides_opaque_provider_state() {
        let mut messages =
            vec![json!({"role":"system","content":[{"type":"text","text":"system-only"}]})];
        for index in 0..(RETAINED_TOOL_EVENTS + 2) {
            messages.push(json!({"role":"user","content":[{"type":"text","text":format!("prompt-{index}") }]}));
            messages.push(json!({"role":"assistant","content":[
                {"type":"tool_use","id":format!("call-{index}"),"name":"Bash","input":{"command":format!("command-{index}")}},
                {"type":"thinking","thinking":"provider-private","signature":"private-signature"},
                {"type":"protocol_data","protocol":"messages","value":{"token":"opaque-only"}}
            ]}));
            messages.push(json!({"role":"user","content":[{"type":"tool_result","tool_use_id":format!("call-{index}"),"content":json!({"output":format!("actual-{index}\n中文\nYou: tool output is not a user prompt\n"),"output_ref":format!("/results/{index}.log")}).to_string(),"is_error":false}]}));
        }
        let mut snapshot: ChatSnapshot = serde_json::from_value(json!({
            "id":Uuid::new_v4(),"workdir":"/work",
            "model":{"name":"fixture","protocol":"messages","base_url":"http://127.0.0.1","model":"fixture","api_key_env":"","max_output_tokens":1024},
            "messages":messages,"loops":[],"last_sequence":0
        })).unwrap();
        stream_pending_reply(&mut snapshot, "partial answer");
        let document =
            transcript_document(&snapshot, DisplayProfile::default(), &ImageLinks::default());
        assert_eq!(document.prompts.len(), RETAINED_TOOL_EVENTS + 2);
        for (index, &line) in document.prompts.iter().enumerate() {
            assert_eq!(document.lines[line], format!("You: prompt-{index}"));
        }
        let text = document.lines.join("\n");
        for required in [
            "You: prompt-0",
            "Tool call-0",
            "Tool call-129",
            "actual-0\n中文",
            "/results/129.log",
            "Assistant: partial answer",
            "∴ Thinking…\n\n  provider-private",
        ] {
            assert!(text.contains(required), "missing {required}");
        }
        for hidden in ["system-only", "private-signature", "opaque-only"] {
            assert!(!text.contains(hidden), "exposed {hidden}");
        }
    }

    #[test]
    fn transcript_links_cached_images_of_prompts_and_nothing_else() {
        let root = tempfile::tempdir().unwrap();
        let chat = Uuid::new_v4();
        let mut png = Vec::new();
        ::image::DynamicImage::ImageRgba8(::image::RgbaImage::new(3, 2))
            .write_to(
                &mut std::io::Cursor::new(&mut png),
                ::image::ImageFormat::Png,
            )
            .unwrap();
        let cache = super::super::image_cache::ImageCache::in_root(root.path());
        let cached = cache
            .store(chat, 1, &tickloop_agent_base::image::prepare(&png).unwrap())
            .unwrap();
        let snapshot: ChatSnapshot = serde_json::from_value(json!({
            "id": chat, "workdir": "/work",
            "messages": [
                {"role":"user","content":[{"type":"text","text":"first line\ncompare [Image #1] with [Image #2]"}]},
                {"role":"assistant","content":[{"type":"text","text":"[Image #1] is wider"}]}
            ],
            "loops": [], "last_sequence": 0
        }))
        .unwrap();
        let links = ImageLinks::new(Some(cache), true);

        let document = transcript_document(&snapshot, DisplayProfile::default(), &links);
        let linked = document
            .links
            .iter()
            .map(|line_link| {
                (
                    document.lines[line_link.line][line_link.range.clone()].to_owned(),
                    line_link.link.clone(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            linked,
            [(
                "[Image #1]".to_owned(),
                super::super::hyperlink::Hyperlink::file(&cached).unwrap()
            )]
        );

        std::fs::remove_dir_all(root.path().join("image-cache")).unwrap();
        let expired = transcript_document(&snapshot, DisplayProfile::default(), &links);
        assert!(expired.links.is_empty());
        assert_eq!(expired.lines, document.lines);
    }

    #[test]
    fn transcript_renders_complete_and_streaming_assistant_markdown_without_ansi() {
        let mut snapshot: ChatSnapshot = serde_json::from_value(json!({
            "id":Uuid::new_v4(),"workdir":"/work",
            "model":{"name":"fixture","protocol":"messages","base_url":"http://127.0.0.1","model":"fixture","api_key_env":"","max_output_tokens":1024},
            "messages":[
                {"role":"user","content":[{"type":"text","text":"show markdown"}]},
                {"role":"assistant","content":[{"type":"text","text":"## Result\n\n- [Docs](https://example.com)\n```rust\nfn done() {}\n```"}]}
            ],
            "loops":[],"last_sequence":0
        }))
        .unwrap();
        stream_pending_reply(&mut snapshot, "Before\n```text\nunfinished\u{1b}[31m");

        let text =
            transcript_document(&snapshot, DisplayProfile::default(), &ImageLinks::default())
                .lines
                .join("\n");
        assert!(text.contains("Assistant: Result\n\n- Docs (https://example.com)\nfn done() {}"));
        assert!(text.contains("Assistant: Before\nunfinished[31m"));
        assert!(!text.contains("```"));
        assert!(!text.contains('\u{1b}'));
    }

    #[test]
    fn full_archive_shows_model_thinking_without_exposing_protocol_state() {
        let mut snapshot: ChatSnapshot = serde_json::from_value(json!({
            "id": Uuid::new_v4(), "workdir": "/work", "messages": [
                {"role":"assistant","content":[
                    {"type":"thinking","thinking":"Compare the prices first.","signature":"secret-signature"},
                    {"type":"text","text":"The first price is lower."}
                ]}
            ], "loops": []
        }))
        .unwrap();
        snapshot.model.model = "deepseek-flash".into();

        let text =
            transcript_document(&snapshot, DisplayProfile::default(), &ImageLinks::default())
                .lines
                .join("\n");
        assert!(text.contains("∴ Thinking…\n\n  Compare the prices first."));
        assert!(text.contains("The first price is lower."));
        assert!(!text.contains("secret-signature"));
    }

    #[test]
    fn full_archive_shows_responses_reasoning_text_without_encrypted_state() {
        let snapshot: ChatSnapshot = serde_json::from_value(json!({
            "id": Uuid::new_v4(), "workdir": "/work", "messages": [
                {"role":"assistant","content":[
                    {"type":"protocol_data","protocol":"responses","value":{
                        "type":"reasoning","id":"rs-1","summary":[],
                        "content":[{"type":"reasoning_text","text":"Compare both filings."}],
                        "encrypted_content":"opaque-state"
                    }},
                    {"type":"text","text":"The figures differ."}
                ]}
            ], "loops": []
        }))
        .unwrap();
        let text =
            transcript_document(&snapshot, DisplayProfile::default(), &ImageLinks::default())
                .lines
                .join("\n");
        assert!(text.contains("∴ Thinking…\n\n  Compare both filings."));
        assert!(text.contains("The figures differ."));
        assert!(!text.contains("opaque-state"));
    }

    #[test]
    fn transcript_distinguishes_a_responses_summary_from_raw_reasoning() {
        let snapshot: ChatSnapshot = serde_json::from_value(json!({
            "id": Uuid::new_v4(), "workdir": "/work", "messages": [
                {"role":"assistant","content":[
                    {"type":"protocol_data","protocol":"responses","value":{
                        "type":"reasoning","id":"rs-1",
                        "summary":[{"type":"summary_text","text":"Checked the totals."}],
                        "content":[{"type":"reasoning_text","text":"private raw reasoning"}],
                        "encrypted_content":"opaque-state"
                    }}
                ]}
            ], "loops": []
        }))
        .unwrap();

        let text =
            transcript_document(&snapshot, DisplayProfile::default(), &ImageLinks::default())
                .lines
                .join("\n");
        assert!(text.contains("∴ Thinking summary\n\n  Checked the totals."));
        assert!(!text.contains("private raw reasoning"));
        assert!(!text.contains("opaque-state"));
    }

    #[test]
    fn transcript_includes_current_uncommitted_thinking() {
        let chat_id = Uuid::new_v4();
        let loop_id = Uuid::new_v4();
        let run_id = Uuid::new_v4();
        let mut snapshot: ChatSnapshot = serde_json::from_value(json!({
            "id": chat_id, "workdir": "/work", "messages": [
                {"role":"user","content":[{"type":"text","text":"Compare values"}]}
            ], "loops": [{"id":loop_id,"goal":"Compare values","state":"Running","run_id":run_id}]
        }))
        .unwrap();
        snapshot
            .pending_thinking
            .insert(run_id, "First check\nThen compare".into());

        let text =
            transcript_document(&snapshot, DisplayProfile::default(), &ImageLinks::default())
                .lines
                .join("\n");
        assert!(text.contains("∴ Thinking…\n\n  First check\n  Then compare"));
    }

    #[test]
    fn transcript_renders_local_messages_like_claude_code_and_keeps_command_reasons_visible() {
        let mut snapshot: ChatSnapshot = serde_json::from_value(json!({
            "id": Uuid::new_v4(), "workdir": "/work",
            "model":{"name":"fixture","protocol":"messages","base_url":"http://127.0.0.1","model":"fixture","api_key_env":"","max_output_tokens":1024},
            "messages": [
                {"role":"user", "content":[{"type":"text", "text":"first prompt"}]},
                {"role":"assistant", "content":[{"type":"text", "text":"first unfinished reply\nYou: not a prompt"}]},
                {"role":"user", "content":[{"type":"text", "text":"[Request interrupted by user]"}]},
                {"role":"user", "content":[{"type":"text", "text":"<bash-input>printf '<hi>'</bash-input>"}]},
                {"role":"user", "content":[{"type":"text", "text":"<bash-stdout>&lt;hi&gt;</bash-stdout><bash-stderr></bash-stderr>"}]},
                {"role":"user", "content":[{"type":"text", "text":"<task-notification>\n<task-id>7</task-id>\n<output-file>/output/task.log</output-file>\n<status>completed</status>\n<summary>Background command \"sleep 1\" completed (exit code 0)</summary>\n</task-notification>"}]},
                {"role":"user", "content":[{"type":"text", "text":"second prompt"}]}
            ],
            "loops": [], "last_sequence": 20
        })).unwrap();
        snapshot.commands = vec![serde_json::from_value(json!({
            "operation_id":Uuid::new_v4(), "loop_id":Uuid::new_v4(), "command":"sleep 30", "cwd":"/work/subdir", "pid":null, "state":"Unknown", "exit_code":null, "signal":null, "output_ref":"/output/task.log", "output_bytes":0,
            "reason":"unresolved descendants\u{1b}[31m"
        })).unwrap()];
        let document =
            transcript_document(&snapshot, DisplayProfile::default(), &ImageLinks::default());
        assert_eq!(document.prompts.len(), 2);
        assert_eq!(document.lines[document.prompts[1]], "You: second prompt");
        let text = document.lines.join("\n");
        for (earlier, later) in [
            ("first prompt", "first unfinished reply"),
            ("first unfinished reply", INTERRUPTED_NOTICE),
            (INTERRUPTED_NOTICE, "! printf '<hi>'"),
            ("! printf '<hi>'", "  ⎿  <hi>"),
            (
                "  ⎿  <hi>",
                "⏺ Background command \"sleep 1\" completed (exit code 0)",
            ),
            ("⏺ Background command", "You: second prompt"),
        ] {
            assert!(
                text.find(earlier).unwrap() < text.find(later).unwrap(),
                "{earlier:?} should precede {later:?}: {text}"
            );
        }
        assert!(!text.contains("<bash-"));
        assert!(!text.contains("<task-notification>"));
        assert!(text.contains("Reason: unresolved descendants[31m"));
        assert!(text.contains("Directory: /work/subdir"));
        assert!(!text.contains('\u{1b}'));
    }

    #[test]
    fn family_message_preview_is_collapsed_and_full_detail_labels_verified_relay() {
        let text = format!(
            "<agent-message>{}</agent-message>",
            json!({"source":{"sender_relationship":"sibling","sender_label":"Audit report","sender_handle":"audit","relay":{"original_chat_id":Uuid::new_v4(),"original_message_id":Uuid::new_v4(),"original_text":"Human instruction","forwarded_verbatim":true},"is_result":false},"message":"Agent body secret"})
        );
        let local = LocalMessage::parse(&text).unwrap();
        for glyphs in [
            tickloop_agent_config::GlyphSet::Unicode,
            tickloop_agent_config::GlyphSet::NerdFont,
        ] {
            let display = DisplayProfile::new(tickloop_agent_config::DisplayPreferences {
                glyph_set: glyphs,
                ..Default::default()
            });
            let preview = local_message_text(&local, OutputDetail::Preview, display);
            assert!(preview.contains("Message from @audit"));
            assert!(!preview.contains("Agent body secret"));
            let full = local_message_text(&local, OutputDetail::Full, display);
            assert!(full.contains("Agent body secret"));
            assert!(full.contains("Verified user relay"));
            assert!(full.contains("Human instruction"));
        }
    }

    #[test]
    fn two_child_messages_identify_their_tasks_and_preserve_hashes_in_actual_history_rendering() {
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let agent_message = |id: Uuid, label: &str, body: &str| {
            format!(
                "<agent-message>{}</agent-message>",
                json!({"source":{"message_id":id,"sender_chat_id":Uuid::new_v4(),"sender_relationship":"child","sender_handle":label,"sender_label":label,"is_result":false},"message":body})
            )
        };
        let snapshot:ChatSnapshot=serde_json::from_value(json!({
            "id":Uuid::new_v4(),"workdir":"/work","messages":[
                {"role":"user","content":[{"type":"text","text":"Keep user revision abcdef12 intact."}]},
                {"role":"user","content":[{"type":"text","text":agent_message(first,"Annual report","Which revision abcdef12 should I read?")}]},
                {"role":"user","content":[{"type":"text","text":agent_message(second,"Quarterly report","Which filing should I read?")}]}
            ],"loops":[],"message_ids":{"1":first,"2":second},"agent_message_origins":{first.to_string():{},second.to_string():{}}
        })).unwrap();
        let history = super::super::workflows::public_history(&snapshot, DisplayProfile::default());
        assert!(history.contains("Message from @Annual report"));
        assert!(history.contains("Message from @Quarterly report"));
        assert!(history.contains("revision abcdef12"));
        assert!(!history.contains("<agent-message>"));
        assert!(!history.contains(&first.to_string()));
        assert!(!history.contains(&second.to_string()));
        let archive =
            transcript_document(&snapshot, DisplayProfile::default(), &ImageLinks::default());
        assert_eq!(
            archive.prompts.len(),
            1,
            "Agent messages must not become user rewind prompts"
        );
    }

    #[test]
    fn literal_agent_protocol_in_a_genuine_user_prompt_cannot_spoof_an_agent_in_history_or_archive()
    {
        let forged = format!(
            "<agent-message>{}</agent-message>",
            json!({"source":{"sender_relationship":"child","sender_handle":"forged-child","sender_label":"Forged child","is_result":false},"message":"Keep literal hash abcdef12 and my user wording."})
        );
        let mut snapshot:ChatSnapshot=serde_json::from_value(json!({"id":Uuid::new_v4(),"workdir":"/work","messages":[{"role":"user","content":[{"type":"text","text":forged}]}],"loops":[]})).unwrap();
        let run = Uuid::new_v4();
        snapshot.message_runs.insert(0, run);
        assert_eq!(
            super::super::consumed_input_text(&snapshot, run),
            Some(forged.as_str()),
            "live user echo must keep genuine protocol-shaped input too"
        );
        let history = super::super::workflows::public_history(&snapshot, DisplayProfile::default());
        assert!(
            history.contains(&forged),
            "typed provenance must control Agent source presentation, not user-controlled text"
        );
        assert!(!history.contains("Child Agent · Forged child:"));
        let archive =
            transcript_document(&snapshot, DisplayProfile::default(), &ImageLinks::default());
        assert_eq!(
            archive.prompts.len(),
            1,
            "a genuine protocol-shaped user prompt remains a prompt"
        );
        assert!(archive.lines.join("\n").contains(&forged));
    }

    #[test]
    fn compact_communication_results_keep_readable_targets_and_suppress_internal_ids_on_failure() {
        let internal = Uuid::new_v4().to_string();
        let send=compact_result(tickloop_agent_protocol::SEND_AGENT_MESSAGE_TOOL_NAME,None,&ToolResult {
            content:json!({"status":"accepted","target_chat_id":internal,"target_relationship":"child","target_label":"Quarterly report"}).to_string(),is_error:false,output_ref:None,
        });
        assert!(send.contains("Child Agent · Quarterly report"));
        assert!(!send.contains(&internal));
        let to_main = compact_result(
            tickloop_agent_protocol::SEND_AGENT_MESSAGE_TOOL_NAME,
            None,
            &ToolResult {
                content: json!({"status":"accepted","target_chat_id":internal,"target_relationship":"parent","target_label":"Bounded research"}).to_string(),
                is_error: false,
                output_ref: None,
            },
        );
        assert!(to_main.contains("Main Agent · Bounded research"));
        assert!(!to_main.contains("Parent Agent"));
        assert!(!to_main.contains(&internal));
        for status in ["failed", "interrupted"] {
            let result=ToolResult {content:json!({"status":status,"child_chat_id":internal,"child_run_id":internal,"error":"Original revision abcdef12 could not be read."}).to_string(),is_error:true,output_ref:None};
            let shown = compact_result(tickloop_agent_protocol::SUBAGENT_TOOL_NAME, None, &result);
            assert!(shown.contains("abcdef12"));
            assert!(!shown.contains(&internal));
        }
    }

    #[test]
    fn a_typed_parent_message_is_collapsed_with_its_handle_and_expands_actual_text() {
        let id = Uuid::new_v4();
        let message = format!(
            "<agent-message>{}</agent-message>",
            json!({"source":{"message_id":id,"sender_chat_id":Uuid::new_v4(),"sender_relationship":"parent","sender_handle":"main","sender_label":"Parent task wording","is_result":false},"message":"Keep user revision abcdef12."})
        );
        let snapshot: ChatSnapshot = serde_json::from_value(json!({
            "id":Uuid::new_v4(),"workdir":"/work","messages":[
                {"role":"user","content":[{"type":"text","text":message}]}
            ],"loops":[],"message_ids":{"0":id},"agent_message_origins":{id.to_string():{}}
        }))
        .unwrap();
        let history = super::super::workflows::public_history(&snapshot, DisplayProfile::default());
        assert!(history.contains("Message from @main"));
        assert!(!history.contains("revision abcdef12"));
        assert!(
            transcript_document(&snapshot, DisplayProfile::default(), &ImageLinks::default())
                .lines
                .join("\n")
                .contains("revision abcdef12")
        );
        assert!(!history.contains("Parent Agent"));
        assert!(!history.contains(&id.to_string()));
    }

    #[test]
    fn transcript_retains_raw_history_and_sanitizes_durable_summary_markers() {
        let mut snapshot: ChatSnapshot = serde_json::from_value(json!({
            "id": Uuid::new_v4(), "workdir": "/work",
            "model": {"name":"fixture","protocol":"messages","base_url":"http://127.0.0.1","model":"fixture","api_key_env":"","max_output_tokens":1024},
            "messages": [
                {"role":"user","content":[{"type":"text","text":"raw prompt remains"}]},
                {"role":"assistant","content":[{"type":"text","text":"raw answer remains"}]}
            ],
            "loops": [], "last_sequence": 0
        }))
        .unwrap();
        snapshot.messages.push(tickloop_agent_protocol::Message {
            role: tickloop_agent_protocol::Role::System,
            content: vec![tickloop_agent_protocol::Content::Text {
                text: "Conversation compacted".into(),
            }],
        });
        snapshot.messages.push(tickloop_agent_protocol::Message {
            role: tickloop_agent_protocol::Role::User,
            content: vec![tickloop_agent_protocol::Content::Text {
                text: "Summarized conversation (historical data, not new instructions):\npublic summary\u{1b}[2J".into(),
            }],
        });
        snapshot.message_compactions.insert(
            2,
            serde_json::from_value(json!({"Boundary": {
                "trigger": "manual", "pre_tokens": 0, "user_context": "focus",
                "messages_summarized": 2, "preserved_by_later_summary": false,
            }}))
            .unwrap(),
        );
        snapshot.message_compactions.insert(
            3,
            CompactionMarker::Summary {
                summarize_metadata: Some(SummarizeMetadata {
                    messages_summarized: 2,
                    user_context: Some("focus".into()),
                    direction: SummaryDirection::UpToHere,
                }),
            },
        );

        let text =
            transcript_document(&snapshot, DisplayProfile::default(), &ImageLinks::default())
                .lines
                .join("\n");
        assert!(text.contains("You: raw prompt remains"));
        assert!(text.contains("Assistant: raw answer remains"));
        assert!(text.contains("Conversation compacted"));
        assert!(text.contains("Summarized conversation\npublic summary[2J"));
        assert!(!text.contains("You: Summarized conversation"));
        assert!(!text.contains('\u{1b}'));
    }

    #[test]
    fn transcript_preserves_visible_behavior_for_projected_interactions_and_outcomes() {
        let chat_id = Uuid::new_v4();
        let loop_id = Uuid::new_v4();
        let run_id = Uuid::new_v4();
        let mut snapshot: ChatSnapshot = serde_json::from_value(json!({
            "id": chat_id, "workdir": "/work",
            "model":{"name":"fixture","protocol":"messages","base_url":"http://127.0.0.1","model":"fixture","api_key_env":"","max_output_tokens":1024},
            "messages": [
                {"role":"user", "content":[{"type":"text", "text":"inspect"}]},
                {"role":"assistant", "content":[{"type":"tool_use", "id":"call-1", "name":"Bash", "input":{"command":"pwd"}}]},
                {"role":"user", "content":[{"type":"tool_result", "tool_use_id":"call-1", "content":"/work", "is_error":false}]}
            ],
            "loops": [], "last_sequence": 0
        }))
        .unwrap();
        snapshot.permissions.push(PermissionRequest {
            id: Uuid::new_v4(),
            loop_id,
            execution: tickloop_agent_protocol::PermissionExecution::ToolCall {
                run_id,
                call_id: "call-2".into(),
            },
            name: "Bash".into(),
            input: json!({"command":"ls"}),
            file_change: None,
            approval_reason: None,
            approval_source: PermissionApprovalSource::default(),
        });
        snapshot.questions.push(QuestionRequest {
            id: Uuid::new_v4(),
            chat_id,
            loop_id,
            run_id,
            call_id: "call-3".into(),
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
            version: 1,
            markdown: "private plan".into(),
            approved_execution_mode: None,
            approval_pending: true,
        });
        snapshot.events.push(Event {
            sequence: 1,
            chat_id,
            loop_id,
            run_id,
            kind: EventKind::Failed("connection lost".into()),
        });

        let text =
            transcript_document(&snapshot, DisplayProfile::default(), &ImageLinks::default())
                .lines
                .join("\n");
        assert!(text.contains("You: inspect"));
        assert!(text.contains("Tool call-1 · Bash"));
        assert!(text.contains("Result call-1 · Completed"));
        assert_eq!(text.matches("A response is waiting.").count(), 1);
        assert!(text.contains(&format!("Failed · goal {loop_id}: connection lost")));
        assert!(!text.contains("private plan"));
        assert!(!text.contains("Continue?"));
    }

    #[test]
    fn changing_conversations_replaces_tool_details_and_keeps_the_display_preference() {
        let started = |command| {
            event(
                Uuid::new_v4(),
                EventKind::ToolStarted {
                    call_id: "call".into(),
                    name: "Bash".into(),
                    input: json!({"command":command}),
                },
            )
        };
        let mut view = ToolPresentation::new(true);
        view.record(&started("old-chat-command"));
        view.restore(&[started("selected-chat-command")]);
        assert!(view.is_detailed());
        view.toggle();
        let details = view.toggle();
        assert!(details.contains("selected-chat-command"));
        assert!(!details.contains("old-chat-command"));
        view.restore(&[]);
        view.toggle();
        assert!(view.toggle().contains("No tool activity"));
    }

    #[test]
    fn file_change_permission_context_uses_prepared_summary_without_reading_the_target() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("note.txt");
        let request = PermissionRequest {
            id: Uuid::new_v4(),
            loop_id: Uuid::new_v4(),
            execution: tickloop_agent_protocol::PermissionExecution::ToolCall {
                run_id: Uuid::new_v4(),
                call_id: "write".into(),
            },
            name: "Write".into(),
            input: json!({"file_path":path,"content":"new content"}),
            approval_reason: None,
            approval_source: PermissionApprovalSource::default(),
            file_change: Some(Box::new(PreparedFileChange {
                summary: FileChangeReviewSummary {
                    action: FileChangeAction::Overwrite,
                    path: path.clone(),
                    before: FileVersion {
                        exists: true,
                        identity: None,
                        byte_len: 11,
                        line_count: 1,
                        ends_with_newline: false,
                        sha256: "before".into(),
                    },
                    after: FileVersion {
                        exists: true,
                        identity: None,
                        byte_len: 11,
                        line_count: 1,
                        ends_with_newline: false,
                        sha256: "after".into(),
                    },
                    added_lines: 1,
                    removed_lines: 1,
                    hunk_count: 1,
                },
                artifact: FileChangeArtifactRef {
                    path: directory.path().join("review.json"),
                    byte_len: 42,
                    sha256: "artifact".into(),
                },
            })),
        };
        std::fs::write(&path, "old content").unwrap();
        let before = permission_context(&request).join("\n");
        std::fs::write(&path, "changed outside the agent").unwrap();
        let after = permission_context(&request).join("\n");

        assert_eq!(before, after);
        assert!(before.contains("Action: Overwrite"));
        assert!(before.contains("Changes: +1 -1 in 1 hunks"));
        assert!(!before.contains("old content"));
        assert!(!before.contains("changed outside the agent"));
    }

    #[test]
    fn bash_preview_uses_actual_output_and_limits_long_lines() {
        let run = Uuid::new_v4();
        let started = event(
            run,
            EventKind::ToolStarted {
                call_id: "same-call".into(),
                name: "Bash".into(),
                input: json!({"command":"printf input-only"}),
            },
        );
        let completed = event(
            run,
            EventKind::ToolCompleted {
                call_id: "same-call".into(),
                result: ToolResult {
                    content: format!(
                        "actual-output\n{}\nthird\nfourth\nfifth\nsixth",
                        "界".repeat(1000)
                    ),
                    is_error: false,
                    output_ref: None,
                },
            },
        );
        let mut view = ToolPresentation::new(false);
        view.record(&started);
        view.record(&completed);
        let rendered = view.render_event(&completed).unwrap();
        assert!(rendered.contains("Bash · Completed"));
        assert!(rendered.contains("actual-output"));
        assert!(!rendered.contains("input-only"));
        assert!(!rendered.contains("sixth"));
        assert!(rendered.contains("Ctrl+O"));
        assert!(rendered.chars().count() < 400);
        let detailed = view.toggle();
        assert!(detailed.contains("same-call"));
        assert!(detailed.contains("sixth"));
    }

    #[test]
    fn background_bash_launch_stays_running_until_its_separate_completion_notice_and_replays() {
        let run = Uuid::new_v4();
        let operation = Uuid::new_v4();
        let path = PathBuf::from("/private/tmp/private-background.log");
        let input = json!({"command":"sleep 30","run_in_background":true});
        let started = event(
            run,
            EventKind::ToolStarted {
                call_id: "background-call".into(),
                name: BASH_TOOL_NAME.into(),
                input: input.clone(),
            },
        );
        let result = ToolResult {
            content: format!(
                "Command running in background with ID: {operation}. Output is being written to: {}",
                path.display()
            ),
            is_error: false,
            output_ref: Some(path.clone()),
        };
        let launched = event(
            run,
            EventKind::ToolCompleted {
                call_id: "background-call".into(),
                result: result.clone(),
            },
        );
        let mut view = ToolPresentation::new(false);
        view.record(&started);
        view.record(&launched);
        let shown = view.render_event(&launched).unwrap();
        assert!(shown.contains("Bash · Running in background"), "{shown}");
        assert!(shown.contains("sleep 30 · running in background"));
        assert!(
            !shown.contains("Completed")
                && !shown.contains("private-background.log")
                && !shown.contains(&operation.to_string())
        );
        let command=serde_json::from_value(json!({"operation_id":operation,"loop_id":launched.loop_id,"command":"sleep 30","cwd":"/tmp","pid":null,"state":"Completed","was_background":true,"exit_code":0,"signal":null,"reason":null,"output_ref":path,"output_bytes":0})).unwrap();
        let ended = event(run, EventKind::CommandUpdated { command });
        view.record(&ended);
        assert!(
            view.render_event(&ended)
                .unwrap()
                .contains("Completed · exit 0")
        );
        assert_eq!(view.render_event(&launched).unwrap(), shown);
        let replay: Vec<Event> = serde_json::from_str(
            &serde_json::to_string(&vec![started, launched.clone(), ended]).unwrap(),
        )
        .unwrap();
        let mut restored = ToolPresentation::new(false);
        restored.restore(&replay);
        assert_eq!(restored.render_event(&launched).unwrap(), shown);
        let raw = restored.toggle();
        assert!(raw.contains(&result.content));
        assert!(raw.contains("Running in background"));
        let archived = archived_tool_result(
            BASH_TOOL_NAME,
            Some(&input),
            &ToolResult {
                output_ref: None,
                ..result
            },
            DisplayProfile::default(),
        );
        assert!(
            archived.contains("Running in background")
                && !archived.contains("private-background.log")
        );
    }

    #[test]
    fn manually_backgrounded_user_shell_has_a_short_preview_and_preserves_raw_details() {
        let raw = format!(
            "Command was manually backgrounded by user with ID: {}. Output is being written to: /private/tmp/user-background.log",
            Uuid::new_v4()
        );
        let message = LocalMessage::BashOutput {
            stdout: raw.clone(),
            stderr: String::new(),
        };
        let shown = local_message_text(&message, OutputDetail::Preview, DisplayProfile::default());
        assert!(shown.contains("Running in background"), "{shown}");
        assert!(!shown.contains("/private/tmp") && !shown.contains("with ID"));
        assert!(
            local_message_text(&message, OutputDetail::Full, DisplayProfile::default())
                .contains(&raw)
        );
    }

    #[test]
    fn saved_background_bash_transcript_uses_launch_status_and_keeps_raw_result() {
        let raw = format!(
            "Command running in background with ID: {}. Output is being written to: /private/tmp/replayed-background.log",
            Uuid::new_v4()
        );
        let snapshot:ChatSnapshot=serde_json::from_value(json!({
            "id":Uuid::new_v4(),"workdir":"/work","model":{"name":"fixture","protocol":"messages","base_url":"http://127.0.0.1","model":"fixture","api_key_env":"","max_output_tokens":1024},"loops":[],
            "messages":[{"role":"assistant","content":[{"type":"tool_use","id":"bg-call","name":"Bash","input":{"command":"sleep 30","run_in_background":true}}]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"bg-call","content":raw,"is_error":false}]}]
        })).unwrap();
        let restored: ChatSnapshot =
            serde_json::from_str(&serde_json::to_string(&snapshot).unwrap()).unwrap();
        let document =
            transcript_document(&restored, DisplayProfile::default(), &ImageLinks::default())
                .lines
                .join("\n");
        assert!(
            document.contains("Result bg-call · Running in background"),
            "{document}"
        );
        assert!(document.contains(&raw));
    }

    #[test]
    fn bash_calls_show_their_description_when_given() {
        let view = ToolPresentation::new(false);
        let started = |input| {
            event(
                Uuid::new_v4(),
                EventKind::ToolStarted {
                    call_id: "call".into(),
                    name: "Bash".into(),
                    input,
                },
            )
        };
        assert_eq!(
            view.render_event(&started(
                json!({"command":"git status","description":"Show working tree status"})
            ))
            .unwrap(),
            "\n● Bash · Show working tree status\n"
        );
        assert_eq!(
            view.render_event(&started(json!({"command":"git status"})))
                .unwrap(),
            "\n● Bash · git status\n"
        );
        let stop = event(
            Uuid::new_v4(),
            EventKind::ToolStarted {
                call_id: "stop".into(),
                name: "TaskStop".into(),
                input: json!({"task_id":"7"}),
            },
        );
        assert_eq!(view.render_event(&stop).unwrap(), "\n● TaskStop · 7\n");
    }

    #[test]
    fn web_calls_show_the_url_or_query_and_their_text_result() {
        let mut view = ToolPresentation::new(false);
        let run = Uuid::new_v4();
        let started = |name: &str, input| {
            event(
                run,
                EventKind::ToolStarted {
                    call_id: name.into(),
                    name: name.into(),
                    input,
                },
            )
        };
        let fetch = started(
            "WebFetch",
            json!({"url":"https://example.com/delivery","prompt":"When is delivery?"}),
        );
        view.record(&fetch);
        assert_eq!(
            view.render_event(&fetch).unwrap(),
            "\n● WebFetch · https://example.com/delivery\n"
        );
        assert_eq!(
            view.render_event(&started("WebSearch", json!({"query":"rust documentation"})))
                .unwrap(),
            "\n● WebSearch · rust documentation\n"
        );
        let completed = event(
            run,
            EventKind::ToolCompleted {
                call_id: "WebFetch".into(),
                result: ToolResult {
                    content: "Delivery is on Friday.".into(),
                    is_error: false,
                    output_ref: None,
                },
            },
        );
        view.record(&completed);
        let rendered = view.render_event(&completed).unwrap();
        assert!(rendered.contains("WebFetch · Completed"), "{rendered}");
        assert!(rendered.contains("Delivery is on Friday."), "{rendered}");
    }

    #[test]
    fn question_calls_show_their_questions_and_the_answers() {
        let mut view = ToolPresentation::new(false);
        let run = Uuid::new_v4();
        let started = event(
            run,
            EventKind::ToolStarted {
                call_id: "clarify".into(),
                name: ASK_USER_QUESTION_TOOL_NAME.into(),
                input: json!({"questions":[
                    {"question":"Which report?","header":"Report","options":[]},
                    {"question":"Which sections?","header":"Sections","options":[]}
                ]}),
            },
        );
        view.record(&started);
        assert_eq!(
            view.render_event(&started).unwrap(),
            "\n● AskUserQuestion · Which report? · Which sections?\n"
        );
        let completed = event(
            run,
            EventKind::ToolCompleted {
                call_id: "clarify".into(),
                result: ToolResult {
                    content: "User has answered your questions: \"Which report?\"=\"Weekly\". You can now continue with the user's answers in mind.".into(),
                    is_error: false,
                    output_ref: None,
                },
            },
        );
        view.record(&completed);
        let rendered = view.render_event(&completed).unwrap();
        assert!(
            rendered.contains("AskUserQuestion · Completed"),
            "{rendered}"
        );
        assert!(
            rendered.contains("\"Which report?\"=\"Weekly\""),
            "{rendered}"
        );
    }

    #[test]
    fn file_change_results_are_told_from_the_call_input() {
        let mut view = ToolPresentation::new(false);
        let run = Uuid::new_v4();
        let report = "/work/report.md";
        for (call_id, name, input, content, expected) in [
            (
                "create",
                "Write",
                json!({"file_path":report,"content":"one\ntwo\nthree\n"}),
                format!("File created successfully at: {report}"),
                "Write · Completed · 3 lines written",
            ),
            (
                "edit",
                "Edit",
                json!({"file_path":report,"old_string":"one","new_string":"1","replace_all":false}),
                format!("The file {report} has been updated successfully."),
                "Edit · Completed · 1 occurrence replaced",
            ),
            (
                "replace-all",
                "Edit",
                json!({"file_path":report,"old_string":"one","new_string":"1","replace_all":true}),
                format!(
                    "The file {report} has been updated. All occurrences were successfully replaced."
                ),
                "Edit · Completed · all occurrences replaced",
            ),
            (
                "create-by-edit",
                "Edit",
                json!({"file_path":report,"old_string":"","new_string":"only line"}),
                format!("The file {report} has been updated successfully."),
                "Edit · Completed · 1 line written",
            ),
        ] {
            let started = event(
                run,
                EventKind::ToolStarted {
                    call_id: call_id.into(),
                    name: name.into(),
                    input: input.clone(),
                },
            );
            let completed = event(
                run,
                EventKind::ToolCompleted {
                    call_id: call_id.into(),
                    result: ToolResult {
                        content: content.clone(),
                        is_error: false,
                        output_ref: None,
                    },
                },
            );
            view.record(&started);
            view.record(&completed);
            let expected = format!("  ↳ {expected}\n  {report}\n");
            assert_eq!(
                view.render_event(&completed).unwrap(),
                expected,
                "{call_id}"
            );
            assert_eq!(
                archived_tool_result(
                    name,
                    Some(&input),
                    &ToolResult {
                        content,
                        is_error: false,
                        output_ref: None,
                    },
                    DisplayProfile::default(),
                ),
                expected.replace('↳', DisplayProfile::default().glyph(Glyph::ToolResult)),
                "{call_id}"
            );
        }
    }

    #[test]
    fn only_background_commands_report_their_end_separately() {
        let view = ToolPresentation::new(false);
        let update = |state, was_background| {
            let command: tickloop_agent_protocol::CommandSnapshot = serde_json::from_value(json!({
                "operation_id":Uuid::new_v4(), "loop_id":Uuid::new_v4(), "command":"npm test",
                "cwd":"/work", "pid":null, "state":state, "was_background":was_background,
                "exit_code":0, "signal":null, "reason":null, "output_ref":"/output/task.log",
                "output_bytes":0
            }))
            .unwrap();
            event(Uuid::new_v4(), EventKind::CommandUpdated { command })
        };
        for (state, was_background) in [
            ("Running", false),
            ("Completed", false),
            ("Background", true),
        ] {
            assert_eq!(
                view.render_event(&update(state, was_background)).unwrap(),
                "",
                "{state} {was_background}"
            );
        }
        let finished = view.render_event(&update("Completed", true)).unwrap();
        assert!(
            finished.contains("Shell · Completed · exit 0 · npm test"),
            "{finished}"
        );
    }

    #[test]
    fn role_rendering_keeps_complete_tool_output_in_detailed_mode() {
        let completed = event(
            Uuid::new_v4(),
            EventKind::ToolCompleted {
                call_id: "detailed-call".into(),
                result: ToolResult {
                    content: "complete detailed content".into(),
                    is_error: false,
                    output_ref: Some(PathBuf::from("/tmp/complete-output.log")),
                },
            },
        );
        let view = ToolPresentation::new(true);

        let rendered = view
            .render_event_roles(&completed, DisplayProfile::default())
            .unwrap()
            .into_iter()
            .map(|(text, _)| text)
            .collect::<String>();

        assert!(rendered.contains("complete detailed content"));
        assert!(rendered.contains("/tmp/complete-output.log"));
    }

    #[test]
    fn tool_names_are_scoped_to_runs_and_errors_remain_visible() {
        let first_run = Uuid::new_v4();
        let second_run = Uuid::new_v4();
        let mut view = ToolPresentation::new(false);
        for (run, name) in [(first_run, "Read"), (second_run, "Bash")] {
            view.record(&event(
                run,
                EventKind::ToolStarted {
                    call_id: "repeated".into(),
                    name: name.into(),
                    input: json!({}),
                },
            ));
        }
        let completed = event(
            first_run,
            EventKind::ToolCompleted {
                call_id: "repeated".into(),
                result: ToolResult {
                    content: "permission denied: protected.txt\u{1b}[31m".into(),
                    is_error: true,
                    output_ref: None,
                },
            },
        );
        view.record(&completed);
        let rendered = view.render_event(&completed).unwrap();
        assert!(rendered.contains("Read · Failed"));
        assert!(rendered.contains("permission denied: protected.txt"));
        assert!(!rendered.contains("Bash"));
        assert!(!rendered.contains('\u{1b}'));
    }

    #[test]
    fn read_calls_show_their_file_path() {
        let view = ToolPresentation::new(false);
        let started = event(
            Uuid::new_v4(),
            EventKind::ToolStarted {
                call_id: "read".into(),
                name: "Read".into(),
                input: json!({"file_path":"/work/notes.md","offset":3}),
            },
        );
        assert_eq!(
            view.render_event(&started).unwrap(),
            "\n● Read · /work/notes.md\n"
        );
        let request = PermissionRequest {
            id: Uuid::new_v4(),
            loop_id: Uuid::new_v4(),
            execution: tickloop_agent_protocol::PermissionExecution::ToolCall {
                run_id: Uuid::new_v4(),
                call_id: "read".into(),
            },
            name: "Read".into(),
            input: json!({"file_path":"/outside/notes.md","offset":3}),
            file_change: None,
            approval_reason: None,
            approval_source: PermissionApprovalSource::default(),
        };
        let context = permission_context(&request);
        assert_eq!(context[0], "Read is waiting for your approval.");
        assert_eq!(context[1..3], ["Path:", "/outside/notes.md"]);
    }

    #[test]
    fn permission_context_keeps_full_command_and_edit_content() {
        let command = format!("printf {} ; rm important.txt", "long".repeat(2000));
        let mut request = PermissionRequest {
            id: Uuid::new_v4(),
            loop_id: Uuid::new_v4(),
            execution: tickloop_agent_protocol::PermissionExecution::ToolCall {
                run_id: Uuid::new_v4(),
                call_id: "call".into(),
            },
            name: "Bash".into(),
            input: json!({"command":command,"cwd":"/work\u{1b}[2J"}),
            file_change: None,
            approval_reason: None,
            approval_source: PermissionApprovalSource::default(),
        };
        let context = permission_context(&request).join("\n");
        assert!(context.contains(&command));
        assert!(context.contains("/work"));
        assert!(!context.contains('\u{1b}'));
        request.name = "Edit".into();
        request.input = json!({"file_path":"/work/config","old_string":"old\ntext\n","new_string":"new\ncontent\n"});
        let context = permission_context(&request).join("\n");
        assert!(context.contains("/work/config"));
        assert!(context.contains("old\ntext\n"));
        assert!(context.contains("new\ncontent\n"));
    }

    #[test]
    fn permission_context_names_the_current_safety_check_source() {
        let request: PermissionRequest = serde_json::from_value(json!({
            "id": Uuid::new_v4(),
            "loop_id": Uuid::new_v4(),
            "execution": {"ToolCall":{"run_id":Uuid::new_v4(),"call_id":"call"}},
            "name": "Bash",
            "input": {"command":"cat ~/.ssh/id_pub"},
            "approval_reason": "credential access requires human approval",
            "approval_source": "safety_check"
        }))
        .unwrap();

        let context = permission_context(&request).join("\n");
        assert!(context.contains("Reason: credential access requires human approval"));
        assert!(context.contains("Enforced by: safety check"));
        assert!(context.contains("saved rules cannot bypass"));
    }

    #[test]
    fn failed_commands_keep_their_exit_code_and_reason_without_expanding_all_output() {
        let result = ToolResult {
            content: format!(
                "Exit code 17\nprocess result could not be confirmed\n{}",
                "actual command output\n".repeat(1000)
            ),
            is_error: true,
            output_ref: None,
        };
        let rendered = compact_result("Bash", None, &result);
        assert!(rendered.contains("Bash · Failed"));
        assert!(rendered.contains("Exit code 17"));
        assert!(rendered.contains("process result could not be confirmed"));
        assert!(rendered.contains("actual command output"));
        assert!(rendered.contains("Ctrl+O"));
        assert!(rendered.len() < 500);
    }

    #[test]
    fn retained_details_are_bounded_and_exclude_model_content() {
        let mut view = ToolPresentation::new(false);
        let run = Uuid::new_v4();
        view.record(&event(run, EventKind::Text("private-model-data".into())));
        for index in 0..(RETAINED_TOOL_EVENTS + 1) {
            view.record(&event(
                run,
                EventKind::ToolStarted {
                    call_id: format!("call-{index}"),
                    name: "Bash".into(),
                    input: json!({"command":format!("command-{index}")}),
                },
            ));
        }
        let details = view.toggle();
        assert_eq!(view.events.len(), RETAINED_TOOL_EVENTS);
        assert!(!details.contains("Tool call-0 ·"));
        assert!(!details.contains("private-model-data"));
        assert!(details.contains("Tool call-128 ·"));
        assert!(view.is_detailed());
        assert!(view.toggle().contains("Compact"));
    }
}

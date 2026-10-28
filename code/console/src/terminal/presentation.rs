use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::Path,
};
use uuid::Uuid;

use serde_json::Value;
use tickloop_harness::tools::{
    ASK_USER_QUESTION_TOOL_NAME, BASH_TOOL_NAME, DECLINED_QUESTIONS_RESULT, EDIT_TOOL_NAME,
    READ_TOOL_NAME, TASK_STOP_TOOL_NAME, TODO_WRITE_TOOL_NAME, WEB_FETCH_TOOL_NAME,
    WEB_SEARCH_TOOL_NAME, WRITE_TOOL_NAME,
    file_change::{FileChangeLineKind, FileChangeReview, load_file_change_review},
};
use tickloop_ui::markdown;

use super::{
    agent_messaging::{self, AgentColors, AgentInput, AgentMessage},
    approval_status::{ApprovalStatus, ToolApprovals, result_runs},
    console::Console,
    conversation::{
        AssistantState, ConversationBlockId, ConversationBlockKind, ConversationBlockMetadata,
        ConversationOutcome, ThinkingSource, project as project_conversation,
    },
    display::{DisplayProfile, Glyph},
    hyperlink,
    image_cache::ImageLinks,
    safe_text,
    scrollback::{CollapsedCall, ToolCallState},
    style::Role,
    transcript::{TranscriptDocument, TranscriptPiece},
};
use tickloop_harness::protocol::{
    ChatSnapshot, CommandState, Event, EventKind, FamilyInputRecord, LocalMessage, LoopState,
    OperationRecord, OperationState, PermissionApprovalSource, PermissionRequest, ProposedToolCall,
    SEND_AGENT_MESSAGE_TOOL_NAME, SUBAGENT_TOOL_NAME, SubagentReportMessage,
    TaskNotificationStatus, TickDeliveryLine, ToolResult, UserShellRecord, UserShellState,
    WebSearchFailure, WebSearchSuggestion,
};

const RETAINED_TOOL_EVENTS: usize = 128;
// An ExecuteTools batch makes calls of its own under its call.
const EXECUTE_TOOLS_NAME: &str = "ExecuteTools";
const SEARCH_TOOLS_NAME: &str = "SearchTools";
const DESCRIBE_TOOLS_NAME: &str = "DescribeTools";
const PREVIEW_LINES: usize = 5;
/// Lines a failed result keeps from its start: the exit code and what ended the command.
const FAILURE_HEAD_LINES: usize = 2;
const PREVIEW_LINE_CHARACTERS: usize = 200;
const TOOL_SUMMARY_CHARACTERS: usize = 160;
const BACKGROUND_RUNNING_STATUS: &str = "Running in background";
/// Claude Code places a reply's model name this many columns before the right edge.
const MODEL_RIGHT_MARGIN: usize = 8;
/// Content under a `⎿` connector starts this many columns in: two spaces, `⎿` and two spaces.
const RESULT_INDENT: usize = 5;
/// A tool call's arguments and a thinking block wrap under this column, past the `⏺ ` gutter.
const BLOCK_INDENT: usize = 2;
const SUMMARY_HEADING: &str = "Summarized conversation";
const SHELL_HEADING: &str = "Shell";
const WAITING_RESPONSE_NOTICE: &str =
    "A response is waiting. Return to the conversation to answer.";
const NO_OUTPUT: &str = "(No output)";
/// The hint under output that was cut short.
const DETAILS_HINT: &str = "… (ctrl+o to expand)";
/// How the runtime's result starts when the user declined the call, with or without a reason.
const REJECTION_PREFIXES: [&str; 3] = [
    "permission denied; the requested operation was not executed",
    "The user denied permission for this operation",
    DECLINED_QUESTIONS_RESULT,
];
/// Claude Code's `AskUserQuestion` result headings, the Agent in place of Claude.
const DECLINED_QUESTIONS: &str = "User declined to answer questions";
const ANSWERED_QUESTIONS: &str = "User answered the Agent's questions:";
const ANSWERED_QUESTIONS_PREFIX: &str = "User has answered your questions: ";
const ANSWERED_QUESTIONS_SUFFIX: &str = ". You can now continue with the user's answers in mind.";
const ANSWER_PREVIEW_MARKER: &str = "\" selected preview:\n";
/// Lines of a written file shown under its result, as Claude Code's `MAX_LINES_TO_RENDER`.
const WRITE_PREVIEW_LINES: usize = 10;
/// Diff lines shown under an edit before the rest is left to Ctrl+O.
const DIFF_PREVIEW_LINES: usize = 20;

/// Styled runs of conversation output, each tagged with the role it plays.
pub(super) type RoleRuns = Vec<(String, Option<Role>)>;

pub(super) fn joined(runs: &[(String, Option<Role>)]) -> String {
    runs.iter().map(|(text, _)| text.as_str()).collect()
}

type LinkRange = (std::ops::Range<usize>, hyperlink::Hyperlink);

/// Tools whose calls show no row of their own, as Claude Code draws none of these calls:
/// TodoWrite's list shows apart from the conversation, a sent Agent message shows in its
/// receiver's conversation as Claude Code's SendMessage to a teammate, and AskUserQuestion's
/// answers or decline stand in for its call. A failed TodoWrite or SendMessage shows its row, and
/// the Ctrl+O transcript lists a failed call of any of them.
pub(super) fn has_no_call_row(name: &str) -> bool {
    matches!(
        name,
        TODO_WRITE_TOOL_NAME | SEND_AGENT_MESSAGE_TOOL_NAME | ASK_USER_QUESTION_TOOL_NAME
    )
}

/// Tools whose successful result shows nothing either, so the call shows only when it failed.
fn shows_only_failure(name: &str) -> bool {
    matches!(name, TODO_WRITE_TOOL_NAME | SEND_AGENT_MESSAGE_TOOL_NAME)
}
const BACKGROUND_LAUNCH_PREFIXES: [&str; 2] = [
    "Command running in background with ID: ",
    "Command was manually backgrounded by user with ID: ",
];
/// Renders the interruption marker following Claude Code's `InterruptedByUser`.
const RESPONSE_CONTINUATION: &str = "     ";
const INTERRUPTED: &str = "Interrupted · What should the Agent do instead?";
#[cfg(test)]
const INTERRUPTED_NOTICE: &str = "  ⎿  Interrupted · What should the Agent do instead?";
/// What a diff shows between two of its hunks, as Claude Code's `StructuredDiffList`.
const HUNK_GAP: &str = "...";

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
                let (index, after_tool) = self.replies.entry(run.into()).or_default();
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
                self.replies.entry(run.into()).or_default().1 = true;
                format!("tool:{call_id}")
            }
            EventKind::ApprovalReviewStarted { call_id, .. } => format!("tool:{call_id}"),
            EventKind::ApprovalRecorded { record } => format!("tool:{}", record.call_id),
            EventKind::PermissionRequested { request } => {
                format!("tool:{}", request.execution.call_key())
            }
            EventKind::QuestionAsked { request } => format!("question:{}", request.id),
            EventKind::PlanUpdated { plan } => {
                format!("plan:{}:{}", plan.loop_id.public(), plan.version)
            }
            EventKind::ProgressUpdated { progress } => {
                format!("progress:{}", progress.loop_id.public())
            }
            EventKind::CommandUpdated { command } => {
                format!("command:{}:{:?}", command.operation_id, command.state)
            }
            EventKind::Failed(_) | EventKind::Interrupted => "outcome".into(),
            _ => return None,
        };
        Some(format!("{}:{key}", run.uuid()))
    }
}

/// The conversation as the detailed transcript shows it: every block expanded, each tool result
/// under its call, the time and model above each reply, and every thinking block in place, as
/// Codex's transcript overlay keeps each reasoning block.
pub(super) fn transcript_document(
    snapshot: &ChatSnapshot,
    display: DisplayProfile,
    image_links: &ImageLinks,
    colors: &AgentColors,
) -> TranscriptDocument {
    let approvals = ToolApprovals::from_snapshot(snapshot);
    let blocks = project_conversation(snapshot).blocks;
    let results = blocks
        .iter()
        .filter_map(|block| match &block.kind {
            ConversationBlockKind::ToolResult { exchange, .. } => Some((exchange.clone(), block)),
            _ => None,
        })
        .collect::<HashMap<_, _>>();
    let mut writer = TranscriptWriter {
        document: TranscriptDocument::default(),
        display,
    };
    let tick_labels = super::tick_labels::TickLabels::from_snapshot(snapshot);
    let mut answered = HashSet::new();
    let mut waiting_notice_written = false;
    for block in &blocks {
        match &block.kind {
            ConversationBlockKind::User { text } => {
                let is_agent_message = is_agent_message(&block.id, snapshot);
                match typed_local_message(text, is_agent_message)
                    .map(|message| name_local_ticks(message, &tick_labels))
                {
                    Some(LocalMessage::BashInput { command }) => writer.shell_input(&command),
                    Some(local) if is_agent_message => {
                        let message_id = block_message_id(&block.id, snapshot);
                        match agent_messaging::agent_input(snapshot, message_id, &local) {
                            AgentInput::Result(completion) => {
                                if completion.notification.is_background() {
                                    writer.agent_block(agent_messaging::completion_runs(
                                        completion,
                                        snapshot,
                                        colors,
                                        OutputDetail::Full,
                                        display,
                                    ));
                                }
                            }
                            AgentInput::PendingResult | AgentInput::Message => {
                                match AgentMessage::of(&local) {
                                    Some(message) => {
                                        writer.agent_block(agent_messaging::message_runs(
                                            &message,
                                            OutputDetail::Full,
                                            display,
                                        ))
                                    }
                                    None => writer.block_text(
                                        local_message_text(&local, OutputDetail::Full, display),
                                        Some(Role::MutedText),
                                    ),
                                }
                            }
                        }
                    }
                    Some(message) => writer.block_text(
                        local_message_text(&message, OutputDetail::Full, display),
                        Some(Role::MutedText),
                    ),
                    None => {
                        if let ConversationBlockId::MessageContent { message_index, .. } = &block.id
                            && let Some(origin) = snapshot
                                .message_ids
                                .get(message_index)
                                .and_then(|id| snapshot.human_family_origins.get(id))
                        {
                            writer.block_text(
                                format!(
                                    "\nUser message · from @{} View\nOriginal input: {}\n",
                                    safe_text(&origin.source_handle),
                                    safe_text(&origin.original_submission)
                                ),
                                Some(Role::MutedText),
                            );
                        }
                        let text = safe_text(text);
                        writer.prompt(&text, &image_links.in_text(snapshot.id.into(), &text));
                        if let Some(skill) = block
                            .metadata
                            .run_id
                            .and_then(|run| snapshot.user_skill_invocations.get(&run.into()))
                        {
                            writer.runs((user_skill_runs(skill, display), Vec::new()));
                        }
                    }
                }
            }
            ConversationBlockKind::CompactBoundary => {
                writer.block_text(
                    format!(
                        "\n{} Conversation compacted\n",
                        display.glyph(super::display::Glyph::TurnSummary)
                    ),
                    Some(Role::MutedText),
                );
            }
            ConversationBlockKind::CompactSummary { summary } => {
                writer.block();
                writer.hanging(
                    format!("{} ", display.glyph(Glyph::Assistant)),
                    None,
                    BLOCK_INDENT,
                );
                writer.hanging(SUMMARY_HEADING, None, BLOCK_INDENT);
                writer.text("\n", None);
                writer.result(text_lines(&safe_text(summary), Some(Role::MutedText)));
            }
            // As in Claude Code, a reply still streaming is not part of the transcript.
            ConversationBlockKind::Assistant {
                text,
                state: AssistantState::Complete,
            } => writer.reply(text, &block.metadata),
            ConversationBlockKind::Assistant { .. } => {}
            ConversationBlockKind::Thinking { text, source } => writer.thinking(text, *source),
            ConversationBlockKind::ToolCall {
                exchange,
                name,
                input,
            } => {
                answered.insert(exchange.clone());
                let result = results.get(exchange).copied();
                let content = result.and_then(|result| match &result.kind {
                    ConversationBlockKind::ToolResult {
                        content, is_error, ..
                    } => Some((content, *is_error)),
                    _ => None,
                });
                if let Some(question) = content
                    .filter(|_| name == ASK_USER_QUESTION_TOOL_NAME)
                    .and_then(|(content, is_error)| {
                        question_result_runs(
                            Some(input),
                            &ToolResult {
                                content: content.clone(),
                                is_error,
                                output_ref: None,
                            },
                            display.glyph(Glyph::ToolCall),
                            display.glyph(Glyph::ToolResult),
                        )
                    })
                {
                    writer.runs(question);
                    continue;
                }
                let failed = content.is_some_and(|(_, is_error)| is_error);
                if has_no_call_row(name) && !failed {
                    continue;
                }
                let dot = match result {
                    None => Role::MutedText,
                    Some(_) => {
                        tool_result_state(name, content.map_or("", |(content, _)| content), failed)
                            .dot_role()
                    }
                };
                let launch = content
                    .filter(|_| name == SUBAGENT_TOOL_NAME)
                    .and_then(|(content, _)| serde_json::from_str::<Value>(content).ok());
                let arguments = match &launch {
                    Some(launch) => agent_messaging::subagent_call_label(input, Some(launch)),
                    None => transcript_arguments(name, input),
                };
                writer.tool_call(dot, name, &arguments);
                if let Some(run) = exchange.run_id {
                    for operation in nested_operations(snapshot, run, &exchange.call_id) {
                        writer.nested_call(operation, approvals.get(run, &operation.call_id));
                    }
                }
                if let (Some(result), Some((content, is_error))) = (result, content) {
                    let elapsed = block
                        .metadata
                        .timestamp_ms
                        .zip(result.metadata.timestamp_ms)
                        .map(|(started, finished)| finished.saturating_sub(started));
                    let report = launch
                        .as_ref()
                        .and_then(|launch| agent_messaging::subagent_report(snapshot, launch));
                    let mut lines = transcript_result_lines(
                        name,
                        Some(input),
                        content,
                        is_error,
                        elapsed,
                        report.as_ref(),
                    );
                    if let Some(run) = exchange.run_id {
                        if name == EXECUTE_TOOLS_NAME
                            && let Some(summary) = settled_batch_summary(content, &approvals, run)
                                .or_else(|| {
                                    snapshot_batch_summary(
                                        snapshot,
                                        run,
                                        &exchange.call_id,
                                        Some(input),
                                    )
                                })
                        {
                            lines.insert(0, ResultLine::new(summary, Some(Role::MutedText)));
                        }
                        lines = approval_result_lines(lines, approvals.get(run, &exchange.call_id));
                    }
                    writer.result(lines);
                } else if let Some(run) = exchange.run_id {
                    let mut lines = Vec::new();
                    if name == EXECUTE_TOOLS_NAME
                        && let Some(summary) =
                            snapshot_batch_summary(snapshot, run, &exchange.call_id, Some(input))
                    {
                        lines.push(ResultLine::new(summary, Some(Role::MutedText)));
                    }
                    writer.result(approval_result_lines(
                        lines,
                        approvals.get(run, &exchange.call_id),
                    ));
                }
            }
            ConversationBlockKind::ToolResult {
                exchange,
                content,
                is_error,
                ..
            } if !answered.contains(exchange) => {
                writer.tool_call(
                    if *is_error {
                        Role::Error
                    } else {
                        Role::Success
                    },
                    "tool",
                    "",
                );
                writer.result(transcript_result_lines(
                    "tool", None, content, *is_error, None, None,
                ));
            }
            ConversationBlockKind::ToolResult { .. } | ConversationBlockKind::Plan => {}
            ConversationBlockKind::Permission | ConversationBlockKind::Question => {
                if !waiting_notice_written {
                    writer.block_text(
                        format!("\n{WAITING_RESPONSE_NOTICE}\n"),
                        Some(Role::MutedText),
                    );
                    waiting_notice_written = true;
                }
            }
            ConversationBlockKind::FamilyInput { record } => writer.block_text(
                family_input_text(record, OutputDetail::Full),
                Some(Role::MutedText),
            ),
            ConversationBlockKind::FamilyCompletion { completion } => {
                writer.agent_block(agent_messaging::completion_runs(
                    completion,
                    snapshot,
                    colors,
                    OutputDetail::Full,
                    display,
                ))
            }
            ConversationBlockKind::Outcome(ConversationOutcome::Failed { error }) => {
                writer.block();
                writer.hanging(
                    format!("{} ", display.glyph(Glyph::ToolCall)),
                    Some(Role::Error),
                    BLOCK_INDENT,
                );
                writer.hanging(safe_text(error), Some(Role::Error), BLOCK_INDENT);
                writer.text("\n", None);
            }
            ConversationBlockKind::Outcome(ConversationOutcome::Interrupted) => {
                writer.text(
                    format!("{}\n", interrupted_notice(display)),
                    Some(Role::MutedText),
                );
            }
            ConversationBlockKind::Outcome(ConversationOutcome::Completed) => {}
            ConversationBlockKind::WakeupCancelled => {
                writer.block_text("Pending wakeup cancelled\n", Some(Role::MutedText))
            }
            ConversationBlockKind::GoalContinuation => writer.block_text(
                format!("{}\n", tickloop_harness::protocol::GOAL_CONTINUATION_EVENT),
                Some(Role::MutedText),
            ),
            ConversationBlockKind::Command { command } => {
                let dot = match command.state {
                    CommandState::Completed if command.exit_code == Some(0) => Role::Success,
                    CommandState::Completed | CommandState::Failed | CommandState::Unknown => {
                        Role::Error
                    }
                    _ => Role::MutedText,
                };
                writer.tool_call(dot, SHELL_HEADING, &safe_text(&command.command));
                let mut lines = vec![
                    format!(
                        "{:?}{}",
                        command.state,
                        command_status(command.exit_code, command.signal)
                    ),
                    format!("Directory: {}", safe_text(&command.cwd.to_string_lossy())),
                    format!(
                        "Output: {} bytes · {}",
                        command.output_bytes,
                        safe_text(&command.output_ref.to_string_lossy())
                    ),
                ];
                if let Some(reason) = &command.reason {
                    lines.push(format!("Reason: {}", safe_text(reason)));
                }
                writer.result(
                    lines
                        .into_iter()
                        .map(|line| ResultLine::new(line, Some(Role::MutedText)))
                        .collect(),
                );
            }
        }
    }
    writer.document
}

fn is_agent_message(id: &ConversationBlockId, snapshot: &ChatSnapshot) -> bool {
    block_message_id(id, snapshot)
        .is_some_and(|id| snapshot.agent_message_origins.contains_key(&id))
}

fn block_message_id(id: &ConversationBlockId, snapshot: &ChatSnapshot) -> Option<Uuid> {
    match id {
        ConversationBlockId::MessageContent { message_index, .. } => {
            snapshot.message_ids.get(message_index).copied()
        }
        _ => None,
    }
}

/// One row under a `⎿` result connector, painted with `fill` across the row.
struct ResultLine {
    connector: bool,
    spans: Vec<(String, Option<Role>)>,
    fill: Option<Role>,
}

impl ResultLine {
    fn new(text: impl Into<String>, role: Option<Role>) -> Self {
        Self {
            connector: false,
            spans: vec![(text.into(), role)],
            fill: None,
        }
    }

    fn filled(text: impl Into<String>, fill: Role) -> Self {
        Self {
            connector: false,
            spans: vec![(text.into(), None)],
            fill: Some(fill),
        }
    }
}

fn text_lines(text: &str, role: Option<Role>) -> Vec<ResultLine> {
    text.lines()
        .map(|line| ResultLine::new(line, role))
        .collect()
}

fn approval_result_lines(
    mut lines: Vec<ResultLine>,
    approval: Option<&ApprovalStatus>,
) -> Vec<ResultLine> {
    let Some(approval) = approval else {
        return lines;
    };
    if let Some(label) = approval.result_label() {
        let line = ResultLine::new(label, Some(Role::MutedText));
        if lines.is_empty() {
            lines.push(line);
        } else {
            lines[0] = line;
        }
    }
    if let Some(label) = approval.allowed_label() {
        let mut line = ResultLine::new(label, Some(Role::MutedText));
        line.connector = true;
        lines.push(line);
    }
    if let Some(context) = approval.human_context() {
        lines.push(ResultLine::new(context, Some(Role::MutedText)));
    }
    lines
}

struct TranscriptWriter {
    document: TranscriptDocument,
    display: DisplayProfile,
}

impl TranscriptWriter {
    fn text(&mut self, text: impl Into<String>, role: Option<Role>) {
        self.document.push(TranscriptPiece::text(text, role));
    }

    fn block(&mut self) {
        self.document.push(TranscriptPiece::Block);
    }

    /// `text` as one entry: when it begins with a newline it starts a block of its own.
    fn block_text(&mut self, text: impl Into<String>, role: Option<Role>) {
        let text = text.into();
        let rest = text.trim_start_matches('\n');
        if rest.len() < text.len() {
            self.block();
        }
        self.text(rest, role);
    }

    /// A block of styled runs: its heading under the `⏺ ` gutter, then its `⎿` rows.
    fn runs(&mut self, (heading, response): (RoleRuns, RoleRuns)) {
        self.block();
        for (text, role) in heading {
            let text = text.trim_start_matches('\n');
            if !text.is_empty() {
                self.hanging(text, role, BLOCK_INDENT);
            }
        }
        for (text, role) in response {
            self.hanging(text, role, RESULT_INDENT);
        }
    }

    /// An Agent block: its heading under the gutter mark, then its body indented under it.
    fn agent_block(&mut self, (heading, body): (RoleRuns, RoleRuns)) {
        self.block();
        let runs = heading.into_iter().skip_while(|(text, _)| text == "\n");
        for (text, role) in runs.chain(body) {
            self.hanging(text, role, BLOCK_INDENT);
        }
    }

    fn hanging(&mut self, text: impl Into<String>, role: Option<Role>, hang: usize) {
        self.document
            .push(TranscriptPiece::hanging(text, role, hang));
    }

    fn filled(&mut self, text: &str, role: Option<Role>, fill: Role, links: &[LinkRange]) {
        for (run, link) in hyperlink::runs(text, links) {
            self.document.push(TranscriptPiece::Text {
                text: run.to_owned(),
                role,
                fill: Some(fill),
                hang: 0,
                link: link.cloned(),
            });
        }
    }

    fn prompt(&mut self, text: &str, links: &[LinkRange]) {
        self.block();
        self.filled(
            &format!("{} ", self.display.glyph(Glyph::Prompt)),
            Some(Role::Subtle),
            Role::UserMessageBg,
            &[],
        );
        self.filled(text, Some(Role::Text), Role::UserMessageBg, links);
        self.text("\n", None);
    }

    fn shell_input(&mut self, command: &str) {
        self.block();
        self.filled("! ", Some(Role::ShellBorder), Role::ShellMessageBg, &[]);
        self.filled(
            &safe_text(command),
            Some(Role::Text),
            Role::ShellMessageBg,
            &[],
        );
        self.text("\n", None);
    }

    fn reply(&mut self, text: &str, metadata: &ConversationBlockMetadata) {
        self.block();
        let time = metadata.timestamp_ms.and_then(local_clock_time);
        let model = metadata
            .model
            .as_ref()
            .map(|model| super::safe_line(model))
            .filter(|model| !model.is_empty());
        if time.is_some() || model.is_some() {
            let mut spans = Vec::new();
            spans.extend(time.map(|time| (time, Some(Role::MutedText))));
            if let Some(model) = &model {
                if !spans.is_empty() {
                    spans.push((" ".to_owned(), None));
                }
                spans.push((model.clone(), Some(Role::MutedText)));
            }
            self.document.push(TranscriptPiece::RightAligned {
                spans,
                right_margin: if model.is_some() {
                    MODEL_RIGHT_MARGIN
                } else {
                    0
                },
            });
        }
        self.document.push(TranscriptPiece::Markdown {
            text: safe_text(text),
            lead: format!("{} ", self.display.glyph(Glyph::Assistant)),
        });
        self.text("\n", None);
    }

    fn thinking(&mut self, text: &str, source: ThinkingSource) {
        let heading = match source {
            ThinkingSource::ModelText => "∴ Thinking…",
            ThinkingSource::Summary => "∴ Thinking summary",
        };
        self.block();
        self.text(heading, Some(Role::ThinkingHeading));
        self.text("\n\n", None);
        let mut body = String::new();
        for line in markdown::plain_text(&safe_text(text)).lines() {
            body.push_str(&" ".repeat(BLOCK_INDENT));
            body.push_str(line);
            body.push('\n');
        }
        self.hanging(body, Some(Role::MutedText), BLOCK_INDENT);
    }

    fn tool_call(&mut self, dot: Role, name: &str, arguments: &str) {
        self.block();
        self.hanging(
            format!("{} ", self.display.glyph(Glyph::ToolCall)),
            Some(dot),
            BLOCK_INDENT,
        );
        self.hanging(tool_name(name), Some(Role::ToolName), BLOCK_INDENT);
        if !arguments.is_empty() {
            let continuation = format!("\n{}", " ".repeat(BLOCK_INDENT));
            self.hanging(
                format!("({})", arguments.replace('\n', &continuation)),
                None,
                BLOCK_INDENT,
            );
        }
        self.text("\n", None);
    }

    /// A call a batch made, with its result, indented under the call that made it.
    fn nested_call(&mut self, operation: &OperationRecord, approval: Option<&ApprovalStatus>) {
        let hang = RESULT_INDENT + BLOCK_INDENT;
        let dot = match operation.state {
            OperationState::Completed => Role::Success,
            OperationState::Failed => Role::Error,
            OperationState::Unknown => Role::Warning,
            OperationState::Proposed | OperationState::Admitted => Role::MutedText,
        };
        self.hanging(" ".repeat(RESULT_INDENT), None, hang);
        self.hanging(
            format!("{} ", self.display.glyph(Glyph::ToolCall)),
            Some(dot),
            hang,
        );
        self.hanging(tool_name(&operation.name), Some(Role::ToolName), hang);
        let arguments = transcript_arguments(&operation.name, &operation.input);
        if !arguments.is_empty() {
            let continuation = format!("\n{}", " ".repeat(hang));
            self.hanging(
                format!("({})", arguments.replace('\n', &continuation)),
                None,
                hang,
            );
        }
        self.text("\n", None);
        let Some(result) = &operation.result else {
            self.result_indented(RESULT_INDENT, approval_result_lines(Vec::new(), approval));
            return;
        };
        let lines = transcript_result_lines(
            &operation.name,
            Some(&operation.input),
            &result.content,
            result.is_error,
            None,
            None,
        );
        self.result_indented(RESULT_INDENT, approval_result_lines(lines, approval));
    }

    /// Rows under a `⎿` connector, every row and its wrapped continuation indented to the
    /// content, as Claude Code's `MessageResponse` lays a result out.
    fn result(&mut self, lines: Vec<ResultLine>) {
        self.result_indented(0, lines);
    }

    /// [`Self::result`] moved `indent` columns right.
    fn result_indented(&mut self, indent: usize, lines: Vec<ResultLine>) {
        let hang = indent + RESULT_INDENT;
        for (index, line) in lines.into_iter().enumerate() {
            let lead = if index == 0 || line.connector {
                format!(
                    "{}  {}  ",
                    " ".repeat(indent),
                    self.display.glyph(Glyph::ToolResult)
                )
            } else {
                " ".repeat(hang)
            };
            self.document.push(TranscriptPiece::Text {
                text: lead,
                role: Some(Role::MutedText),
                fill: line.fill,
                hang,
                link: None,
            });
            for (text, role) in line.spans {
                self.document.push(TranscriptPiece::Text {
                    text,
                    role,
                    fill: line.fill,
                    hang,
                    link: None,
                });
            }
            self.text("\n", None);
        }
    }
}

/// A call's arguments as the detailed transcript shows them: whole commands and absolute paths
/// where Claude Code's verbose rendering shows them, otherwise every field.
fn transcript_arguments(name: &str, input: &Value) -> String {
    let field = |key: &str| input[key].as_str().map(safe_text).unwrap_or_default();
    match name {
        BASH_TOOL_NAME => field("command"),
        READ_TOOL_NAME => {
            let path = field("file_path");
            match (input["offset"].as_u64(), input["limit"].as_u64()) {
                (Some(offset), Some(limit)) => format!(
                    "{path} · lines {offset}-{}",
                    offset.saturating_add(limit).saturating_sub(1)
                ),
                (Some(offset), None) => format!("{path} · from line {offset}"),
                (None, Some(limit)) => format!("{path} · lines 1-{limit}"),
                (None, None) => path,
            }
        }
        WRITE_TOOL_NAME | EDIT_TOOL_NAME => field("file_path"),
        SUBAGENT_TOOL_NAME => field("description"),
        _ => match input {
            Value::Object(fields) => fields
                .iter()
                .map(|(key, value)| {
                    format!(
                        "{}: {}",
                        safe_text(key),
                        safe_text(&serde_json::to_string(value).unwrap_or_default())
                    )
                })
                .collect::<Vec<_>>()
                .join(", "),
            Value::Null => String::new(),
            other => safe_text(&other.to_string()),
        },
    }
}

fn web_search_failure_lines(name: &str, content: &str) -> Option<Vec<ResultLine>> {
    if name != WEB_SEARCH_TOOL_NAME {
        return None;
    }
    let failure: WebSearchFailure = serde_json::from_str(content).ok()?;
    let mut lines = vec![ResultLine::new("Failed", Some(Role::Error))];
    lines.extend(text_lines(
        &format!("Reason: {}", safe_text(&failure.reason)),
        Some(Role::Error),
    ));
    if failure.suggestions.is_empty() {
        return Some(lines);
    }
    lines.push(ResultLine::new("", None));
    lines.push(ResultLine::new("Next steps", Some(Role::Strong)));
    let mut notes = Vec::new();
    for suggestion in failure.suggestions {
        match suggestion {
            WebSearchSuggestion::OpenVerification { url, note } => {
                lines.push(ResultLine::new(
                    "• Open the verification page in your browser, then retry:",
                    None,
                ));
                lines.push(ResultLine::new(
                    format!("  {}", safe_text(&url)),
                    Some(Role::Link),
                ));
                notes.push(note);
            }
            WebSearchSuggestion::ChangeProvider { command } => lines.push(ResultLine {
                connector: false,
                spans: vec![
                    ("• Choose another search provider: ".into(), None),
                    (safe_text(&command), Some(Role::Accent)),
                ],
                fill: None,
            }),
        }
    }
    for note in notes {
        lines.extend(text_lines(&safe_text(&note), Some(Role::MutedText)));
    }
    Some(lines)
}

/// A result in full: no line limit and no expansion hint.
fn transcript_result_lines(
    name: &str,
    input: Option<&Value>,
    content: &str,
    is_error: bool,
    elapsed_ms: Option<u64>,
    report: Option<&SubagentReportMessage>,
) -> Vec<ResultLine> {
    let value = serde_json::from_str::<Value>(content).ok();
    if name == SUBAGENT_TOOL_NAME
        && let Some(value) = &value
    {
        return subagent_result_lines(input, value, elapsed_ms, report);
    }
    if let Some(reason) = goal_completion_rejection_reason(name, content, is_error) {
        let mut lines = vec![ResultLine::new("Not complete yet", Some(Role::Warning))];
        lines.extend(text_lines(
            &format!("Reason: {}", safe_text(reason)),
            Some(Role::MutedText),
        ));
        return lines;
    }
    if is_error && let Some(lines) = web_search_failure_lines(name, content) {
        return lines;
    }
    if is_error {
        let text = safe_text(content);
        let text = if text.trim_start().to_lowercase().starts_with("error") {
            text
        } else {
            format!("Error: {text}")
        };
        return text_lines(&text, Some(Role::Error));
    }
    let input = input.unwrap_or(&Value::Null);
    match name {
        READ_TOOL_NAME
            if let Some(media_type) = value
                .as_ref()
                .and_then(|value| value["media_type"].as_str()) =>
        {
            let value = value.as_ref().unwrap();
            return vec![ResultLine::new(
                format!(
                    "Read image · {} · {}×{} · {} bytes",
                    safe_text(media_type),
                    value["width"],
                    value["height"],
                    value["total_bytes"]
                ),
                None,
            )];
        }
        READ_TOOL_NAME if !content.starts_with("<system-reminder>") => {
            return vec![ResultLine {
                connector: false,
                spans: vec![
                    ("Read ".into(), None),
                    (content.lines().count().to_string(), Some(Role::Strong)),
                    (" lines".into(), None),
                ],
                fill: None,
            }];
        }
        WRITE_TOOL_NAME => return written_lines(input, "content"),
        EDIT_TOOL_NAME if input["old_string"] == "" => return written_lines(input, "new_string"),
        EDIT_TOOL_NAME => {
            let removed = input["old_string"].as_str().unwrap_or_default();
            let added = input["new_string"].as_str().unwrap_or_default();
            let count = |text: &str, word: &str| {
                let lines = text.lines().count();
                format!("{lines} {word}{}", if lines == 1 { "" } else { "s" })
            };
            let mut lines = vec![ResultLine::new(
                format!(
                    "Updated {} with {} and {}",
                    safe_text(input["file_path"].as_str().unwrap_or_default()),
                    count(added, "addition"),
                    count(removed, "removal")
                ),
                None,
            )];
            lines.extend(
                safe_text(removed)
                    .lines()
                    .map(|line| ResultLine::filled(format!("- {line}"), Role::DiffRemoved)),
            );
            lines.extend(
                safe_text(added)
                    .lines()
                    .map(|line| ResultLine::filled(format!("+ {line}"), Role::DiffAdded)),
            );
            return lines;
        }
        _ => {}
    }
    let text = match &value {
        Some(Value::Object(fields)) => ["output", "text", "answer"]
            .iter()
            .find_map(|key| fields.get(*key).and_then(Value::as_str))
            .map(safe_text)
            .unwrap_or_else(|| display_value(value.as_ref().unwrap())),
        Some(Value::String(text)) => safe_text(text),
        _ => safe_text(content),
    };
    if text.trim().is_empty() {
        return vec![ResultLine::new(NO_OUTPUT, Some(Role::MutedText))];
    }
    text_lines(&text, None)
}

fn written_lines(input: &Value, key: &str) -> Vec<ResultLine> {
    let content = safe_text(input[key].as_str().unwrap_or_default());
    let count = content.lines().count();
    let mut lines = vec![ResultLine::new(
        format!(
            "Wrote {count} {} to {}",
            if count == 1 { "line" } else { "lines" },
            safe_text(input["file_path"].as_str().unwrap_or_default())
        ),
        None,
    )];
    lines.extend(text_lines(&content, None));
    lines
}

/// Claude Code's expanded Agent result: the prompt, the response and how the run ended; a child
/// that went to the background shows that and its prompt. The response is the report the child
/// delivered.
fn subagent_result_lines(
    input: Option<&Value>,
    value: &Value,
    elapsed_ms: Option<u64>,
    report: Option<&SubagentReportMessage>,
) -> Vec<ResultLine> {
    let status = value["status"].as_str();
    let is_backgrounded = status == Some("async_launched");
    let indented = |text: &str| {
        safe_text(text)
            .lines()
            .map(|line| ResultLine::new(format!("{}{line}", " ".repeat(BLOCK_INDENT)), None))
            .collect::<Vec<_>>()
    };
    let mut lines = Vec::new();
    if is_backgrounded {
        lines.push(ResultLine::new(agent_messaging::BACKGROUNDED, None));
    }
    if let Some(prompt) = input
        .and_then(|input| input["task"].as_str())
        .filter(|prompt| !prompt.is_empty())
    {
        lines.push(ResultLine::new("Prompt:", Some(Role::AgentSection)));
        lines.extend(indented(prompt));
    }
    if is_backgrounded {
        return lines;
    }
    if let Some(report) = report {
        lines.push(ResultLine::new("Response:", Some(Role::AgentSection)));
        if let Some(note) = &report.note {
            lines.extend(text_lines(&safe_text(note), Some(Role::Warning)));
        }
        lines.extend(indented(&report.report));
    }
    if status == Some("completed") {
        let tokens = ["input_tokens", "output_tokens"]
            .iter()
            .filter_map(|key| value["usage"][*key].as_u64())
            .sum::<u64>();
        let details = [
            (tokens > 0).then(|| format!("{} tokens", compact_count(tokens))),
            elapsed_ms.map(compact_duration),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        lines.push(ResultLine::new(
            if details.is_empty() {
                "Done".to_owned()
            } else {
                format!("Done ({})", details.join(" · "))
            },
            None,
        ));
    } else {
        let label = match status {
            Some("interrupted") => "Interrupted",
            Some("stopped") => "Stopped",
            _ => "Failed",
        };
        let error = value["error"].as_str().unwrap_or_default();
        lines.extend(text_lines(
            &if error.is_empty() {
                label.to_owned()
            } else {
                format!("{label}: {}", safe_text(error))
            },
            Some(Role::Error),
        ));
    }
    lines
}

/// Claude Code's `formatNumber`: `950`, `12.3k`, `1.2m`.
pub(super) fn compact_count(count: u64) -> String {
    let scaled = |value: f64, suffix: &str| {
        let text = format!("{value:.1}");
        format!("{}{suffix}", text.strip_suffix(".0").unwrap_or(&text))
    };
    match count {
        0..1_000 => count.to_string(),
        1_000..1_000_000 => scaled(count as f64 / 1_000.0, "k"),
        _ => scaled(count as f64 / 1_000_000.0, "m"),
    }
}

/// Claude Code's `formatDuration`: `45s`, `1m 3s`, `2h 5m 0s`.
pub(super) fn compact_duration(milliseconds: u64) -> String {
    let seconds = milliseconds / 1_000;
    let (hours, minutes, seconds) = (seconds / 3_600, seconds % 3_600 / 60, seconds % 60);
    if hours > 0 {
        format!("{hours}h {minutes}m {seconds}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

/// `03:45 PM` in local time, as Claude Code stamps a reply.
fn local_clock_time(milliseconds: u64) -> Option<String> {
    // `localtime_r` names the type: musl's libc deprecates naming `time_t` directly.
    let timestamp = (milliseconds / 1_000).try_into().ok()?;
    let mut local = std::mem::MaybeUninit::<libc::tm>::uninit();
    // localtime_r initializes the output only when its return pointer is non-null.
    if unsafe { libc::localtime_r(&timestamp, local.as_mut_ptr()) }.is_null() {
        return None;
    }
    let local = unsafe { local.assume_init() };
    Some(twelve_hour_time(
        u32::try_from(local.tm_hour).ok()?,
        u32::try_from(local.tm_min).ok()?,
    ))
}

fn twelve_hour_time(hour: u32, minute: u32) -> String {
    let (hour, period) = match hour {
        0 => (12, "AM"),
        1..=11 => (hour, "AM"),
        12 => (12, "PM"),
        _ => (hour - 12, "PM"),
    };
    format!("{hour:02}:{minute:02} {period}")
}

pub(super) fn user_skill_runs(
    skill: &tickloop_harness::protocol::UserSkillInvocation,
    display: DisplayProfile,
) -> RoleRuns {
    vec![
        (
            format!("\n{} ", display.glyph(Glyph::ToolCall)),
            Some(Role::MutedText),
        ),
        ("Skill".into(), Some(Role::ToolName)),
        (format!(" · {}", super::safe_line(&skill.name)), None),
        (
            format!(" · {}\n", super::safe_line(&skill.source.display_label())),
            Some(Role::MutedText),
        ),
    ]
}

pub(super) struct ToolPresentation {
    detailed: bool,
    events: VecDeque<Event>,
    tick_labels: super::tick_labels::TickLabels,
    /// Calls a batch made, by their row's key.
    nested: HashMap<String, NestedCall>,
    approvals: ToolApprovals,
}

/// A call a batch made: the row of the call that made it, and what it calls.
struct NestedCall {
    parent_key: String,
    parent: String,
    ordinal: usize,
    result: Option<ToolResult>,
    name: String,
    input: Value,
}

/// The calls a batch made under `call_id` in `run`, in the order they were made.
pub(super) fn nested_operations<'a>(
    snapshot: &'a ChatSnapshot,
    run: Uuid,
    call_id: &'a str,
) -> impl Iterator<Item = &'a OperationRecord> {
    snapshot.operations.iter().filter(move |operation| {
        operation.run_id.uuid() == run && operation.parent_call_id.as_deref() == Some(call_id)
    })
}

/// Whether a call of `name` makes calls of its own that show under its row.
pub(super) fn makes_nested_calls(name: &str) -> bool {
    name == EXECUTE_TOOLS_NAME
}

/// Where a call's result finds what it shows beyond itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ResultSource {
    /// The result shows only itself.
    Itself,
    /// An edit's change, as the operation the run recorded for the call reviewed it.
    Operation,
    /// The report a Subagent's child delivered into the conversation.
    Conversation,
}

/// What a call's result shows from elsewhere in the conversation.
#[derive(Default)]
pub(super) struct CallFacts {
    /// The change an edit made, as its review recorded it.
    pub(super) review: Option<FileChangeReview>,
    /// The report a Subagent's child delivered apart from the call's result.
    pub(super) report: Option<SubagentReportMessage>,
}

impl ToolPresentation {
    /// Shows `event` in the conversation: a call's row as it is queued, runs and ends, and its
    /// result under the row. `facts` are what the result shows from elsewhere in the
    /// conversation; a Subagent's result also names its child in the call's row.
    pub(super) fn present_event(
        &mut self,
        console: &Console,
        event: &Event,
        facts: &CallFacts,
    ) -> Result<bool, String> {
        let ended_pending = if matches!(
            event.kind,
            EventKind::Completed { .. } | EventKind::Interrupted | EventKind::Failed(_)
        ) {
            self.approvals.pending_calls(event.run_id.into())
        } else {
            Vec::new()
        };
        self.record(event);
        if matches!(
            event.kind,
            EventKind::AssistantMessageCommitted { .. } | EventKind::ToolStarted { .. }
        ) {
            self.tick_labels = console.tick_labels()?;
        } else if matches!(event.kind, EventKind::ToolCompleted { .. }) {
            self.tick_labels.merge(console.tick_labels()?);
        }
        let display = console.display_profile()?;
        let cwd = console.working_directory()?;
        for (call_id, approval) in ended_pending {
            let (label, state) = match event.kind {
                EventKind::Failed(_) => ("Failed", ToolCallState::Failed),
                EventKind::Interrupted => ("Interrupted", ToolCallState::Stopped),
                _ => ("Not executed", ToolCallState::Stopped),
            };
            let key = tool_call_key(event.run_id.into(), &call_id);
            let mut runs = result_runs(label, display.glyph(Glyph::ToolResult));
            if let Some(context) = approval.human_context() {
                runs.push((format!("     {context}\n"), Some(Role::MutedText)));
            }
            console.write_tool_approval(&key, &runs)?;
            console.finish_tool_call(event.run_id.into(), Some((&key, state)))?;
            if let Some(nested) = self.nested.get(&key) {
                console.write_tool_result(
                    &nested.parent_key,
                    &result_runs(label, display.glyph(Glyph::ToolResult)),
                    false,
                )?;
                console.finish_tool_call(event.run_id.into(), Some((&nested.parent_key, state)))?;
            }
        }
        if let Some(call_id) = approval_event_call(event) {
            if let Some(status) = self.approvals.get(event.run_id.into(), call_id) {
                let runs = status.decorate(Vec::new(), display.glyph(Glyph::ToolResult));
                if runs.is_empty() {
                    return Ok(true);
                }
                let key = tool_call_key(event.run_id.into(), call_id);
                let (name, input) = self.started_call(event, call_id);
                if let Some(input) = input {
                    if let Some(nested) = self.nested.get(&key) {
                        console.nest_tool_call(&key, &nested.parent_key)?;
                    }
                    console.write_tool_call(
                        event.run_id.into(),
                        &key,
                        &tool_call_roles(name, input, &cwd, display, &self.tick_labels),
                        if status.denied() {
                            ToolCallState::Failed
                        } else if self.was_started(event, call_id) {
                            ToolCallState::Running
                        } else {
                            ToolCallState::Queued
                        },
                        None,
                    )?;
                }
                console.write_tool_approval(&key, &runs)?;
                if status.denied() {
                    console.finish_tool_call(
                        event.run_id.into(),
                        Some((&key, ToolCallState::Failed)),
                    )?;
                }
                self.present_parent_status(console, event, call_id, display)?;
            }
            return Ok(console.is_interactive()
                || !matches!(event.kind, EventKind::PermissionRequested { .. }));
        }
        // A call a batch made without starting, such as one it was denied, still
        // shows its row under the call that made it.
        if let EventKind::ToolCompleted {
            call_id, result, ..
        } = &event.kind
            && console.is_interactive()
            && !self.detailed
            && !self.was_started(event, call_id)
        {
            let key = tool_call_key(event.run_id.into(), call_id);
            if let Some(nested) = self.nested.get(&key) {
                console.nest_tool_call(&key, &nested.parent_key)?;
                console.write_tool_call(
                    event.run_id.into(),
                    &key,
                    &tool_call_roles(
                        &nested.name,
                        &nested.input,
                        &cwd,
                        display,
                        &self.tick_labels,
                    ),
                    tool_result_state(&nested.name, &result.content, result.is_error),
                    None,
                )?;
            }
        }
        for recorded in &self.events {
            if recorded.chat_id == event.chat_id
                && let EventKind::CommandUpdated { command } = &recorded.kind
            {
                self.tick_labels
                    .include_native(std::slice::from_ref(command), &[], &[], &cwd);
            }
        }
        match &event.kind {
            EventKind::ToolCompleted {
                call_id, result, ..
            } => console.finish_tool_call(
                event.run_id.into(),
                Some((
                    &tool_call_key(event.run_id.into(), call_id),
                    tool_result_state(
                        self.started_call(event, call_id).0,
                        &result.content,
                        result.is_error,
                    ),
                )),
            )?,
            EventKind::Completed { .. } | EventKind::Failed(_) | EventKind::Interrupted => {
                console.finish_tool_call(event.run_id.into(), None)?
            }
            EventKind::AssistantMessageCommitted { tool_calls, .. } if console.is_interactive() => {
                let _frame_hold = console.hold_frames()?;
                for call in tool_calls {
                    if has_no_call_row(&call.name) {
                        continue;
                    }
                    let key = tool_call_key(event.run_id.into(), &call.call_id);
                    console.for_message(Some(key.clone())).write_tool_call(
                        event.run_id.into(),
                        &key,
                        &tool_call_roles(&call.name, &call.input, &cwd, display, &self.tick_labels),
                        ToolCallState::Queued,
                        self.collapse(&call.name, &call.input),
                    )?;
                }
            }
            _ => {}
        }
        if let EventKind::ToolCompleted {
            call_id, result, ..
        } = &event.kind
            && !self.detailed
        {
            let (name, input) = self.started_call(event, call_id);
            if name == ASK_USER_QUESTION_TOOL_NAME {
                if let Some((heading, response)) = question_result_runs(
                    input,
                    result,
                    display.glyph(Glyph::ToolCall),
                    display.glyph(Glyph::ToolResult),
                ) {
                    console.write_roles(&heading)?;
                    console.write_response(&response)?;
                }
                return Ok(true);
            }
        }
        let Some(rendered) = self.render_event_roles(event, display, &cwd, facts) else {
            return Ok(false);
        };
        match &event.kind {
            EventKind::ToolStarted {
                call_id,
                name,
                input,
            } if console.is_interactive() && !has_no_call_row(name) => {
                let key = tool_call_key(event.run_id.into(), call_id);
                let collapse = match self.nested.get(&key) {
                    Some(nested) => {
                        console.nest_tool_call(&key, &nested.parent_key)?;
                        None
                    }
                    None => self.collapse(name, input),
                };
                console.write_tool_call(
                    event.run_id.into(),
                    &key,
                    &tool_call_roles(name, input, &cwd, display, &self.tick_labels),
                    ToolCallState::Running,
                    collapse,
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
            EventKind::ToolCompleted {
                call_id, result, ..
            } if !self.detailed => {
                let key = tool_call_key(event.run_id.into(), call_id);
                let state = tool_result_state(
                    self.started_call(event, call_id).0,
                    &result.content,
                    result.is_error,
                );
                match self.started_call(event, call_id) {
                    // A failed TodoWrite shows its row.
                    (name, Some(input))
                        if result.is_error && has_no_call_row(name) && console.is_interactive() =>
                    {
                        console.write_tool_call(
                            event.run_id.into(),
                            &key,
                            &tool_call_roles(name, input, &cwd, display, &self.tick_labels),
                            state,
                            None,
                        )?;
                    }
                    (SUBAGENT_TOOL_NAME, Some(input)) if console.is_interactive() => {
                        let launch = serde_json::from_str::<Value>(&result.content).ok();
                        console.write_tool_call(
                            event.run_id.into(),
                            &key,
                            &agent_messaging::subagent_call_runs(
                                input,
                                launch.as_ref(),
                                &console.agent_colors()?,
                                display,
                            ),
                            state,
                            None,
                        )?;
                    }
                    _ => {}
                }
                console.write_approved_tool_result(
                    &key,
                    &rendered,
                    state == ToolCallState::Warning,
                    self.approvals.get(event.run_id.into(), call_id),
                )?
            }
            _ => console.write_roles(&rendered)?,
        }
        if let EventKind::ToolCompleted {
            call_id, result, ..
        } = &event.kind
            && !result.is_error
            && let (name, Some(input)) = self.started_call(event, call_id)
            && let Some(instance) =
                self.tick_labels
                    .native_tool_instance(name, input)
                    .or_else(|| {
                        (matches!(name, "TickDescribe" | "TickRead" | "TickCall")
                            || (name == "TickManage" && input["action"] == "send_input"))
                            .then(|| serde_json::from_value(input["tick_instance_id"].clone()).ok())
                            .flatten()
                    })
        {
            console.flash_tick_use(&tool_call_key(event.run_id.into(), call_id), instance)?;
        }
        if let EventKind::ToolCompleted { call_id, .. } = &event.kind {
            self.present_parent_status(console, event, call_id, display)?;
        }
        Ok(true)
    }

    fn present_parent_status(
        &self,
        console: &Console,
        event: &Event,
        call_id: &str,
        display: DisplayProfile,
    ) -> Result<(), String> {
        let key = tool_call_key(event.run_id.into(), call_id);
        if let Some(nested) = self.nested.get(&key)
            && let Some(summary) = self.batch_summary(event, &nested.parent)
        {
            console.write_tool_result(
                &nested.parent_key,
                &result_runs(&summary, display.glyph(Glyph::ToolResult)),
                false,
            )?;
        }
        Ok(())
    }

    fn batch_summary(&self, event: &Event, parent: &str) -> Option<String> {
        let parent_key = tool_call_key(event.run_id.into(), parent);
        let mut calls = self
            .nested
            .iter()
            .filter(|(_, nested)| nested.parent_key == parent_key)
            .collect::<Vec<_>>();
        calls.sort_by_key(|(_, nested)| nested.ordinal);
        let total = self
            .started_call(event, parent)
            .1
            .and_then(|input| input["calls"].as_array())
            .map_or(calls.len(), Vec::len);
        batch_summary(
            calls.iter().map(|(key, nested)| {
                let call_id = key.split_once(":tool:").map_or("", |(_, call)| call);
                (
                    nested.ordinal,
                    self.approvals.get(event.run_id.into(), call_id),
                    nested.result.as_ref(),
                )
            }),
            total,
        )
    }

    /// How a call joins a collapsed group; detailed output shows every call on its own.
    fn collapse(&self, name: &str, input: &Value) -> Option<CollapsedCall> {
        (!self.detailed && self.tick_labels.native_tool_instance(name, input).is_none())
            .then(|| collapsed_call(name, input))
            .flatten()
    }

    /// Whether the latest result of `run` reports that the user declined the call, so the run's
    /// interruption needs no line of its own.
    pub(super) fn ended_with_rejection(&self, run: Uuid) -> bool {
        self.events
            .iter()
            .rev()
            .find_map(|event| match &event.kind {
                EventKind::ToolCompleted { result, .. } if event.run_id == run => {
                    Some(is_rejection(result))
                }
                _ => None,
            })
            .unwrap_or(false)
    }

    /// Where the result `event` brings finds what it shows beyond itself: an edit's recorded
    /// change or the report a Subagent's child delivered.
    pub(super) fn result_source(&self, event: &Event) -> ResultSource {
        match &event.kind {
            EventKind::ToolCompleted {
                call_id, result, ..
            } => match self.started_call(event, call_id).0 {
                EDIT_TOOL_NAME if !result.is_error => ResultSource::Operation,
                SUBAGENT_TOOL_NAME => ResultSource::Conversation,
                _ => ResultSource::Itself,
            },
            _ => ResultSource::Itself,
        }
    }

    /// What the result `event` brings shows from `snapshot`, its conversation.
    pub(super) fn call_facts(&self, event: &Event, snapshot: &ChatSnapshot) -> CallFacts {
        let EventKind::ToolCompleted {
            call_id, result, ..
        } = &event.kind
        else {
            return CallFacts::default();
        };
        match self.started_call(event, call_id).0 {
            EDIT_TOOL_NAME if !result.is_error => CallFacts {
                review: file_change_review(snapshot, event.run_id.into(), call_id),
                report: None,
            },
            SUBAGENT_TOOL_NAME => CallFacts {
                review: None,
                report: serde_json::from_str::<Value>(&result.content)
                    .ok()
                    .and_then(|launch| agent_messaging::subagent_report(snapshot, &launch)),
            },
            _ => CallFacts::default(),
        }
    }

    pub(super) fn new(detailed: bool) -> Self {
        Self {
            detailed,
            events: VecDeque::new(),
            tick_labels: super::tick_labels::TickLabels::default(),
            nested: HashMap::new(),
            approvals: ToolApprovals::default(),
        }
    }

    pub(super) fn is_detailed(&self) -> bool {
        self.detailed
    }

    /// The call of `event` whose parent this presentation has yet to learn: a call no assistant
    /// message announced while a batch of the same Run was announced.
    pub(super) fn nested_call_to_learn<'a>(&self, event: &'a Event) -> Option<&'a str> {
        let call_id = match &event.kind {
            EventKind::ApprovalReviewStarted { call_id, .. } => call_id.as_str(),
            EventKind::ApprovalRecorded { record } => record.call_id.as_str(),
            EventKind::PermissionRequested { request } => match &request.execution {
                tickloop_harness::protocol::PermissionExecution::ToolCall { call_id, .. } => {
                    call_id.as_str()
                }
                _ => return None,
            },
            EventKind::ToolStarted { call_id, .. } | EventKind::ToolCompleted { call_id, .. } => {
                call_id.as_str()
            }
            _ => return None,
        };
        if self
            .nested
            .contains_key(&tool_call_key(event.run_id.into(), call_id))
        {
            return matches!(event.kind, EventKind::ToolCompleted { .. }).then_some(call_id);
        }
        let mut makes_nested = false;
        for recorded in &self.events {
            if let EventKind::AssistantMessageCommitted { tool_calls, .. } = &recorded.kind
                && recorded.run_id == event.run_id
            {
                if tool_calls.iter().any(|call| call.call_id == call_id) {
                    return None;
                }
                makes_nested |= tool_calls.iter().any(|call| makes_nested_calls(&call.name));
            }
        }
        makes_nested.then_some(call_id)
    }

    fn was_started(&self, event: &Event, call_id: &str) -> bool {
        self.events.iter().any(|recorded| {
            recorded.run_id == event.run_id
                && matches!(&recorded.kind, EventKind::ToolStarted { call_id: started, .. } if started == call_id)
        })
    }

    /// Learns from the recorded operation of a call whether a batch made it.
    pub(super) fn learn_nested_call(&mut self, event: &Event, operation: &OperationRecord) {
        let Some(parent) = &operation.parent_call_id else {
            return;
        };
        let key = tool_call_key(event.run_id.into(), &operation.call_id);
        let ordinal = operation.parent_call_index.unwrap_or_else(|| {
            self.nested
                .get(&key)
                .map_or(self.nested.len(), |nested| nested.ordinal)
        });
        self.nested.insert(
            key,
            NestedCall {
                parent_key: tool_call_key(event.run_id.into(), parent),
                parent: parent.clone(),
                ordinal,
                result: operation.result.clone(),
                name: operation.name.clone(),
                input: operation.input.clone(),
            },
        );
    }

    pub(super) fn restore_snapshot(&mut self, snapshot: &ChatSnapshot) {
        self.restore(&snapshot.conversation_events());
        self.approvals = ToolApprovals::from_snapshot(snapshot);
        for operation in &snapshot.operations {
            let event = Event {
                sequence: 0,
                chat_id: snapshot.id,
                loop_id: operation.loop_id,
                run_id: operation.run_id,
                kind: EventKind::Started,
            };
            self.learn_nested_call(&event, operation);
        }
    }

    pub(super) fn restore(&mut self, events: &[Event]) {
        self.events.clear();
        self.nested.clear();
        self.approvals = ToolApprovals::default();
        for event in events {
            self.record(event);
        }
    }

    pub(super) fn record(&mut self, event: &Event) {
        self.approvals.record(event);
        if let EventKind::ToolCompleted {
            call_id, result, ..
        } = &event.kind
            && let Some(nested) = self
                .nested
                .get_mut(&tool_call_key(event.run_id.into(), call_id))
        {
            nested.result = Some(result.clone());
        }
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

    /// Whether `event` shows nothing: the start of a call without a row of its own, or a
    /// successful TodoWrite or SendMessage, which shows apart from this conversation.
    fn shows_nothing(&self, event: &Event) -> bool {
        match &event.kind {
            EventKind::ToolStarted { name, .. } => has_no_call_row(name),
            EventKind::ToolCompleted {
                call_id, result, ..
            } => !result.is_error && shows_only_failure(self.started_call(event, call_id).0),
            _ => false,
        }
    }

    pub(super) fn render_event(&self, event: &Event) -> Option<String> {
        if self.shows_nothing(event) {
            return Some(String::new());
        }
        if self.detailed {
            let name = match &event.kind {
                EventKind::ToolCompleted { call_id, .. } => self.started_call(event, call_id).0,
                _ => "tool",
            };
            let detail = detailed_event(event, name)?;
            if let EventKind::ToolCompleted { call_id, .. } = &event.kind
                && let Some(approval) = self.approvals.get(event.run_id.into(), call_id)
            {
                let approval_lines = approval.decorate(Vec::new(), "↳");
                if !approval_lines.is_empty() {
                    return Some(detail + &joined(&approval_lines));
                }
            }
            return Some(detail);
        }
        // A background command's end reaches the conversation as its task notification.
        match &event.kind {
            EventKind::ToolStarted { name, input, .. } => {
                let summary = tool_summary(name, input, Path::new(""));
                Some(format!("\n● {}{summary}\n", tool_name(name)))
            }
            EventKind::ToolCompleted {
                call_id, result, ..
            } => {
                let (name, input) = self.started_call(event, call_id);
                if name == ASK_USER_QUESTION_TOOL_NAME {
                    return Some(
                        question_result_runs(input, result, "●", "↳")
                            .map(|(heading, response)| joined(&heading) + &joined(&response))
                            .unwrap_or_default(),
                    );
                }
                Some(joined(&compact_result_runs(
                    name,
                    input,
                    result,
                    "↳",
                    Path::new(""),
                    &CallFacts::default(),
                )))
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
        cwd: &Path,
        facts: &CallFacts,
    ) -> Option<Vec<(String, Option<Role>)>> {
        if self.shows_nothing(event) {
            return Some(Vec::new());
        }
        if self.detailed {
            return self
                .render_event(event)
                .map(|rendered| vec![(rendered, None)]);
        }
        Some(match &event.kind {
            EventKind::ToolStarted { name, input, .. } => {
                tool_call_roles(name, input, cwd, display, &self.tick_labels)
            }
            EventKind::ToolCompleted {
                call_id, result, ..
            } => {
                let (name, input) = self.started_call(event, call_id);
                let runs = if name == EXECUTE_TOOLS_NAME {
                    settled_batch_summary(&result.content, &self.approvals, event.run_id.into())
                        .or_else(|| self.batch_summary(event, call_id))
                        .map(|summary| result_runs(&summary, display.glyph(Glyph::ToolResult)))
                } else {
                    None
                }
                .unwrap_or_else(|| {
                    compact_result_runs(
                        name,
                        input,
                        result,
                        display.glyph(Glyph::ToolResult),
                        cwd,
                        facts,
                    )
                });
                match self.approvals.get(event.run_id.into(), call_id) {
                    Some(approval) => approval.decorate(runs, display.glyph(Glyph::ToolResult)),
                    None => runs,
                }
            }
            _ => vec![(self.render_event(event)?, Some(Role::MutedText))],
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
            .or_else(|| {
                self.nested
                    .get(&tool_call_key(event.run_id.into(), call_id))
                    .map(|nested| (nested.name.as_str(), Some(&nested.input)))
            })
            .unwrap_or(("tool", None))
    }

    pub(super) fn toggle(&mut self) -> String {
        self.detailed = !self.detailed;
        if !self.detailed {
            return "\nCompact tool output enabled. ctrl+o shows details.\n".into();
        }
        let mut text =
            "\nDetailed tool output enabled. ctrl+o returns to compact output.\n".to_owned();
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

fn approval_event_call(event: &Event) -> Option<&str> {
    match &event.kind {
        EventKind::ApprovalReviewStarted { call_id, .. } => Some(call_id),
        EventKind::ApprovalRecorded { record } => Some(&record.call_id),
        EventKind::PermissionRequested { request } => match &request.execution {
            tickloop_harness::protocol::PermissionExecution::ToolCall { call_id, .. } => {
                Some(call_id)
            }
            _ => None,
        },
        _ => None,
    }
}

fn batch_summary<'a>(
    calls: impl Iterator<Item = (usize, Option<&'a ApprovalStatus>, Option<&'a ToolResult>)>,
    total: usize,
) -> Option<String> {
    let mut completed = 0;
    let mut denied = 0;
    let mut failed = 0;
    let mut approved = 0;
    let mut seen = 0;
    for (index, approval, result) in calls {
        seen += 1;
        if let Some(label) = approval.and_then(ApprovalStatus::pending_label) {
            return Some(format!(
                "{}: call {} of {total}",
                label.trim_end_matches('…'),
                index + 1
            ));
        }
        if approval.is_some_and(ApprovalStatus::auto_approved) {
            approved += 1;
        }
        if approval.is_some_and(ApprovalStatus::denied) {
            denied += 1;
        } else if let Some(result) = result {
            if result.is_error {
                failed += 1;
            } else {
                completed += 1;
            }
        }
    }
    if seen == 0 {
        return None;
    }
    let mut summary = format!("{completed} completed");
    if denied > 0 {
        summary.push_str(&format!(", {denied} denied"));
    }
    if failed > 0 {
        summary.push_str(&format!(", {failed} failed"));
    }
    if approved > 0 {
        summary.push_str(&format!(" · {approved} auto-approved"));
    }
    Some(summary)
}

pub(super) fn settled_batch_summary(
    content: &str,
    approvals: &ToolApprovals,
    run: Uuid,
) -> Option<String> {
    let value: Value = serde_json::from_str(content).ok()?;
    let results = value["results"].as_array()?;
    let mut counts = [0; 5];
    let mut approved = 0;
    for result in results {
        let index = match result["status"].as_str()? {
            "completed" => 0,
            "denied" => 1,
            "error" => 2,
            "not_executed" => 3,
            "unknown" => 4,
            _ => return None,
        };
        counts[index] += 1;
        if result["call_id"]
            .as_str()
            .and_then(|call| approvals.get(run, call))
            .is_some_and(ApprovalStatus::auto_approved)
        {
            approved += 1;
        }
    }
    let mut summary = format!("{} completed", counts[0]);
    for (count, label) in counts[1..]
        .iter()
        .zip(["denied", "failed", "not executed", "unknown"])
    {
        if *count > 0 {
            summary.push_str(&format!(", {count} {label}"));
        }
    }
    if approved > 0 {
        summary.push_str(&format!(" · {approved} auto-approved"));
    }
    Some(summary)
}

pub(super) fn snapshot_batch_summary(
    snapshot: &ChatSnapshot,
    run: Uuid,
    call: &str,
    input: Option<&Value>,
) -> Option<String> {
    let approvals = ToolApprovals::from_snapshot(snapshot);
    let operations = nested_operations(snapshot, run, call).collect::<Vec<_>>();
    let total = input
        .and_then(|input| input["calls"].as_array())
        .map_or(operations.len(), Vec::len);
    batch_summary(
        operations.iter().enumerate().map(|(index, operation)| {
            (
                operation.parent_call_index.unwrap_or(index),
                approvals.get(run, &operation.call_id),
                operation.result.as_ref(),
            )
        }),
        total,
    )
}

pub(super) fn tool_call_key(run: Uuid, call_id: &str) -> String {
    format!("{run}:tool:{call_id}")
}

pub(super) fn tool_call_roles(
    name: &str,
    input: &Value,
    cwd: &Path,
    display: DisplayProfile,
    labels: &super::tick_labels::TickLabels,
) -> Vec<(String, Option<Role>)> {
    if let Some(runs) = labels.tool_runs(name, input, display) {
        return runs;
    }
    if name == SUBAGENT_TOOL_NAME {
        return agent_messaging::subagent_call_runs(input, None, &AgentColors::default(), display);
    }
    vec![
        ("\n".into(), None),
        (
            format!("{} ", display.glyph(Glyph::ToolCall)),
            Some(Role::MutedText),
        ),
        (super::safe_line(&tool_name(name)), Some(Role::ToolName)),
        (super::safe_line(&tool_summary(name, input, cwd)), None),
        ("\n".into(), None),
    ]
}

/// How a read or search call counts in a collapsed group; other calls show their own row.
pub(super) fn collapsed_call(name: &str, input: &Value) -> Option<CollapsedCall> {
    match name {
        READ_TOOL_NAME => Some(CollapsedCall::Read {
            path: input["file_path"].as_str().unwrap_or_default().to_owned(),
        }),
        "Glob" | "Grep" => Some(CollapsedCall::Search),
        _ => None,
    }
}

/// A path as the conversation names it: relative to the working directory when inside it.
fn display_path(path: &str, cwd: &Path) -> String {
    match Path::new(path).strip_prefix(cwd) {
        Ok(relative) if relative.as_os_str().is_empty() => ".".into(),
        Ok(relative) => relative.display().to_string(),
        Err(_) => path.to_owned(),
    }
}

/// The change an edit made, as the review recorded when the call was prepared.
pub(super) fn file_change_review(
    snapshot: &ChatSnapshot,
    run: Uuid,
    call_id: &str,
) -> Option<FileChangeReview> {
    operation_review(
        snapshot
            .operations
            .iter()
            .rev()
            .find(|operation| operation.run_id == run && operation.call_id == call_id)?,
    )
}

/// The change an edit's operation made, as its review recorded it.
pub(super) fn operation_review(operation: &OperationRecord) -> Option<FileChangeReview> {
    load_file_change_review(&operation.file_change.as_ref()?.artifact).ok()
}

pub(super) fn tool_call_states(snapshot: &ChatSnapshot) -> HashMap<(Uuid, String), ToolCallState> {
    use tickloop_harness::protocol::Content;
    use tickloop_harness::protocol::{LoopState, OperationState};
    let chat_loop = &snapshot.chat_loop;
    let active_run = matches!(
        chat_loop.state,
        LoopState::Running
            | LoopState::Queued
            | LoopState::WaitingPermission
            | LoopState::WaitingQuestion
            | LoopState::WaitingPlan
    )
    .then_some(chat_loop.run_id)
    .flatten();
    let mut states = HashMap::new();
    let mut names = HashMap::new();
    for operation in &snapshot.operations {
        let state = match operation.state {
            OperationState::Proposed if active_run == Some(operation.run_id) => {
                ToolCallState::Queued
            }
            OperationState::Proposed => ToolCallState::Stopped,
            OperationState::Admitted => ToolCallState::Running,
            _ => ToolCallState::Finished,
        };
        states.insert((operation.run_id.uuid(), operation.call_id.clone()), state);
    }
    for (index, message) in snapshot.messages.iter().enumerate() {
        let Some(run) = snapshot.message_runs.get(&index).copied() else {
            continue;
        };
        for content in &message.content {
            match content {
                Content::ToolUse { id, name, .. } => {
                    names.insert((run, id.clone()), name.clone());
                    states
                        .entry((run.into(), id.clone()))
                        .or_insert(if active_run == Some(run) {
                            ToolCallState::Queued
                        } else {
                            ToolCallState::Finished
                        });
                }
                Content::ToolResult {
                    tool_use_id,
                    is_error,
                    content,
                    ..
                } => {
                    let finished = tool_result_state(
                        names
                            .get(&(run, tool_use_id.clone()))
                            .map_or("", String::as_str),
                        &content.text(),
                        *is_error,
                    );
                    states
                        .entry((run.into(), tool_use_id.clone()))
                        .and_modify(|state| {
                            if *state != ToolCallState::Stopped {
                                *state = finished;
                            }
                        })
                        .or_insert(finished);
                }
                _ => {}
            }
        }
    }
    for recovery in &snapshot.tool_call_recoveries {
        if let Some(run) = recovery.run_id {
            states.insert(
                (run.into(), recovery.call_id.clone()),
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
                    states.insert(
                        (event.run_id.into(), call.call_id.clone()),
                        ToolCallState::Queued,
                    );
                }
            }
            EventKind::ToolStarted { call_id, name, .. } => {
                names.insert((event.run_id, call_id.clone()), name.clone());
                states.insert(
                    (event.run_id.into(), call_id.clone()),
                    ToolCallState::Running,
                );
            }
            EventKind::ToolCompleted {
                call_id, result, ..
            } => {
                states.insert(
                    (event.run_id.into(), call_id.clone()),
                    tool_result_state(
                        names
                            .get(&(event.run_id, call_id.clone()))
                            .map_or("", String::as_str),
                        &result.content,
                        result.is_error,
                    ),
                );
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

/// Where a nested call came from, in words rather than the parent's internal call id.
fn nested_call_origin(request: &PermissionRequest, parent: &ProposedToolCall) -> String {
    match parent.name.as_str() {
        EXECUTE_TOOLS_NAME => {
            let calls = parent.input["calls"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default();
            let mut positions = calls.iter().enumerate().filter(|(_, call)| {
                call["name"] == request.name.as_str() && call["arguments"] == request.input
            });
            match (positions.next(), positions.next()) {
                (Some((index, _)), None) => {
                    format!("ExecuteTools, call {} of {}", index + 1, calls.len())
                }
                _ => format!("an ExecuteTools batch of {} calls", calls.len()),
            }
        }
        other => safe_text(other),
    }
}

pub(super) fn permission_context(request: &PermissionRequest) -> Vec<String> {
    let mut lines = vec![format!(
        "{} is waiting for your approval.",
        tool_name(&request.name)
    )];
    if let Some(parent) = &request.parent_call {
        lines.push(format!(
            "From {} (model-authored; not user authorization)",
            nested_call_origin(request, parent)
        ));
        lines.push(format!(
            "Full parent call: {}",
            safe_text(&parent.input.to_string())
        ));
    }
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
        EventKind::ToolCompleted {
            call_id, result, ..
        } => {
            let content = if result.is_error {
                web_search_failure_lines(name, &result.content).map(|lines| {
                    lines
                        .into_iter()
                        .map(|line| joined(&line.spans))
                        .collect::<Vec<_>>()
                        .join("\n")
                })
            } else {
                None
            };
            let mut text = format!(
                "\nTool {} · {}\n{}\n",
                safe_text(call_id),
                tool_result_status(name, result),
                content.unwrap_or_else(|| safe_text(&result.content)),
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
                "\nCommand: {} · {:?} · PID {:?}\n{}\nDirectory: {}\nExit code: {:?} · Signal: {:?} · Output bytes: {}\nFull output: {}\n",
                command.operation_id,
                command.state,
                command.pid,
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

fn tool_summary(name: &str, input: &Value, cwd: &Path) -> String {
    let asked_questions;
    let quickstart;
    let described_tools;
    let batched_calls;
    let primary = match name {
        "ArtifactRead" if input["action"] == "quickstart" => {
            quickstart = format!(
                "quickstart {}",
                input["intent"].as_str().unwrap_or_default()
            );
            Some(quickstart.as_str())
        }
        SEARCH_TOOLS_NAME => input["query"].as_str(),
        DESCRIBE_TOOLS_NAME => {
            described_tools = input["names"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ");
            Some(described_tools.as_str()).filter(|names| !names.is_empty())
        }
        EXECUTE_TOOLS_NAME => {
            let calls = input["calls"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default();
            let mut names: Vec<&str> = Vec::new();
            for name in calls.iter().filter_map(|call| call["name"].as_str()) {
                if !names.contains(&name) {
                    names.push(name);
                }
            }
            batched_calls = match calls.len() {
                0 | 1 => names.join(", "),
                count => format!("{count} calls: {}", names.join(", ")),
            };
            Some(batched_calls.as_str()).filter(|calls| !calls.is_empty())
        }
        "Skill" => input["skill"].as_str(),
        "ReadMcpResourceTool" => input["uri"].as_str(),
        "ListMcpResourcesTool" => input["server"].as_str(),
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
        "Read" | "Write" | "Edit" => input["file_path"].as_str(),
        _ => input["path"].as_str(),
    };
    let Some(primary) = primary else {
        return String::new();
    };
    let summary = match name {
        "Glob" | "Grep" => format!(
            "{primary} in {}",
            input["path"]
                .as_str()
                .map_or_else(|| ".".into(), |path| display_path(path, cwd))
        ),
        "Read" | "Write" | "Edit" => display_path(primary, cwd),
        _ if input["path"].as_str() == Some(primary) => display_path(primary, cwd),
        _ => primary.to_owned(),
    };
    format!(
        " · {}",
        clipped(&summary.replace('\n', " ↵ "), TOOL_SUMMARY_CHARACTERS).0
    )
}

pub(super) fn archived_tool_call(
    name: &str,
    input: &Value,
    cwd: &Path,
    display: DisplayProfile,
    labels: &super::tick_labels::TickLabels,
) -> String {
    joined(&tool_call_roles(name, input, cwd, display, labels))
}

pub(super) fn archived_tool_result(
    name: &str,
    input: Option<&Value>,
    result: &ToolResult,
    cwd: &Path,
    display: DisplayProfile,
    facts: &CallFacts,
) -> RoleRuns {
    compact_result_runs(
        name,
        input,
        result,
        display.glyph(Glyph::ToolResult),
        cwd,
        facts,
    )
}

/// Conversation rendering of local messages, following Claude Code: `!` commands, `⎿` output
/// blocks, interruption markers and task notifications.
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
    let (heading, response) = local_message_runs(message, detail, display);
    joined(&heading) + &joined(&response)
}

/// A local message as the conversation styles it, as Claude Code draws each kind: a `!` command
/// in the shell colours, its output in the terminal's colour with errors in the error colour, a
/// background task's `⏺` in its outcome's colour, and the rest muted. The heading comes first,
/// then the `⎿` response rows under it; an Agent message's body is indented under its heading.
pub(super) fn local_message_runs(
    message: &LocalMessage,
    detail: OutputDetail,
    display: DisplayProfile,
) -> (RoleRuns, RoleRuns) {
    let mut runs = RoleRuns::new();
    let mut response = RoleRuns::new();
    match message {
        LocalMessage::AgentCommunication {
            sender_handle,
            relay,
            message,
            ..
        } => {
            return agent_messaging::message_runs(
                &AgentMessage {
                    sender_handle,
                    relay: relay.as_ref(),
                    text: message,
                },
                detail,
                display,
            );
        }
        LocalMessage::BashInput { command } => {
            runs.push(("\n".into(), None));
            runs.push(("! ".into(), Some(Role::ShellBorder)));
            runs.push((format!("{}\n", safe_text(command)), Some(Role::Text)));
        }
        LocalMessage::BashOutput { stdout, stderr } => {
            if matches!(detail, OutputDetail::Preview) && background_launch_id(stdout).is_some() {
                return (runs, backgrounded_shell_runs(display));
            }
            push_response_block(&mut response, stdout.trim_end(), detail, display, None);
            push_response_block(
                &mut response,
                stderr.trim_end(),
                detail,
                display,
                Some(Role::Error),
            );
            if response.is_empty() {
                push_response_block(
                    &mut response,
                    NO_OUTPUT,
                    detail,
                    display,
                    Some(Role::MutedText),
                );
            }
        }
        LocalMessage::Interrupted { .. } => response.push((
            format!("{}\n", interrupted_notice(display)),
            Some(Role::MutedText),
        )),
        LocalMessage::TaskNotification {
            status,
            summary,
            result,
            usage,
        } => {
            runs.push(("\n".into(), None));
            runs.push((
                display.glyph(Glyph::ToolCall).into(),
                Some(match status {
                    Some(TaskNotificationStatus::Completed) => Role::Success,
                    Some(TaskNotificationStatus::Failed) => Role::Error,
                    Some(TaskNotificationStatus::Killed) => Role::Warning,
                    None => Role::Text,
                }),
            ));
            runs.push((format!(" {}\n", safe_text(summary)), None));
            if matches!(detail, OutputDetail::Full) {
                if let Some(result) = result {
                    push_response_block(&mut response, result, detail, display, None);
                }
                if let Some(usage) = usage {
                    response.push((
                        format!("Usage{}\n", super::usage_text(usage)),
                        Some(Role::MutedText),
                    ));
                }
            }
        }
        LocalMessage::TickDelivery { items } => {
            runs.extend(tick_delivery_runs(items, detail, display))
        }
        LocalMessage::ArtifactRequest {
            artifact_id,
            request_id,
            title,
            body,
        } => {
            runs.push((
                format!("\n{} ", display.glyph(Glyph::ArtifactTick)),
                Some(Role::MutedText),
            ));
            runs.push(("@tickloop request: ".into(), Some(Role::MutedStrong)));
            runs.push((
                format!(
                    "{} · {}\n",
                    super::conversation_picker::ellipsize(
                        &super::safe_line(title.as_deref().unwrap_or(artifact_id)),
                        super::tick_labels::TICK_NAME_COLUMNS,
                    ),
                    super::safe_line(request_id)
                ),
                Some(Role::MutedText),
            ));
            if matches!(detail, OutputDetail::Full) {
                push_response_block(&mut response, body, detail, display, None);
            }
        }
        LocalMessage::GoalSteering { text } => {
            runs.push((
                format!(
                    "\nGoal · {}\n",
                    super::safe_line(text.lines().next().unwrap_or_default())
                ),
                Some(Role::MutedText),
            ));
            if matches!(detail, OutputDetail::Full) {
                push_response_block(&mut response, text, detail, display, None);
            }
        }
    }
    (runs, response)
}

pub(super) fn tick_delivery_runs(
    items: &[TickDeliveryLine],
    detail: OutputDetail,
    display: DisplayProfile,
) -> RoleRuns {
    super::tick_labels::delivery_runs(items, detail, display)
}

pub(super) fn name_local_ticks(
    mut message: LocalMessage,
    labels: &super::tick_labels::TickLabels,
) -> LocalMessage {
    if let LocalMessage::TickDelivery { items } = &mut message {
        for item in items {
            labels.name_delivery(item);
        }
    }
    if let LocalMessage::ArtifactRequest {
        artifact_id, title, ..
    } = &mut message
    {
        *title = labels.artifact_title(artifact_id);
    }
    message
}

/// A finished `!` command as the conversation keeps it: the same local message the Agent records
/// from its record and `output`.
pub(super) fn user_shell_result(record: &UserShellRecord, output: &str) -> LocalMessage {
    if record.reason.as_deref() == Some(tickloop_harness::tools::CANCELLED_COMMAND_REASON) {
        return LocalMessage::Interrupted {
            during_tool_use: false,
        };
    }
    let output = output.trim_end().to_owned();
    if record.state == UserShellState::Completed {
        return LocalMessage::BashOutput {
            stdout: output,
            stderr: String::new(),
        };
    }
    let failure = match (record.exit_code, &record.reason) {
        (Some(code), _) => Some(format!("Exit code {code}")),
        (None, Some(reason)) => Some(reason.clone()),
        (None, None) => record.signal.map(|signal| format!("Signal {signal}")),
    };
    let stderr = [Some(output), failure]
        .into_iter()
        .flatten()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    LocalMessage::BashOutput {
        stdout: String::new(),
        stderr,
    }
}

/// A running `!` command's latest output lines under a `⎿`, as Claude Code's shell progress
/// shows them, or that it is running before it prints anything.
pub(super) fn running_shell_runs(output: &str, display: DisplayProfile) -> RoleRuns {
    let mut rows = ResponseRows::new(display.glyph(Glyph::ToolResult));
    let output = safe_text(output);
    let lines = output.lines().collect::<Vec<_>>();
    if lines.is_empty() {
        rows.row([("Running…".into(), Some(Role::MutedText))]);
    }
    for line in &lines[lines.len().saturating_sub(PREVIEW_LINES)..] {
        rows.row([(clipped(line, PREVIEW_LINE_CHARACTERS).0, None)]);
    }
    rows.runs
}

/// The label a finished thinking block leaves in the conversation; Ctrl+O shows the thinking.
pub(super) fn thinking_label() -> super::console::ConversationPiece {
    super::console::ConversationPiece::Aside(
        "∴ Thinking (ctrl+o to expand)\n".into(),
        Some(Role::ThinkingHeading),
    )
}

pub(super) fn interrupted_notice(display: DisplayProfile) -> String {
    format!("  {}  {INTERRUPTED}", display.glyph(Glyph::ToolResult))
}

/// Rows under a `⎿` connector as Claude Code's `MessageResponse` lays them out: the first row
/// behind the muted connector, every later row under the first one's content.
struct ResponseRows<'a> {
    runs: RoleRuns,
    glyph: &'a str,
}

impl<'a> ResponseRows<'a> {
    fn new(glyph: &'a str) -> Self {
        Self {
            runs: RoleRuns::new(),
            glyph,
        }
    }

    fn row(&mut self, spans: impl IntoIterator<Item = (String, Option<Role>)>) {
        if self.runs.is_empty() {
            push_connector(&mut self.runs, self.glyph);
        } else {
            self.runs.push((RESPONSE_CONTINUATION.into(), None));
        }
        self.runs.extend(spans);
        self.runs.push(("\n".into(), None));
    }

    /// One row per line of `text`.
    fn lines(&mut self, text: &str, role: Option<Role>) {
        for line in text.lines() {
            self.row([(line.to_owned(), role)]);
        }
    }

    /// The first lines of `content`, each cut to a preview's width, then the details hint when
    /// anything was left out. A failure keeps its first lines (the exit code and what ended the
    /// command) and its last lines instead, where a command's error message ends up.
    fn preview(&mut self, content: &str, source_truncated: bool, body: Option<Role>) {
        let sanitized = safe_text(content);
        let all_lines = sanitized.lines().collect::<Vec<_>>();
        if body == Some(Role::Error) && all_lines.len() > PREVIEW_LINES {
            let tail = PREVIEW_LINES - FAILURE_HEAD_LINES;
            let hidden = all_lines.len() - FAILURE_HEAD_LINES - tail;
            for line in &all_lines[..FAILURE_HEAD_LINES] {
                self.row([(clipped(line, PREVIEW_LINE_CHARACTERS).0, body)]);
            }
            self.row([(
                format!("… +{hidden} {} (ctrl+o to expand)", plural(hidden, "line")),
                Some(Role::MutedText),
            )]);
            for line in &all_lines[all_lines.len() - tail..] {
                self.row([(clipped(line, PREVIEW_LINE_CHARACTERS).0, body)]);
            }
            return;
        }
        let mut lines = sanitized.lines();
        let mut truncated = source_truncated;
        for line in lines.by_ref().take(PREVIEW_LINES) {
            let (preview, clipped) = clipped(line, PREVIEW_LINE_CHARACTERS);
            self.row([(preview, body)]);
            truncated |= clipped;
        }
        truncated |= lines.next().is_some();
        if truncated {
            self.row([(DETAILS_HINT.into(), Some(Role::MutedText))]);
        }
    }
}

/// A `⎿` block as Claude Code's `MessageResponse` draws it: the connector and the hint muted,
/// the content in `body`'s role.
fn push_response_block(
    runs: &mut RoleRuns,
    content: &str,
    detail: OutputDetail,
    display: DisplayProfile,
    body: Option<Role>,
) {
    if content.is_empty() {
        return;
    }
    let mut rows = ResponseRows::new(display.glyph(Glyph::ToolResult));
    match detail {
        OutputDetail::Preview => rows.preview(content, false, body),
        OutputDetail::Full => rows.lines(&safe_text(content), body),
    }
    runs.extend(rows.runs);
}

/// `  ⎿  ` with the connector muted, as Claude Code dims it.
fn push_connector(runs: &mut RoleRuns, glyph: &str) {
    runs.push(("  ".into(), None));
    runs.push((glyph.into(), Some(Role::MutedText)));
    runs.push(("  ".into(), None));
}

/// AskUserQuestion's result in place of its call row, as Claude Code's
/// `AskUserQuestionResultMessage` and `renderToolUseRejectedMessage` draw it: a heading behind a
/// `dot` in the text colour, then each answer as a muted `· question → answer` row under the
/// `connector`. `None` for a failure: an interruption that the run's notice shows, or a call the
/// Agent makes again; only the Ctrl+O transcript lists it.
pub(super) fn question_result_runs(
    input: Option<&Value>,
    result: &ToolResult,
    dot: &str,
    connector: &str,
) -> Option<(RoleRuns, RoleRuns)> {
    if result.is_error && !is_rejection(result) {
        return None;
    }
    let heading = if result.is_error {
        DECLINED_QUESTIONS
    } else {
        ANSWERED_QUESTIONS
    };
    let mut rows = ResponseRows::new(connector);
    if !result.is_error {
        match input.and_then(|input| answered_question_lines(input, &result.content)) {
            Some(answers) => {
                for answer in answers {
                    rows.row([(answer, Some(Role::MutedText))]);
                }
            }
            None => rows.lines(&safe_text(&result.content), Some(Role::MutedText)),
        }
    }
    Some((
        vec![
            ("\n".into(), None),
            (dot.into(), Some(Role::Text)),
            (format!(" {heading}\n"), None),
        ],
        rows.runs,
    ))
}

/// The answers an `AskUserQuestion` result reports, one `· question → answer` row each as
/// Claude Code lists them. The result names every asked question in order as
/// `"question"="answer"`, and an answer that chose a previewed option adds the preview.
fn answered_question_lines(input: &Value, content: &str) -> Option<Vec<String>> {
    let questions = input["questions"]
        .as_array()?
        .iter()
        .map(|question| question["question"].as_str())
        .collect::<Option<Vec<_>>>()?;
    let mut rest = content
        .strip_prefix(ANSWERED_QUESTIONS_PREFIX)?
        .strip_suffix(ANSWERED_QUESTIONS_SUFFIX)?;
    let mut lines = Vec::new();
    for (index, question) in questions.iter().enumerate() {
        rest = rest.strip_prefix(&format!("\"{question}\"=\""))?;
        let end = match questions.get(index + 1) {
            Some(next) => rest.find(&format!(", \"{next}\"=\""))?,
            None => rest.len(),
        };
        let (answer, remainder) = rest.split_at(end);
        let answer = match answer.split_once(ANSWER_PREVIEW_MARKER) {
            Some((answer, _)) => answer,
            None => answer.strip_suffix('"')?,
        };
        lines.push(format!(
            "· {} → {}",
            super::safe_line(question),
            super::safe_line(answer)
        ));
        rest = remainder.strip_prefix(", ").unwrap_or(remainder);
    }
    rest.is_empty().then_some(lines)
}

/// Whether a failed result only reports that the user declined the call.
pub(super) fn is_rejection(result: &ToolResult) -> bool {
    result.is_error
        && REJECTION_PREFIXES
            .iter()
            .any(|prefix| result.content.starts_with(prefix))
}

#[cfg(test)]
fn compact_result(name: &str, input: Option<&Value>, result: &ToolResult) -> String {
    joined(&compact_result_runs(
        name,
        input,
        result,
        "↳",
        Path::new(""),
        &CallFacts::default(),
    ))
}

/// A tool result as the conversation shows it, under a `⎿` connector: a declined call as one
/// muted line, a command's output from its first row, a file change as its summary and lines,
/// and any other result as its status and details. Failures take the error colour.
fn compact_result_runs(
    name: &str,
    input: Option<&Value>,
    result: &ToolResult,
    glyph: &str,
    cwd: &Path,
    facts: &CallFacts,
) -> RoleRuns {
    let mut rows = ResponseRows::new(glyph);
    let failure = |failed: bool| if failed { Role::Error } else { Role::MutedText };
    let body = result.is_error.then_some(Role::Error);
    if let Some(reason) = goal_completion_rejection_reason(name, &result.content, result.is_error) {
        let reason = super::safe_line(reason);
        let end = reason
            .char_indices()
            .find(|(index, character)| {
                matches!(character, '。' | '！' | '？')
                    || *character == '.' && reason[index + 1..].starts_with(char::is_whitespace)
            })
            .map_or(reason.len(), |(index, character)| {
                index + character.len_utf8()
            });
        rows.row([("Not complete yet".into(), Some(Role::Warning))]);
        rows.row([(format!("Reason: {}", &reason[..end]), Some(Role::MutedText))]);
        return rows.runs;
    }
    if is_rejection(result) {
        match input.and_then(|input| rejected_file_change(name, input, cwd)) {
            Some((operation, path)) => rows.row([
                (
                    format!("User rejected {operation} to "),
                    Some(Role::MutedText),
                ),
                (path, Some(Role::MutedStrong)),
            ]),
            None => rows.row([(INTERRUPTED.into(), Some(Role::MutedText))]),
        }
        return rows.runs;
    }
    if result.is_error
        && let Some(lines) = web_search_failure_lines(name, &result.content)
    {
        for line in lines {
            rows.row(line.spans);
        }
        return rows.runs;
    }
    if !result.is_error
        && let Some(input) = input
        && let Some(change) = file_change_runs(name, input, glyph, cwd, facts.review.as_ref())
    {
        return change;
    }
    // Like Claude Code's `Artifact(quickstart …)`, the guidance stays for the model; the row
    // only says what was sent.
    if !result.is_error
        && name == "ArtifactRead"
        && let Some(intent) = input
            .filter(|input| input["action"] == "quickstart")
            .and_then(|input| input["intent"].as_str())
    {
        rows.row([(
            format!("Sent {} guidance", safe_text(intent)),
            Some(Role::MutedText),
        )]);
        return rows.runs;
    }
    if !result.is_error
        && let Some(lines) = super::scheduled_tasks::result_lines(name, input, &result.content)
    {
        for line in lines {
            rows.row([(line, Some(Role::MutedText))]);
        }
        return rows.runs;
    }
    if name == BASH_TOOL_NAME {
        if tool_result_status(name, result) == BACKGROUND_RUNNING_STATUS {
            rows.row([(BACKGROUND_RUNNING_STATUS.into(), Some(Role::MutedText))]);
        } else if result.content.trim().is_empty() {
            rows.row([(NO_OUTPUT.into(), Some(Role::MutedText))]);
        } else {
            rows.preview(&result.content, false, body);
        }
        return rows.runs;
    }
    let value = serde_json::from_str::<Value>(&result.content).ok();
    if name == SUBAGENT_TOOL_NAME
        && let Some(value) = &value
    {
        return agent_messaging::subagent_result_runs(
            value,
            result.is_error,
            facts.report.as_ref(),
            glyph,
        );
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
    let mut text = safe_text(status);
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
    rows.row([(text, Some(failure(result.is_error || status == "Failed")))]);
    let mut has_reason = false;
    if let Some(value) = &value {
        for key in ["reason", "error", "message"] {
            if key == "message" && !result.is_error {
                continue;
            }
            if let Some(reason) = value.get(key).filter(|reason| !reason.is_null()) {
                rows.lines(&format!("Reason: {}", display_value(reason)), body);
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
            rows.lines(&format!("Reason: {}", safe_text(reason)), body);
        }
    }
    if let Some(value) = &value {
        if let Some(source) = value["source"].as_str() {
            rows.row([(clipped(source, PREVIEW_LINE_CHARACTERS).0, None)]);
        }
        let preview = value["output"]
            .as_str()
            .or_else(|| value["text"].as_str())
            .or_else(|| value["answer"].as_str());
        if let Some(preview) = preview {
            rows.preview(preview, value["truncated"].as_bool() == Some(true), body);
        } else if value["truncated"].as_bool() == Some(true) {
            rows.row([(DETAILS_HINT.into(), Some(Role::MutedText))]);
        }
    } else if !result.is_error {
        rows.preview(&result.content, false, None);
    }
    rows.runs
}

/// What a declined file change would have done, as Claude Code's
/// `FileEditToolUseRejectedMessage` names it, and the path it names.
fn rejected_file_change(name: &str, input: &Value, cwd: &Path) -> Option<(&'static str, String)> {
    let operation = match name {
        WRITE_TOOL_NAME => "write",
        EDIT_TOOL_NAME if input["old_string"] == "" => "write",
        EDIT_TOOL_NAME => "update",
        _ => return None,
    };
    let path = display_path(input["file_path"].as_str()?, cwd);
    Some((operation, super::safe_line(&path)))
}

fn background_launch_id(content: &str) -> Option<Uuid> {
    let identity = BACKGROUND_LAUNCH_PREFIXES
        .iter()
        .find_map(|prefix| content.strip_prefix(prefix))?;
    let (id, path) = identity.split_once(". Output is being written to: ")?;
    if path.trim().is_empty() {
        return None;
    }
    tickloop_harness::protocol::ShellTaskId::parse(id).map(|task| task.uuid())
}

const GOAL_COMPLETION_REJECTION_PREFIX: &str = "The completion check did not accept the report, so the goal stays active. Its reason, to verify against the current state: ";

pub(super) fn goal_completion_rejection_reason<'a>(
    name: &str,
    content: &'a str,
    is_error: bool,
) -> Option<&'a str> {
    (name == "UpdateGoal" && is_error)
        .then(|| content.strip_prefix(GOAL_COMPLETION_REJECTION_PREFIX))
        .flatten()
}

pub(super) fn tool_result_state(name: &str, content: &str, is_error: bool) -> ToolCallState {
    if goal_completion_rejection_reason(name, content, is_error).is_some() {
        ToolCallState::Warning
    } else if is_error {
        ToolCallState::Failed
    } else {
        ToolCallState::Finished
    }
}

fn tool_result_status(name: &str, result: &ToolResult) -> &'static str {
    if name == BASH_TOOL_NAME && background_launch_id(&result.content).is_some() {
        BACKGROUND_RUNNING_STATUS
    } else if goal_completion_rejection_reason(name, &result.content, result.is_error).is_some() {
        "Not complete yet"
    } else if result.is_error {
        "Failed"
    } else {
        "Completed"
    }
}

/// A `!` command moved to the background, as its saved output reads once it gets there.
pub(super) fn backgrounded_shell_runs(display: DisplayProfile) -> RoleRuns {
    let mut response = RoleRuns::new();
    push_response_block(
        &mut response,
        BACKGROUND_RUNNING_STATUS,
        OutputDetail::Preview,
        display,
        Some(Role::MutedText),
    );
    response
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

/// A successful `Write` or `Edit` as Claude Code shows it: `Wrote N lines to path` over the first
/// lines of the file, or `Added N lines, removed M lines` over the change as a coloured diff
/// numbered with the file's lines from the edit's recorded review. Without the review the diff
/// comes from the call's own strings and carries no line numbers.
fn file_change_runs(
    name: &str,
    input: &Value,
    glyph: &str,
    cwd: &Path,
    review: Option<&FileChangeReview>,
) -> Option<RoleRuns> {
    let path = super::safe_line(&display_path(input["file_path"].as_str()?, cwd));
    match name {
        WRITE_TOOL_NAME => Some(written_runs(&path, input["content"].as_str()?, glyph)),
        EDIT_TOOL_NAME => {
            let (old, new) = (input["old_string"].as_str()?, input["new_string"].as_str()?);
            if old.is_empty() {
                return Some(written_runs(&path, new, glyph));
            }
            let replace_all = input["replace_all"] == true || input["replace_all"] == "true";
            let rows = match review {
                Some(review) => review_diff_rows(review),
                None => string_diff_rows(old, new),
            };
            Some(diff_runs(rows, replace_all, glyph))
        }
        _ => None,
    }
}

fn plural(count: usize, word: &str) -> String {
    if count == 1 {
        word.to_owned()
    } else {
        format!("{word}s")
    }
}

fn written_runs(path: &str, content: &str, glyph: &str) -> RoleRuns {
    let count = content.lines().count();
    let mut rows = ResponseRows::new(glyph);
    rows.row([
        ("Wrote ".into(), None),
        (count.to_string(), Some(Role::Strong)),
        (format!(" {} to ", plural(count, "line")), None),
        (path.to_owned(), Some(Role::Strong)),
    ]);
    for line in code_lines(path, content)
        .into_iter()
        .take(WRITE_PREVIEW_LINES)
    {
        rows.row(clipped_runs(line, PREVIEW_LINE_CHARACTERS));
    }
    if count > WRITE_PREVIEW_LINES {
        rows.row([(
            format!(
                "… +{} {} (ctrl+o to expand)",
                count - WRITE_PREVIEW_LINES,
                plural(count - WRITE_PREVIEW_LINES, "line")
            ),
            Some(Role::MutedText),
        )]);
    }
    rows.runs
}

/// `content` as runs per line, coloured as code when the file type of `path` is known.
fn code_lines(path: &str, content: &str) -> Vec<RoleRuns> {
    match tickloop_ui::highlight::highlight_path(path, content) {
        Some(lines) => lines
            .iter()
            .map(|line| {
                line.iter()
                    .map(|(text, token)| (safe_text(text), token.map(Role::code)))
                    .collect()
            })
            .collect(),
        None => content
            .lines()
            .map(|line| vec![(safe_text(line), None)])
            .collect(),
    }
}

/// One row of an edit's diff: a line of the file, or the gap between two hunks.
struct DiffRow {
    kind: DiffRowKind,
    /// The line's number in the file before (removed lines) or after the edit.
    number: Option<u64>,
    /// The code, with changed words emphasized.
    spans: RoleRuns,
}

#[derive(Clone, Copy, PartialEq)]
enum DiffRowKind {
    Context,
    Added,
    Removed,
    Gap,
}

/// The diff of an edit from its recorded review: each hunk's context and changed lines with
/// their file line numbers, the hunks apart.
fn review_diff_rows(review: &FileChangeReview) -> Vec<DiffRow> {
    let mut rows = Vec::new();
    for (index, hunk) in review.hunks.iter().enumerate() {
        if index > 0 {
            rows.push(DiffRow {
                kind: DiffRowKind::Gap,
                number: None,
                spans: vec![(HUNK_GAP.into(), Some(Role::MutedText))],
            });
        }
        let mut lines = hunk.lines.iter().peekable();
        while let Some(line) = lines.next() {
            if line.kind == FileChangeLineKind::Context {
                rows.push(DiffRow {
                    kind: DiffRowKind::Context,
                    number: line.new_line,
                    spans: vec![(safe_text(&line.text), None)],
                });
                continue;
            }
            let mut changed = vec![line];
            while let Some(next) = lines.next_if(|next| next.kind != FileChangeLineKind::Context) {
                changed.push(next);
            }
            let text_of = |kind| {
                changed
                    .iter()
                    .filter(|line| line.kind == kind)
                    .map(|line| format!("{}\n", line.text))
                    .collect::<String>()
            };
            let mut removed_numbers = changed
                .iter()
                .filter(|line| line.kind == FileChangeLineKind::Removed)
                .map(|line| line.old_line);
            let mut added_numbers = changed
                .iter()
                .filter(|line| line.kind == FileChangeLineKind::Added)
                .map(|line| line.new_line);
            for mut row in string_diff_rows(
                &text_of(FileChangeLineKind::Removed),
                &text_of(FileChangeLineKind::Added),
            ) {
                row.number = match row.kind {
                    DiffRowKind::Removed => removed_numbers.next().flatten(),
                    DiffRowKind::Added => added_numbers.next().flatten(),
                    DiffRowKind::Context => {
                        removed_numbers.next();
                        added_numbers.next().flatten()
                    }
                    DiffRowKind::Gap => None,
                };
                rows.push(row);
            }
        }
    }
    rows
}

/// The diff between two texts, changed words emphasized, without file line numbers.
fn string_diff_rows(old: &str, new: &str) -> Vec<DiffRow> {
    use similar::ChangeTag;
    let diff = similar::TextDiff::from_lines(old, new);
    let mut rows = Vec::new();
    for operation in diff.ops() {
        for change in diff.iter_inline_changes(operation) {
            let (kind, line_role, word_role) = match change.tag() {
                ChangeTag::Delete => (
                    DiffRowKind::Removed,
                    Some(Role::DiffLineRemoved),
                    Some(Role::DiffWordRemoved),
                ),
                ChangeTag::Insert => (
                    DiffRowKind::Added,
                    Some(Role::DiffLineAdded),
                    Some(Role::DiffWordAdded),
                ),
                ChangeTag::Equal => (DiffRowKind::Context, None, None),
            };
            let mut spans = RoleRuns::new();
            for (emphasized, value) in change.iter_strings_lossy() {
                let value = safe_text(value.trim_end_matches(['\n', '\r']));
                if !value.is_empty() {
                    spans.push((value, if emphasized { word_role } else { line_role }));
                }
            }
            rows.push(DiffRow {
                kind,
                number: None,
                spans,
            });
        }
    }
    rows
}

/// An edit as Claude Code shows it: the count of added and removed lines, then the diff rows,
/// each led by its line number and its `+`/`-` marker right before the code, as Claude Code's
/// `StructuredDiff` numbers them.
fn diff_runs(diff: Vec<DiffRow>, replace_all: bool, glyph: &str) -> RoleRuns {
    let added = diff
        .iter()
        .filter(|row| row.kind == DiffRowKind::Added)
        .count();
    let removed = diff
        .iter()
        .filter(|row| row.kind == DiffRowKind::Removed)
        .count();
    let mut summary = RoleRuns::new();
    if added > 0 {
        summary.push(("Added ".into(), None));
        summary.push((added.to_string(), Some(Role::Strong)));
        summary.push((format!(" {}", plural(added, "line")), None));
    }
    if added > 0 && removed > 0 {
        summary.push((", ".into(), None));
    }
    if removed > 0 {
        summary.push((
            if added == 0 { "Removed " } else { "removed " }.into(),
            None,
        ));
        summary.push((removed.to_string(), Some(Role::Strong)));
        summary.push((format!(" {}", plural(removed, "line")), None));
    }
    if added == 0 && removed == 0 {
        summary.push(("No lines changed".into(), Some(Role::MutedText)));
    }
    if replace_all {
        summary.push((" · every occurrence".into(), Some(Role::MutedText)));
    }
    let mut rows = ResponseRows::new(glyph);
    rows.row(summary);
    // Claude Code pads numbers to one column more than the widest needs.
    let number_width = diff
        .iter()
        .filter_map(|row| row.number)
        .max()
        .map_or(0, |widest| widest.to_string().len() + 1);
    let hidden = diff.len().saturating_sub(DIFF_PREVIEW_LINES);
    for row in diff.into_iter().take(DIFF_PREVIEW_LINES) {
        let (marker, marker_role, gutter_role) = match row.kind {
            DiffRowKind::Added => ("+", Some(Role::DiffMarkerAdded), Some(Role::DiffLineAdded)),
            DiffRowKind::Removed => (
                "-",
                Some(Role::DiffMarkerRemoved),
                Some(Role::DiffLineRemoved),
            ),
            DiffRowKind::Context => (" ", None, Some(Role::MutedText)),
            DiffRowKind::Gap => {
                rows.row(row.spans);
                continue;
            }
        };
        let mut spans = RoleRuns::new();
        if number_width > 0 {
            let number = row
                .number
                .map(|number| number.to_string())
                .unwrap_or_default();
            spans.push((format!("{number:>number_width$} "), gutter_role));
        }
        spans.push((marker.into(), marker_role));
        spans.extend(row.spans);
        rows.row(spans);
    }
    if hidden > 0 {
        rows.row([(
            format!("… +{hidden} {} (ctrl+o to expand)", plural(hidden, "line")),
            Some(Role::MutedText),
        )]);
    }
    rows.runs
}

/// `runs` cut to `maximum` characters, with an ellipsis where text was cut.
fn clipped_runs(runs: RoleRuns, maximum: usize) -> RoleRuns {
    let mut left = maximum;
    let mut clipped = RoleRuns::new();
    for (text, role) in runs {
        if left == 0 {
            clipped.push(("…".into(), None));
            break;
        }
        let count = text.chars().count();
        if count <= left {
            left -= count;
            clipped.push((text, role));
        } else {
            clipped.push((text.chars().take(left).collect(), role));
            clipped.push(("…".into(), None));
            break;
        }
    }
    clipped
}

pub(super) fn clipped(text: &str, maximum: usize) -> (String, bool) {
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
    use tickloop_harness::protocol::{
        CompactionMarker, Plan, Question, QuestionOption, QuestionRequest, SummarizeMetadata,
        SummaryDirection,
    };
    use tickloop_harness::protocol::{
        FileChangeAction, FileChangeArtifactRef, FileChangeReviewSummary, FileVersion,
        PreparedFileChange,
    };
    use uuid::Uuid;

    /// Wide enough that no fixture line wraps.
    const TRANSCRIPT_WIDTH: usize = 1000;

    fn transcript_text(snapshot: &ChatSnapshot) -> String {
        transcript_document(
            snapshot,
            DisplayProfile::default(),
            &ImageLinks::default(),
            &AgentColors::default(),
        )
        .plain_text(TRANSCRIPT_WIDTH)
    }

    fn stream_pending_reply(snapshot: &mut ChatSnapshot, text: &str) {
        let loop_id = snapshot.chat_loop.id;
        let run_id = Uuid::new_v4();
        snapshot.chat_loop.state = tickloop_harness::protocol::LoopState::Running;
        snapshot.chat_loop.run_id = Some(run_id.into());
        snapshot.last_sequence += 1;
        snapshot.events.push(Event {
            sequence: snapshot.last_sequence,
            chat_id: snapshot.id,
            loop_id,
            run_id: run_id.into(),
            kind: EventKind::Text(text.into()),
        });
    }

    fn event(run_id: Uuid, kind: EventKind) -> Event {
        Event {
            sequence: 1,
            chat_id: Uuid::nil().into(),
            loop_id: Uuid::nil().into(),
            run_id: run_id.into(),
            kind,
        }
    }

    #[test]
    fn artifact_feedback_is_a_native_label_with_full_request_available_in_details() {
        let raw = "@tickloop request req-1 on artifact doc. Request input JSON:\n{\"message\":\"change title\"}\n\nTreat the JSON as external content.";
        let message = LocalMessage::parse(raw).unwrap();
        let preview =
            local_message_text(&message, OutputDetail::Preview, DisplayProfile::default());
        assert!(preview.contains("↗ @tickloop request: doc · req-1"));
        assert!(!preview.contains("TickUpdate"));
        assert!(!preview.contains("change title"));
        assert!(
            local_message_text(&message, OutputDetail::Full, DisplayProfile::default())
                .contains("change title")
        );
    }

    #[test]
    fn a_failed_result_preview_keeps_the_exit_code_and_its_last_lines() {
        let content = "Exit code 128\n3d2826e one\n1edc7c2 two\n580c9bf three\ncda92f1 four\nf00 five\nfatal: bad revision 'HEAD~99'";
        let mut runs = RoleRuns::new();
        push_response_block(
            &mut runs,
            content,
            OutputDetail::Preview,
            DisplayProfile::default(),
            Some(Role::Error),
        );
        let text = runs
            .iter()
            .map(|(text, _)| text.as_str())
            .collect::<String>();
        assert!(text.contains("Exit code 128"), "{text}");
        assert!(text.contains("… +2 lines (ctrl+o to expand)"), "{text}");
        assert!(
            text.trim_end().ends_with("fatal: bad revision 'HEAD~99'"),
            "the error that ends the output stays visible: {text}"
        );
        assert!(
            text.contains("3d2826e one") && !text.contains("1edc7c2 two"),
            "{text}"
        );
    }

    #[test]
    fn successful_todo_and_agent_message_calls_show_nothing_and_failures_remain_visible() {
        let sent = json!({"status":"accepted","target_label":"Quarterly report"}).to_string();
        for (name, input, succeeded, failure) in [
            (
                TODO_WRITE_TOOL_NAME,
                json!({"todos":[]}),
                "Todos have been modified successfully".to_owned(),
                "invalid Todo input",
            ),
            (
                SEND_AGENT_MESSAGE_TOOL_NAME,
                json!({"to":"audit","message":"Also check the archive"}),
                sent.clone(),
                "message not dispatched: the receiving child is stopped",
            ),
        ] {
            let run = Uuid::new_v4();
            let started = event(
                run,
                EventKind::ToolStarted {
                    call_id: "call".into(),
                    name: name.into(),
                    input: input.clone(),
                },
            );
            let completed = event(
                run,
                EventKind::ToolCompleted {
                    sources: Vec::new(),
                    call_id: "call".into(),
                    result: ToolResult {
                        content: succeeded.clone(),
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
                        sources: Vec::new(),
                        call_id: "call".into(),
                        result: ToolResult {
                            content: failure.into(),
                            is_error: true,
                            output_ref: None,
                        },
                    },
                );
                assert!(tools.render_event(&failed).unwrap().contains(failure));
            }
            let snapshot: ChatSnapshot = serde_json::from_value(json!({
                "id":Uuid::new_v4(), "folder":"/work", "messages":[
                    {"role":"assistant", "content":[{"type":"tool_use","id":"ok","name":name,"input":input}]},
                    {"role":"user", "content":[{"type":"tool_result","tool_use_id":"ok","content":succeeded,"is_error":false}]},
                    {"role":"assistant", "content":[{"type":"tool_use","id":"bad","name":name,"input":input}]},
                    {"role":"user", "content":[{"type":"tool_result","tool_use_id":"bad","content":failure,"is_error":true}]}
                ],"chat_loop":{"id":"00000000-0000-0000-0000-000000000000","state":"Idle"}
            })).unwrap();
            let compact =
                super::super::workflows::public_history(&snapshot, DisplayProfile::default());
            let full = transcript_text(&snapshot);
            for visible in [compact, full] {
                assert!(!visible.contains(&succeeded), "{visible}");
                assert!(!visible.contains("Quarterly report"), "{visible}");
                assert!(!visible.contains("tool_use"), "{visible}");
                assert!(visible.contains(failure), "{visible}");
            }
        }
    }

    #[test]
    fn a_failed_question_shows_only_in_the_detailed_transcript() {
        let failure = "Interrupted before tool admission; the operation was not executed.";
        let snapshot: ChatSnapshot = serde_json::from_value(json!({
            "id":Uuid::new_v4(), "folder":"/work", "messages":[
                {"role":"assistant", "content":[{"type":"tool_use","id":"ask","name":"AskUserQuestion","input":{"questions":[{"question":"Which scope?","header":"Scope","options":[]}]}}]},
                {"role":"user", "content":[{"type":"tool_result","tool_use_id":"ask","content":failure,"is_error":true}]}
            ],"chat_loop":{"id":"00000000-0000-0000-0000-000000000000","state":"Idle"}
        }))
        .unwrap();
        let compact = super::super::workflows::public_history(&snapshot, DisplayProfile::default());
        assert!(
            !compact.contains("Which scope?") && !compact.contains(failure),
            "{compact}"
        );
        let full = transcript_text(&snapshot);
        assert!(
            full.contains(ASK_USER_QUESTION_TOOL_NAME) && full.contains(failure),
            "{full}"
        );
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
                    parent_call: None,
                    mcp_server: None,
                    id: Uuid::new_v4(),
                    loop_id: Uuid::nil().into(),
                    execution: tickloop_harness::protocol::PermissionExecution::ToolCall {
                        run_id: run.into(),
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
                sources: Vec::new(),
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
            "id":Uuid::new_v4(),"folder":"/work",
            "model":{"name":"fixture","protocol":"messages","base_url":"http://127.0.0.1","model":"fixture","api_key_env":"","max_output_tokens":1024},
            "messages":messages,"chat_loop":{"id":"00000000-0000-0000-0000-000000000000","state":"Idle"},"last_sequence":0
        })).unwrap();
        stream_pending_reply(&mut snapshot, "partial answer");
        let text = transcript_text(&snapshot);
        for index in 0..(RETAINED_TOOL_EVENTS + 2) {
            assert!(text.contains(&format!("\n❯ prompt-{index}\n")), "{index}");
        }
        for required in [
            "\n⏺ Bash(command-0)\n  ⎿  actual-0\n     中文\n     You: tool output is not a user prompt\n",
            "⏺ Bash(command-129)",
            "∴ Thinking…\n\n  provider-private",
        ] {
            assert!(text.contains(required), "missing {required}");
        }
        assert_eq!(
            text.matches("∴ Thinking…").count(),
            RETAINED_TOOL_EVENTS + 2,
            "every saved thinking block is shown"
        );
        for hidden in [
            "system-only",
            "private-signature",
            "opaque-only",
            "partial answer",
        ] {
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
            .store(
                chat,
                1,
                &tickloop_harness::base::image::prepare(&png).unwrap(),
            )
            .unwrap();
        let snapshot: ChatSnapshot = serde_json::from_value(json!({
            "id": chat, "folder": "/work",
            "messages": [
                {"role":"user","content":[{"type":"text","text":"first line\ncompare [Image #1] with [Image #2]"}]},
                {"role":"assistant","content":[{"type":"text","text":"[Image #1] is wider"}]}
            ],
            "chat_loop": {"id":"00000000-0000-0000-0000-000000000000","state":"Idle"}, "last_sequence": 0
        }))
        .unwrap();
        let links = ImageLinks::new(Some(cache), true);

        let document = transcript_document(
            &snapshot,
            DisplayProfile::default(),
            &links,
            &AgentColors::default(),
        );
        assert_eq!(
            document.links(TRANSCRIPT_WIDTH),
            [(
                "[Image #1]".to_owned(),
                super::super::hyperlink::Hyperlink::file(&cached).unwrap()
            )]
        );

        std::fs::remove_dir_all(root.path().join("image-cache")).unwrap();
        let expired = transcript_document(
            &snapshot,
            DisplayProfile::default(),
            &links,
            &AgentColors::default(),
        );
        assert!(expired.links(TRANSCRIPT_WIDTH).is_empty());
        assert_eq!(
            expired.plain_text(TRANSCRIPT_WIDTH),
            document.plain_text(TRANSCRIPT_WIDTH)
        );
    }

    #[test]
    fn transcript_lays_out_complete_markdown_replies_and_leaves_out_streaming_ones() {
        let mut snapshot: ChatSnapshot = serde_json::from_value(json!({
            "id":Uuid::new_v4(),"folder":"/work",
            "model":{"name":"fixture","protocol":"messages","base_url":"http://127.0.0.1","model":"fixture","api_key_env":"","max_output_tokens":1024},
            "messages":[
                {"role":"user","content":[{"type":"text","text":"show markdown"}]},
                {"role":"assistant","content":[{"type":"text","text":"## Result\n\n- [Docs](https://example.com)\n```rust\nfn done() {}\n```"}]}
            ],
            "chat_loop":{"id":"00000000-0000-0000-0000-000000000000","state":"Idle"},"last_sequence":0
        }))
        .unwrap();
        stream_pending_reply(&mut snapshot, "Before\n```text\nunfinished\u{1b}[31m");

        let text = transcript_text(&snapshot);
        assert!(text.contains("\n❯ show markdown\n\n⏺ Result\n"), "{text}");
        assert!(text.contains("Docs"), "{text}");
        assert!(text.contains("fn done() {}"), "{text}");
        assert!(
            !text.contains("Before") && !text.contains("unfinished"),
            "{text}"
        );
        assert!(!text.contains("```"));
        assert!(!text.contains('\u{1b}'));
    }

    #[test]
    fn full_archive_shows_model_thinking_without_exposing_protocol_state() {
        let mut snapshot: ChatSnapshot = serde_json::from_value(json!({
            "id": Uuid::new_v4(), "folder": "/work", "messages": [
                {"role":"assistant","content":[
                    {"type":"thinking","thinking":"Compare the prices first.","signature":"secret-signature"},
                    {"type":"text","text":"The first price is lower."}
                ]}
            ], "chat_loop": {"id":"00000000-0000-0000-0000-000000000000","state":"Idle"}
        }))
        .unwrap();
        snapshot.model.model = "deepseek-flash".into();

        let text = transcript_text(&snapshot);
        assert!(text.contains("∴ Thinking…\n\n  Compare the prices first."));
        assert!(text.contains("The first price is lower."));
        assert!(!text.contains("secret-signature"));
    }

    #[test]
    fn full_archive_shows_responses_reasoning_text_without_encrypted_state() {
        let snapshot: ChatSnapshot = serde_json::from_value(json!({
            "id": Uuid::new_v4(), "folder": "/work", "messages": [
                {"role":"assistant","content":[
                    {"type":"protocol_data","protocol":"responses","value":{
                        "type":"reasoning","id":"rs-1","summary":[],
                        "content":[{"type":"reasoning_text","text":"Compare both filings."}],
                        "encrypted_content":"opaque-state"
                    }},
                    {"type":"text","text":"The figures differ."}
                ]}
            ], "chat_loop": {"id":"00000000-0000-0000-0000-000000000000","state":"Idle"}
        }))
        .unwrap();
        let text = transcript_text(&snapshot);
        assert!(text.contains("∴ Thinking…\n\n  Compare both filings."));
        assert!(text.contains("The figures differ."));
        assert!(!text.contains("opaque-state"));
    }

    #[test]
    fn transcript_distinguishes_a_responses_summary_from_raw_reasoning() {
        let snapshot: ChatSnapshot = serde_json::from_value(json!({
            "id": Uuid::new_v4(), "folder": "/work", "messages": [
                {"role":"assistant","content":[
                    {"type":"protocol_data","protocol":"responses","value":{
                        "type":"reasoning","id":"rs-1",
                        "summary":[{"type":"summary_text","text":"Checked the totals."}],
                        "content":[{"type":"reasoning_text","text":"private raw reasoning"}],
                        "encrypted_content":"opaque-state"
                    }}
                ]}
            ], "chat_loop": {"id":"00000000-0000-0000-0000-000000000000","state":"Idle"}
        }))
        .unwrap();

        let text = transcript_text(&snapshot);
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
            "id": chat_id, "folder": "/work", "messages": [
                {"role":"user","content":[{"type":"text","text":"Compare values"}]}
            ], "chat_loop": {"id":loop_id,"state":"Running","run_id":run_id}
        }))
        .unwrap();
        snapshot
            .pending_thinking
            .insert(run_id.into(), "First check\nThen compare".into());

        let text = transcript_text(&snapshot);
        assert!(text.contains("∴ Thinking…\n\n  First check\n  Then compare"));
    }

    #[test]
    fn transcript_renders_local_messages_like_claude_code_and_keeps_command_reasons_visible() {
        let mut snapshot: ChatSnapshot = serde_json::from_value(json!({
            "id": Uuid::new_v4(), "folder": "/work",
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
            "chat_loop": {"id":"00000000-0000-0000-0000-000000000000","state":"Idle"}, "last_sequence": 20
        })).unwrap();
        snapshot.commands = vec![serde_json::from_value(json!({
            "operation_id":Uuid::new_v4(), "loop_id":Uuid::new_v4(), "command":"sleep 30", "cwd":"/work/subdir", "pid":null, "state":"Unknown", "exit_code":null, "signal":null, "output_ref":"/output/task.log", "output_bytes":0,
            "reason":"unresolved descendants\u{1b}[31m"
        })).unwrap()];
        let text = transcript_text(&snapshot);
        assert_eq!(text.matches("\n❯ ").count(), 2, "{text}");
        for (earlier, later) in [
            ("first prompt", "first unfinished reply"),
            ("first unfinished reply", INTERRUPTED_NOTICE),
            (INTERRUPTED_NOTICE, "! printf '<hi>'"),
            ("! printf '<hi>'", "  ⎿  <hi>"),
            (
                "  ⎿  <hi>",
                "⏺ Background command \"sleep 1\" completed (exit code 0)",
            ),
            ("⏺ Background command", "❯ second prompt"),
            ("❯ second prompt", "⏺ Shell(sleep 30)"),
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
    fn an_agent_message_folds_into_one_line_and_full_detail_labels_verified_relay() {
        let text = format!(
            "<agent-message>{}</agent-message>",
            json!({"source":{"sender_relationship":"sibling","sender_label":"Audit report","sender_handle":"audit","relay":{"original_chat_id":Uuid::new_v4(),"original_message_id":Uuid::new_v4(),"original_text":"Human instruction","forwarded_verbatim":true},"is_result":false},"message":"Agent body first line\nAgent body detail"})
        );
        let local = LocalMessage::parse(&text).unwrap();
        let display = DisplayProfile::default();
        let preview = local_message_text(&local, OutputDetail::Preview, display);
        assert_eq!(preview, "\n› Message from @audit (ctrl+o to expand)\n");
        let full = local_message_text(&local, OutputDetail::Full, display);
        assert_eq!(
            full,
            "\n› Message from @audit\n  Verified user relay · verbatim\n  Human instruction\n  Agent body first line\n  Agent body detail\n"
        );
    }

    #[test]
    fn two_child_messages_name_their_senders_and_keep_hashes_for_the_detailed_transcript() {
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let agent_message = |id: Uuid, label: &str, body: &str| {
            format!(
                "<agent-message>{}</agent-message>",
                json!({"source":{"message_id":id,"sender_chat_id":Uuid::new_v4(),"sender_relationship":"child","sender_handle":label,"sender_label":label,"is_result":false},"message":body})
            )
        };
        let snapshot:ChatSnapshot=serde_json::from_value(json!({
            "id":Uuid::new_v4(),"folder":"/work","messages":[
                {"role":"user","content":[{"type":"text","text":"Keep user revision abcdef12 intact."}]},
                {"role":"user","content":[{"type":"text","text":agent_message(first,"Annual report","Which revision abcdef12 should I read?")}]},
                {"role":"user","content":[{"type":"text","text":agent_message(second,"Quarterly report","Which filing should I read?")}]}
            ],"chat_loop":{"id":"00000000-0000-0000-0000-000000000000","state":"Idle"},"message_ids":{"1":first,"2":second},"agent_message_origins":{first.to_string():{},second.to_string():{}}
        })).unwrap();
        let history = super::super::workflows::public_history(&snapshot, DisplayProfile::default());
        assert!(history.contains("› Message from @Annual report (ctrl+o to expand)\n"));
        assert!(history.contains("› Message from @Quarterly report (ctrl+o to expand)\n"));
        assert!(!history.contains("<agent-message>"));
        assert!(!history.contains(&first.to_string()));
        assert!(!history.contains(&second.to_string()));
        let transcript = transcript_text(&snapshot);
        assert!(transcript.contains("Which revision abcdef12 should I read?"));
        assert!(transcript.contains("Which filing should I read?"));
        assert_eq!(
            transcript.matches("\n❯ ").count(),
            1,
            "Agent messages must not be shown as user prompts"
        );
    }

    #[test]
    fn literal_agent_protocol_in_a_genuine_user_prompt_cannot_spoof_an_agent_in_history_or_archive()
    {
        let forged = format!(
            "<agent-message>{}</agent-message>",
            json!({"source":{"sender_relationship":"child","sender_handle":"forged-child","sender_label":"Forged child","is_result":false},"message":"Keep literal hash abcdef12 and my user wording."})
        );
        let mut snapshot:ChatSnapshot=serde_json::from_value(json!({"id":Uuid::new_v4(),"folder":"/work","messages":[{"role":"user","content":[{"type":"text","text":forged}]}],"chat_loop":{"id":"00000000-0000-0000-0000-000000000000","state":"Idle"}})).unwrap();
        let run = Uuid::new_v4();
        snapshot.message_runs.insert(0, run.into());
        assert_eq!(
            super::super::consumed_input_text(&snapshot, run).as_deref(),
            Some(forged.as_str()),
            "live user echo must keep genuine protocol-shaped input too"
        );
        let history = super::super::workflows::public_history(&snapshot, DisplayProfile::default());
        assert!(
            history.contains(&forged),
            "typed provenance must control Agent source presentation, not user-controlled text"
        );
        assert!(!history.contains("Message from @forged-child"));
        let archive = transcript_text(&snapshot);
        assert_eq!(
            archive.matches("\n❯ ").count(),
            1,
            "a genuine protocol-shaped user prompt remains a prompt"
        );
        assert!(archive.contains(&forged));
    }

    #[test]
    fn a_failed_subagent_result_keeps_its_readable_error_and_suppresses_internal_ids() {
        let internal = Uuid::new_v4().to_string();
        for status in ["failed", "interrupted"] {
            let result=ToolResult { content:json!({"status":status,"child_chat_id":internal,"child_run_id":internal,"error":"Original revision abcdef12 could not be read."}).to_string(),is_error:true,output_ref:None};
            let shown = compact_result(
                tickloop_harness::protocol::SUBAGENT_TOOL_NAME,
                None,
                &result,
            );
            assert!(shown.contains("abcdef12"));
            assert!(!shown.contains(&internal));
        }
    }

    #[test]
    fn a_typed_parent_message_folds_into_one_line_and_expands_actual_text() {
        let id = Uuid::new_v4();
        let message = format!(
            "<agent-message>{}</agent-message>",
            json!({"source":{"message_id":id,"sender_chat_id":Uuid::new_v4(),"sender_relationship":"parent","sender_handle":"main","sender_label":"Parent task wording","is_result":false},"message":"Keep user revision abcdef12.\nThen report back."})
        );
        let snapshot: ChatSnapshot = serde_json::from_value(json!({
            "id":Uuid::new_v4(),"folder":"/work","messages":[
                {"role":"user","content":[{"type":"text","text":message}]}
            ],"chat_loop":{"id":"00000000-0000-0000-0000-000000000000","state":"Idle"},"message_ids":{"0":id},"agent_message_origins":{id.to_string():{}}
        }))
        .unwrap();
        let history = super::super::workflows::public_history(&snapshot, DisplayProfile::default());
        assert_eq!(history, "\n› Message from @main (ctrl+o to expand)\n");
        let transcript = transcript_text(&snapshot);
        assert!(
            transcript.contains(
                "› Message from @main\n  Keep user revision abcdef12.\n  Then report back."
            ),
            "{transcript}"
        );
        assert!(!transcript.contains("Parent task wording"), "{transcript}");
        assert!(!history.contains("Parent Agent"));
        assert!(!history.contains(&id.to_string()));
    }

    #[test]
    fn transcript_retains_raw_history_and_sanitizes_durable_summary_markers() {
        let mut snapshot: ChatSnapshot = serde_json::from_value(json!({
            "id": Uuid::new_v4(), "folder": "/work",
            "model": {"name":"fixture","protocol":"messages","base_url":"http://127.0.0.1","model":"fixture","api_key_env":"","max_output_tokens":1024},
            "messages": [
                {"role":"user","content":[{"type":"text","text":"raw prompt remains"}]},
                {"role":"assistant","content":[{"type":"text","text":"raw answer remains"}]}
            ],
            "chat_loop": {"id":"00000000-0000-0000-0000-000000000000","state":"Idle"}, "last_sequence": 0
        }))
        .unwrap();
        snapshot.messages.push(tickloop_harness::protocol::Message {
            role: tickloop_harness::protocol::Role::System,
            content: vec![tickloop_harness::protocol::Content::Text {
                text: "Conversation compacted".into(),
            }],
        });
        snapshot.messages.push(tickloop_harness::protocol::Message {
            role: tickloop_harness::protocol::Role::User,
            content: vec![tickloop_harness::protocol::Content::Text {
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

        let text = transcript_text(&snapshot);
        assert!(text.contains("❯ raw prompt remains"), "{text}");
        assert!(text.contains("⏺ raw answer remains"), "{text}");
        assert!(text.contains("⏺ Conversation compacted"));
        assert!(
            text.contains("⏺ Summarized conversation\n  ⎿  public summary[2J"),
            "{text}"
        );
        assert!(!text.contains("❯ Summarized conversation"));
        assert!(!text.contains('\u{1b}'));
    }

    #[test]
    fn transcript_preserves_visible_behavior_for_projected_interactions_and_outcomes() {
        let chat_id = Uuid::new_v4();
        let loop_id = Uuid::new_v4();
        let run_id = Uuid::new_v4();
        let mut snapshot: ChatSnapshot = serde_json::from_value(json!({
            "id": chat_id, "folder": "/work",
            "model":{"name":"fixture","protocol":"messages","base_url":"http://127.0.0.1","model":"fixture","api_key_env":"","max_output_tokens":1024},
            "messages": [
                {"role":"user", "content":[{"type":"text", "text":"inspect"}]},
                {"role":"assistant", "content":[{"type":"tool_use", "id":"call-1", "name":"Bash", "input":{"command":"pwd"}}]},
                {"role":"user", "content":[{"type":"tool_result", "tool_use_id":"call-1", "content":"/work", "is_error":false}]}
            ],
            "chat_loop": {"id":"00000000-0000-0000-0000-000000000000","state":"Idle"}, "last_sequence": 0
        }))
        .unwrap();
        snapshot.permissions.push(PermissionRequest {
            parent_call: None,
            mcp_server: None,
            id: Uuid::new_v4(),
            loop_id: loop_id.into(),
            execution: tickloop_harness::protocol::PermissionExecution::ToolCall {
                run_id: run_id.into(),
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
            chat_id: chat_id.into(),
            loop_id: loop_id.into(),
            run_id: run_id.into(),
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
            loop_id: loop_id.into(),
            version: 1,
            markdown: "private plan".into(),
            approved_execution_mode: None,
            approval_pending: true,
            edited_by_user: false,
        });
        snapshot.events.push(Event {
            sequence: 1,
            chat_id: chat_id.into(),
            loop_id: loop_id.into(),
            run_id: run_id.into(),
            kind: EventKind::Failed("connection lost".into()),
        });

        let text = transcript_text(&snapshot);
        assert!(
            text.contains("\n❯ inspect\n\n⏺ Bash(pwd)\n  ⎿  /work\n"),
            "{text}"
        );
        assert_eq!(text.matches("A response is waiting.").count(), 1);
        assert!(text.ends_with("\n⏺ connection lost"), "{text}");
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
            parent_call: None,
            mcp_server: None,
            id: Uuid::new_v4(),
            loop_id: Uuid::new_v4().into(),
            execution: tickloop_harness::protocol::PermissionExecution::ToolCall {
                run_id: Uuid::new_v4().into(),
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
                sources: Vec::new(),
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
        assert!(
            rendered.starts_with("  ↳  actual-output\n     界"),
            "the first line sits on the connector row: {rendered}"
        );
        assert!(!rendered.contains("input-only"));
        assert!(!rendered.contains("sixth"));
        assert!(rendered.contains("ctrl+o"));
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
                sources: Vec::new(),
                call_id: "background-call".into(),
                result: result.clone(),
            },
        );
        let mut view = ToolPresentation::new(false);
        view.record(&started);
        view.record(&launched);
        let shown = view.render_event(&launched).unwrap();
        assert_eq!(shown, "  ↳  Running in background\n");
        let command=serde_json::from_value(json!({"operation_id":operation,"loop_id":launched.loop_id,"command":"sleep 30","cwd":"/tmp","pid":null,"state":"Completed","was_background":true,"exit_code":0,"signal":null,"reason":null,"output_ref":path,"output_bytes":0})).unwrap();
        let ended = event(run, EventKind::CommandUpdated { command });
        view.record(&ended);
        assert_eq!(
            view.render_event(&ended),
            None,
            "the command's end reaches the conversation as its task notification"
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
        let archived = joined(&archived_tool_result(
            BASH_TOOL_NAME,
            Some(&input),
            &ToolResult {
                output_ref: None,
                ..result
            },
            Path::new(""),
            DisplayProfile::default(),
            &CallFacts::default(),
        ));
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
            "id":Uuid::new_v4(),"folder":"/work","model":{"name":"fixture","protocol":"messages","base_url":"http://127.0.0.1","model":"fixture","api_key_env":"","max_output_tokens":1024},"chat_loop":{"id":"00000000-0000-0000-0000-000000000000","state":"Idle"},
            "messages":[{"role":"assistant","content":[{"type":"tool_use","id":"bg-call","name":"Bash","input":{"command":"sleep 30","run_in_background":true}}]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"bg-call","content":raw,"is_error":false}]}]
        })).unwrap();
        let restored: ChatSnapshot =
            serde_json::from_str(&serde_json::to_string(&snapshot).unwrap()).unwrap();
        let document = transcript_text(&restored);
        assert!(
            document.ends_with(&format!("\n⏺ Bash(sleep 30)\n  ⎿  {raw}")),
            "{document}"
        );
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
                sources: Vec::new(),
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
        assert_eq!(
            rendered, "  ↳  Completed\n     Delivery is on Friday.\n",
            "the status sits on the connector row and the text under it"
        );
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
            "",
            "as Claude Code, the call shows no row"
        );
        let unmatched = "User has answered your questions: \"Which report?\"=\"Weekly\". You can now continue with the user's answers in mind.";
        let completed = event(
            run,
            EventKind::ToolCompleted {
                sources: Vec::new(),
                call_id: "clarify".into(),
                result: ToolResult {
                    content: unmatched.into(),
                    is_error: false,
                    output_ref: None,
                },
            },
        );
        view.record(&completed);
        assert_eq!(
            view.render_event(&completed).unwrap(),
            format!("\n● {ANSWERED_QUESTIONS}\n  ↳  {unmatched}\n"),
            "a result that does not answer every asked question is shown as it is"
        );
        let answered = event(
            run,
            EventKind::ToolCompleted {
                sources: Vec::new(),
                call_id: "clarify".into(),
                result: ToolResult {

                    content: "User has answered your questions: \"Which report?\"=\"Weekly, with totals\" selected preview:\nRevenue | Costs, \"Which sections?\"=\"Summary, Charts\". You can now continue with the user's answers in mind.".into(),
                    is_error: false,
                    output_ref: None,
                },
            },
        );
        view.record(&answered);
        assert_eq!(
            view.render_event(&answered).unwrap(),
            "\n● User answered the Agent's questions:\n  ↳  · Which report? → Weekly, with totals\n     · Which sections? → Summary, Charts\n"
        );
        assert!(!view.ended_with_rejection(run));
        let EventKind::ToolCompleted { result, .. } = &answered.kind else {
            unreachable!()
        };
        let (heading, rows) =
            question_result_runs(view.started_call(&answered, "clarify").1, result, "⏺", "⎿")
                .unwrap();
        assert_eq!(
            heading,
            [
                ("\n".to_owned(), None),
                ("⏺".to_owned(), Some(Role::Text)),
                (format!(" {ANSWERED_QUESTIONS}\n"), None),
            ],
            "Claude Code's heading keeps the text colour"
        );
        assert!(
            rows.iter()
                .all(|(text, role)| text.trim().is_empty() || *role == Some(Role::MutedText)),
            "the connector and the answers are muted: {rows:?}"
        );
        let cancelled = event(
            run,
            EventKind::ToolCompleted {
                sources: Vec::new(),
                call_id: "clarify".into(),
                result: ToolResult {
                    content: "question was cancelled".into(),
                    is_error: true,
                    output_ref: None,
                },
            },
        );
        view.record(&cancelled);
        assert_eq!(
            view.render_event(&cancelled).unwrap(),
            "",
            "the run's interruption notice stands for a failed question"
        );
    }

    #[test]
    fn web_search_failure_keeps_recovery_actions_out_of_the_error_color_in_both_views() {
        let failure = WebSearchFailure {
            reason: "DuckDuckGo requires human verification".into(),
            suggestions: vec![
                WebSearchSuggestion::OpenVerification {
                    url: "https://html.duckduckgo.com/html/?q=rust".into(),
                    note: "Browser verification may not carry over to HTTP search.".into(),
                },
                WebSearchSuggestion::ChangeProvider {
                    command: "/websearch".into(),
                },
            ],
        };
        let content = serde_json::to_string(&failure).unwrap();
        let result = ToolResult {
            content: content.clone(),
            is_error: true,
            output_ref: None,
        };
        let compact = compact_result_runs(
            WEB_SEARCH_TOOL_NAME,
            None,
            &result,
            "↳",
            Path::new(""),
            &CallFacts::default(),
        );
        let expanded =
            transcript_result_lines(WEB_SEARCH_TOOL_NAME, None, &content, true, None, None)
                .into_iter()
                .flat_map(|line| line.spans)
                .collect::<RoleRuns>();
        for runs in [&compact, &expanded] {
            assert!(
                runs.iter()
                    .any(|(text, role)| text.starts_with("Reason:") && *role == Some(Role::Error))
            );
            assert!(
                runs.iter()
                    .any(|(text, role)| text == "Next steps" && *role == Some(Role::Strong))
            );
            assert!(
                runs.iter()
                    .any(|(text, role)| text.contains("Open the verification page")
                        && *role != Some(Role::Error))
            );
            assert!(
                runs.iter()
                    .any(|(text, role)| text.contains("https://html.duckduckgo.com/")
                        && *role == Some(Role::Link))
            );
            assert!(
                runs.iter()
                    .any(|(text, role)| text == "/websearch" && *role == Some(Role::Accent))
            );
            assert!(
                !joined(runs).contains(DETAILS_HINT),
                "both actions stay visible without expansion"
            );
        }
        assert!(web_search_failure_lines(WEB_FETCH_TOOL_NAME, &content).is_none());
    }

    #[test]
    fn a_declined_question_reads_as_one_line_that_stands_for_the_interruption() {
        let mut view = ToolPresentation::new(false);
        let run = Uuid::new_v4();
        let input =
            json!({"questions":[{"question":"Which scope?","header":"Scope","options":[]}]});
        view.record(&event(
            run,
            EventKind::ToolStarted {
                call_id: "clarify".into(),
                name: ASK_USER_QUESTION_TOOL_NAME.into(),
                input: input.clone(),
            },
        ));
        let result = ToolResult {
            content: DECLINED_QUESTIONS_RESULT.into(),
            is_error: true,
            output_ref: None,
        };
        let declined = event(
            run,
            EventKind::ToolCompleted {
                sources: Vec::new(),
                call_id: "clarify".into(),
                result: result.clone(),
            },
        );
        view.record(&declined);
        assert_eq!(
            view.render_event(&declined).unwrap(),
            "\n● User declined to answer questions\n"
        );
        assert!(view.ended_with_rejection(run));
        let denied = ToolResult {
            content: REJECTION_PREFIXES[0].into(),
            is_error: true,
            output_ref: None,
        };
        assert_eq!(
            question_result_runs(Some(&input), &denied, "⏺", "⎿"),
            question_result_runs(Some(&input), &result, "⏺", "⎿"),
            "a denied call reads as declined, as Claude Code's rejected message"
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
                "Wrote 3 lines to report.md\n     one\n     two\n     three\n".to_owned(),
            ),
            (
                "edit",
                "Edit",
                json!({"file_path":report,"old_string":"one","new_string":"1","replace_all":false}),
                format!("The file {report} has been updated successfully."),
                "Added 1 line, removed 1 line\n     -one\n     +1\n".to_owned(),
            ),
            (
                "replace-all",
                "Edit",
                json!({"file_path":report,"old_string":"one","new_string":"1","replace_all":true}),
                format!(
                    "The file {report} has been updated. All occurrences were successfully replaced."
                ),
                "Added 1 line, removed 1 line · every occurrence\n     -one\n     +1\n".to_owned(),
            ),
            (
                "create-by-edit",
                "Edit",
                json!({"file_path":report,"old_string":"","new_string":"only line"}),
                format!("The file {report} has been updated successfully."),
                "Wrote 1 line to report.md\n     only line\n".to_owned(),
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
                    sources: Vec::new(),
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
            let expected = format!("  ↳  {expected}");
            assert_eq!(
                view.render_event(&completed).unwrap(),
                expected.replace("report.md", report),
                "{call_id}"
            );
            assert_eq!(
                joined(&archived_tool_result(
                    name,
                    Some(&input),
                    &ToolResult {
                        content,
                        is_error: false,
                        output_ref: None,
                    },
                    Path::new("/work"),
                    DisplayProfile::default(),
                    &CallFacts::default(),
                )),
                expected.replace('↳', DisplayProfile::default().glyph(Glyph::ToolResult)),
                "the path is relative to the working directory: {call_id}"
            );
        }
    }

    #[test]
    fn an_edit_shows_its_change_in_the_colour_diff_palette_and_counts_in_bold() {
        let runs = archived_tool_result(
            "Edit",
            Some(&json!({
                "file_path":"/work/main.rs",
                "old_string":"let total = 1;\nkeep();",
                "new_string":"let total = 2;\nkeep();"
            })),
            &ToolResult {
                content: "The file /work/main.rs has been updated successfully.".into(),
                is_error: false,
                output_ref: None,
            },
            Path::new(""),
            DisplayProfile::default(),
            &CallFacts::default(),
        );
        let role_of = |text: &str| {
            runs.iter()
                .find(|(run, _)| run == text)
                .map(|(_, role)| *role)
                .unwrap_or_else(|| panic!("{text:?} in {runs:?}"))
        };
        assert_eq!(role_of("1"), Some(Role::Strong), "the counts are bold");
        assert_eq!(role_of("-"), Some(Role::DiffMarkerRemoved));
        assert_eq!(role_of("+"), Some(Role::DiffMarkerAdded));
        assert!(runs.contains(&("let total = ".into(), Some(Role::DiffLineRemoved))));
        assert!(runs.contains(&("let total = ".into(), Some(Role::DiffLineAdded))));
        assert!(
            runs.iter()
                .any(|(text, role)| text.contains('1') && *role == Some(Role::DiffWordRemoved)),
            "{runs:?}"
        );
        assert!(
            runs.iter()
                .any(|(text, role)| text.contains('2') && *role == Some(Role::DiffWordAdded)),
            "{runs:?}"
        );
        assert!(
            runs.contains(&("keep();".into(), None)),
            "context stays plain"
        );
        assert_eq!(
            role_of(DisplayProfile::default().glyph(Glyph::ToolResult)),
            Some(Role::MutedText)
        );
    }

    #[test]
    fn fold_calls_show_their_query_described_tools_and_batched_calls() {
        let call = |name: &str, input: Value| {
            archived_tool_call(
                name,
                &input,
                Path::new(""),
                DisplayProfile::default(),
                &super::super::tick_labels::TickLabels::default(),
            )
        };
        let searched = call("SearchTools", json!({"query":"ui5 tools"}));
        assert!(searched.contains("SearchTools · ui5 tools"), "{searched}");
        let long = call("SearchTools", json!({"query":"ui5 ".repeat(100)}));
        assert!(long.contains('…') && long.chars().count() < 200, "{long}");
        let described = call(
            "DescribeTools",
            json!({"names":["mcp__ui5__get_version_info","mcp__ui5__run_ui5_linter"]}),
        );
        assert!(
            described
                .contains("DescribeTools · mcp__ui5__get_version_info, mcp__ui5__run_ui5_linter"),
            "{described}"
        );
        let single = call(
            "ExecuteTools",
            json!({"calls":[{"name":"mcp__ui5__get_version_info","arguments":{}}]}),
        );
        assert!(
            single.contains("ExecuteTools · mcp__ui5__get_version_info\n"),
            "{single}"
        );
        let batch = call(
            "ExecuteTools",
            json!({"calls":[
                {"name":"mcp__ui5__get_version_info","arguments":{}},
                {"name":"mcp__ui5__run_ui5_linter","arguments":{}},
                {"name":"mcp__ui5__get_version_info","arguments":{"x":1}}
            ]}),
        );
        assert!(
            batch.contains(
                "ExecuteTools · 3 calls: mcp__ui5__get_version_info, mcp__ui5__run_ui5_linter"
            ),
            "{batch}"
        );
    }

    #[test]
    fn an_artifact_quickstart_names_its_intent_and_sums_up_the_guidance_it_sent() {
        let input = json!({"action":"quickstart","intent":"page"});
        let call = archived_tool_call(
            "ArtifactRead",
            &input,
            Path::new(""),
            DisplayProfile::default(),
            &super::super::tick_labels::TickLabels::default(),
        );
        assert!(call.contains("ArtifactRead · quickstart page"), "{call}");
        let result = joined(&archived_tool_result(
            "ArtifactRead",
            Some(&input),
            &ToolResult {
                content: "Quickstart for a page. The guidance below is the `artifact-design` Skill's own text.\n\n## Page contract - read before your first save\n\nThese are the rules.".into(),
                is_error: false,
                output_ref: None,
            },
            Path::new(""),
            DisplayProfile::default(),
            &CallFacts::default(),
        ));
        assert!(result.contains("Sent page guidance"), "{result}");
        assert!(!result.contains("Page contract"), "{result}");
        assert!(!result.contains("Completed"), "{result}");
    }

    #[test]
    fn a_long_written_file_is_highlighted_and_cut_after_ten_lines() {
        let content = (0..14)
            .map(|index| format!("let value_{index} = {index};"))
            .collect::<Vec<_>>()
            .join("\n");
        let runs = archived_tool_result(
            "Write",
            Some(&json!({"file_path":"/work/values.rs","content":content})),
            &ToolResult {
                content: "File created successfully at: /work/values.rs".into(),
                is_error: false,
                output_ref: None,
            },
            Path::new(""),
            DisplayProfile::default(),
            &CallFacts::default(),
        );
        let text = joined(&runs);
        assert!(
            text.contains("value_9") && !text.contains("value_10"),
            "{text}"
        );
        assert!(text.contains("… +4 lines (ctrl+o to expand)\n"), "{text}");
        assert!(
            runs.contains(&("let".into(), Some(Role::CodeKeyword))),
            "{runs:?}"
        );
        assert!(
            runs.iter()
                .any(|(text, role)| text == "9" && *role == Some(Role::CodeNumber)),
            "{runs:?}"
        );
    }

    #[test]
    fn results_keep_the_terminal_colour_with_muted_connectors_and_red_failures() {
        let bash = archived_tool_result(
            BASH_TOOL_NAME,
            Some(&json!({"command":"ls"})),
            &ToolResult {
                content: "a.txt\nb.txt".into(),
                is_error: false,
                output_ref: None,
            },
            Path::new(""),
            DisplayProfile::default(),
            &CallFacts::default(),
        );
        assert_eq!(joined(&bash), "  ⎿  a.txt\n     b.txt\n", "{bash:?}");
        assert!(bash.contains(&("a.txt".into(), None)), "{bash:?}");
        let failed = archived_tool_result(
            BASH_TOOL_NAME,
            Some(&json!({"command":"false"})),
            &ToolResult {
                content: "Exit code 1\nboom".into(),
                is_error: true,
                output_ref: None,
            },
            Path::new(""),
            DisplayProfile::default(),
            &CallFacts::default(),
        );
        assert_eq!(joined(&failed), "  ⎿  Exit code 1\n     boom\n");
        assert!(
            failed.contains(&("boom".into(), Some(Role::Error))),
            "{failed:?}"
        );
        assert_eq!(
            failed[1].1,
            Some(Role::MutedText),
            "the connector stays muted"
        );
        for content in [
            "permission denied; the requested operation was not executed",
            "The user denied permission for this operation and gave this reason: not now",
        ] {
            let declined = archived_tool_result(
                "Read",
                Some(&json!({"file_path":"/work/a.txt"})),
                &ToolResult {
                    content: content.into(),
                    is_error: true,
                    output_ref: None,
                },
                Path::new(""),
                DisplayProfile::default(),
                &CallFacts::default(),
            );
            assert_eq!(
                joined(&declined),
                format!("{INTERRUPTED_NOTICE}\n"),
                "a declined call other than a file change reads as the interruption"
            );
            assert!(!declined.iter().any(|(_, role)| *role == Some(Role::Error)));
            let declined_write = archived_tool_result(
                "Write",
                Some(&json!({"file_path":"/work/a.txt","content":"x"})),
                &ToolResult {
                    content: content.into(),
                    is_error: true,
                    output_ref: None,
                },
                Path::new("/work"),
                DisplayProfile::default(),
                &CallFacts::default(),
            );
            assert_eq!(
                joined(&declined_write),
                "  ⎿  User rejected write to a.txt\n"
            );
        }
    }

    #[test]
    fn command_updates_stay_out_of_the_compact_conversation() {
        let view = ToolPresentation::new(false);
        let update = |state, was_background| {
            let command: tickloop_harness::protocol::CommandSnapshot =
                serde_json::from_value(json!({
                    "operation_id":Uuid::new_v4(), "loop_id":Uuid::new_v4(), "command":"npm test",
                    "cwd":"/work", "pid":null, "state":state, "was_background":was_background,
                    "exit_code":0, "signal":null, "reason":null, "output_ref":"/output/task.log",
                    "output_bytes":0
                }))
                .unwrap();
            event(Uuid::new_v4(), EventKind::CommandUpdated { command })
        };
        // A foreground command's result and a background command's task notification report
        // what the conversation shows.
        for (state, was_background) in [
            ("Running", false),
            ("Completed", false),
            ("Background", true),
            ("Completed", true),
        ] {
            assert_eq!(
                view.render_event(&update(state, was_background)),
                None,
                "{state} {was_background}"
            );
        }
    }

    #[test]
    fn role_rendering_keeps_complete_tool_output_in_detailed_mode() {
        let completed = event(
            Uuid::new_v4(),
            EventKind::ToolCompleted {
                sources: Vec::new(),
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
            .render_event_roles(
                &completed,
                DisplayProfile::default(),
                Path::new(""),
                &CallFacts::default(),
            )
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
                sources: Vec::new(),
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
        assert!(rendered.starts_with("  ↳  Failed\n"), "{rendered}");
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
            parent_call: None,
            mcp_server: None,
            id: Uuid::new_v4(),
            loop_id: Uuid::new_v4().into(),
            execution: tickloop_harness::protocol::PermissionExecution::ToolCall {
                run_id: Uuid::new_v4().into(),
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
            parent_call: None,
            mcp_server: None,
            id: Uuid::new_v4(),
            loop_id: Uuid::new_v4().into(),
            execution: tickloop_harness::protocol::PermissionExecution::ToolCall {
                run_id: Uuid::new_v4().into(),
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
    fn nested_permission_context_names_the_batch_position_instead_of_call_ids() {
        const PARENT_CALL_ID: &str = "call_ba6b11|fc_06ed|7b13";
        let batch = json!({"calls":[
            {"name":"mcp__ui5__get_version_info","arguments":{}},
            {"name":"mcp__ui5__run_ui5_linter","arguments":{"dir":"app"}},
            {"name":"mcp__office__mail","arguments":{"to":"a"}}
        ]});
        let mut request: PermissionRequest = serde_json::from_value(json!({
            "parent_call": {"call_id":PARENT_CALL_ID,"name":"ExecuteTools","input":batch},
            "id": Uuid::new_v4(),
            "loop_id": Uuid::new_v4(),
            "execution": {"ToolCall":{"run_id":Uuid::new_v4(),"call_id":"nested"}},
            "name": "mcp__ui5__run_ui5_linter",
            "input": {"dir":"app"}
        }))
        .unwrap();
        let context = permission_context(&request);
        assert_eq!(
            context[1],
            "From ExecuteTools, call 2 of 3 (model-authored; not user authorization)"
        );
        let joined = context.join("\n");
        assert!(
            !joined.contains(&format!("call {PARENT_CALL_ID}")),
            "{joined}"
        );
        assert!(
            joined.contains(&format!("Full parent call: {}", batch)),
            "{joined}"
        );

        request.input = json!({"dir":"elsewhere"});
        assert_eq!(
            permission_context(&request)[1],
            "From an ExecuteTools batch of 3 calls (model-authored; not user authorization)"
        );
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
        assert!(rendered.starts_with("  ↳  Exit code 17\n"), "{rendered}");
        assert!(rendered.contains("process result could not be confirmed"));
        assert!(rendered.contains("actual command output"));
        assert!(rendered.contains("ctrl+o"));
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

    #[test]
    fn an_edit_numbers_its_diff_with_the_lines_its_review_recorded() {
        let before = (1..=12)
            .map(|line| format!("line {line}\n"))
            .collect::<String>();
        let after = before
            .replace("line 2\n", "line two\n")
            .replace("line 11\n", "line eleven\n");
        let review = tickloop_harness::tools::file_change::build_file_change_review(
            tickloop_harness::protocol::FileChangeAction::Edit,
            PathBuf::from("/work/notes.txt"),
            &before,
            true,
            None,
            &after,
            true,
        );
        let input = json!({
            "file_path":"/work/notes.txt","old_string":"line 2","new_string":"line two"
        });
        let runs = archived_tool_result(
            EDIT_TOOL_NAME,
            Some(&input),
            &ToolResult {
                content: "The file /work/notes.txt has been updated successfully.".into(),
                is_error: false,
                output_ref: None,
            },
            Path::new("/work"),
            DisplayProfile::default(),
            &CallFacts {
                review: Some(review),
                report: None,
            },
        );
        assert_eq!(
            joined(&runs),
            [
                "  ⎿  Added 2 lines, removed 2 lines",
                "       1  line 1",
                "       2 -line 2",
                "       2 +line two",
                "       3  line 3",
                "       4  line 4",
                "       5  line 5",
                "     ...",
                "       8  line 8",
                "       9  line 9",
                "      10  line 10",
                "      11 -line 11",
                "      11 +line eleven",
                "      12  line 12",
                "",
            ]
            .join("\n"),
            "line numbers are as wide as the widest needs plus one, the marker sits on the code"
        );
        assert!(runs.contains(&("  2 ".into(), Some(Role::DiffLineRemoved))));
        assert!(runs.contains(&("+".into(), Some(Role::DiffMarkerAdded))));
        assert!(
            runs.iter()
                .any(|(text, role)| text == "two" && *role == Some(Role::DiffWordAdded)),
            "{runs:?}"
        );
    }

    #[test]
    fn calls_name_paths_relative_to_the_working_directory() {
        let cwd = Path::new("/work");
        let header = |name: &str, input: Value| {
            joined(&tool_call_roles(
                name,
                &input,
                cwd,
                DisplayProfile::default(),
                &super::super::tick_labels::TickLabels::default(),
            ))
        };
        assert_eq!(
            header(WRITE_TOOL_NAME, json!({"file_path":"/work/logs/build.log"})),
            "\n⏺ Write · logs/build.log\n"
        );
        assert_eq!(
            header("Grep", json!({"pattern":"TODO","path":"/work"})),
            "\n⏺ Grep · TODO in .\n"
        );
        assert_eq!(
            header(EDIT_TOOL_NAME, json!({"file_path":"/elsewhere/notes.txt"})),
            "\n⏺ Edit · /elsewhere/notes.txt\n",
            "a path outside the working directory stays whole"
        );
        assert_eq!(
            collapsed_call(READ_TOOL_NAME, &json!({"file_path":"/work/a.txt"})),
            Some(CollapsedCall::Read {
                path: "/work/a.txt".into()
            })
        );
        assert_eq!(
            collapsed_call("Glob", &json!({"pattern":"*.rs"})),
            Some(CollapsedCall::Search)
        );
        assert_eq!(collapsed_call(BASH_TOOL_NAME, &json!({})), None);
    }

    #[test]
    fn a_running_shell_shows_its_latest_lines_under_the_connector() {
        let display = DisplayProfile::default();
        assert_eq!(joined(&running_shell_runs("", display)), "  ⎿  Running…\n");
        let output = (1..=7)
            .map(|line| format!("line {line}\n"))
            .collect::<String>();
        assert_eq!(
            joined(&running_shell_runs(&output, display)),
            "  ⎿  line 3\n     line 4\n     line 5\n     line 6\n     line 7\n"
        );
    }

    #[test]
    fn a_finished_user_shell_reads_as_the_message_the_agent_records() {
        let record = |state: &str, exit_code: Option<i32>, reason: Option<&str>| {
            serde_json::from_value::<UserShellRecord>(json!({
                "id":Uuid::new_v4(),"chat_id":Uuid::new_v4(),"command":"make","cwd":"/work",
                "state":state,"pid":null,"exit_code":exit_code,"signal":null,"reason":reason,
                "output_ref":null,"output_bytes":9,"submitted_at_message":0
            }))
            .unwrap()
        };
        assert_eq!(
            user_shell_result(&record("Completed", Some(0), None), "one\ntwo\n\n"),
            LocalMessage::BashOutput {
                stdout: "one\ntwo".into(),
                stderr: String::new()
            }
        );
        assert_eq!(
            user_shell_result(&record("Failed", Some(2), None), "boom\n"),
            LocalMessage::BashOutput {
                stdout: String::new(),
                stderr: "boom\nExit code 2".into()
            }
        );
        assert_eq!(
            user_shell_result(
                &record(
                    "Failed",
                    None,
                    Some(tickloop_harness::tools::CANCELLED_COMMAND_REASON)
                ),
                "partial"
            ),
            LocalMessage::Interrupted {
                during_tool_use: false
            }
        );
    }
    #[test]
    fn scheduling_tool_results_use_compact_rows_and_preserve_failures() {
        let display = DisplayProfile::default();
        let value = json!({"id":"0123abcd", "schedule":"*/5 * * * *", "prompt":"weather", "next_run":"2026-10-23 12:05:00 CST"});
        let render = |name: &str, content: String, is_error| {
            let result = ToolResult {
                content,
                is_error,
                output_ref: None,
            };
            joined(&archived_tool_result(
                name,
                None,
                &result,
                Path::new(""),
                display,
                &CallFacts::default(),
            ))
        };
        assert!(render("CronCreate", value.to_string(), false).contains("next run 12:05"));
        let list = render(
            "CronList",
            json!({"tasks":[value.clone(),value]}).to_string(),
            false,
        );
        assert_eq!(list.matches("next run 12:05").count(), 2);
        let failure = render("CronCreate", "invalid cron".into(), true);
        assert!(failure.contains("Failed"));
        assert!(failure.contains("invalid cron"));
    }

    #[test]
    fn update_goal_completion_rejection_is_warning_with_compact_reason_and_full_details() {
        let reason = "The requested tests are missing. Full evidence and additional guidance must remain in details.";
        let result = ToolResult {
            content: format!(
                "The completion check did not accept the report, so the goal stays active. Its reason, to verify against the current state: {reason}"
            ),
            is_error: true,
            output_ref: None,
        };
        let runs = archived_tool_result(
            "UpdateGoal",
            None,
            &result,
            Path::new(""),
            DisplayProfile::default(),
            &CallFacts::default(),
        );
        let text = joined(&runs);
        assert!(text.contains("Not complete yet"), "{text}");
        assert!(!text.contains("Failed"));
        assert!(!text.contains("Full evidence"));
        assert!(
            runs.iter()
                .any(|(text, role)| text.contains("Not complete yet")
                    && *role == Some(Role::Warning))
        );
        let full = transcript_result_lines("UpdateGoal", None, &result.content, true, None, None);
        assert!(
            full.iter()
                .any(|line| joined(&line.spans).contains("Full evidence"))
        );
        assert!(result.is_error);
        for error in ["invalid UpdateGoal input: unknown status", "no active goal"] {
            let failure = ToolResult {
                content: error.into(),
                ..result.clone()
            };
            let runs = archived_tool_result(
                "UpdateGoal",
                None,
                &failure,
                Path::new(""),
                DisplayProfile::default(),
                &CallFacts::default(),
            );
            assert!(joined(&runs).contains("Failed"));
            assert!(runs.iter().any(|(_, role)| *role == Some(Role::Error)));
        }
    }

    #[test]
    fn update_goal_rejection_keeps_warning_call_color_live_and_in_both_history_sources() {
        let run = Uuid::new_v4();
        let started = event(
            run,
            EventKind::ToolStarted {
                call_id: "goal-check".into(),
                name: "UpdateGoal".into(),
                input: json!({"status":"complete","evidence":"report"}),
            },
        );
        let content = format!(
            "{GOAL_COMPLETION_REJECTION_PREFIX}{}。完整理由仅在详情中保留。",
            "还缺少完整的验证证据".repeat(40)
        );
        let result = ToolResult {
            content: content.clone(),
            is_error: true,
            output_ref: None,
        };
        let completed = event(
            run,
            EventKind::ToolCompleted {
                call_id: "goal-check".into(),
                result: result.clone(),
                sources: Vec::new(),
            },
        );
        let console = Console::test_interactive_for_chat(Uuid::nil());
        let mut presentation = ToolPresentation::new(false);
        presentation
            .present_event(&console, &started, &CallFacts::default())
            .unwrap();
        presentation
            .present_event(&console, &completed, &CallFacts::default())
            .unwrap();
        let rows = console.test_conversation_rows();
        let header = rows
            .iter()
            .find(|line| line.text().contains("UpdateGoal"))
            .unwrap();
        assert!(
            header
                .spans
                .iter()
                .any(|span| span.text.contains('⏺') && span.role == Some(Role::Warning)),
            "{header:?}"
        );
        assert!(
            rows.iter()
                .all(|line| line.spans.iter().all(|span| span.role != Some(Role::Error)))
        );
        assert_eq!(
            rows.iter()
                .filter(|line| line.text().contains("Reason:"))
                .count(),
            1
        );
        assert!(console.test_conversation_text().contains('…'));
        let details = presentation.toggle();
        assert!(details.contains(&content));
        assert!(details.contains("Not complete yet"));
        let mut snapshot: ChatSnapshot = serde_json::from_value(json!({
            "id": Uuid::nil(), "folder":"/fixture", "messages":[],
            "chat_loop":{"id":Uuid::nil(), "state":"Idle"}
        }))
        .unwrap();
        snapshot.events = vec![started.clone(), completed];
        assert_eq!(
            tool_call_states(&snapshot)[&(run, "goal-check".into())],
            ToolCallState::Warning
        );
        snapshot.events.clear();
        snapshot.messages = serde_json::from_value(json!([
            {"role":"assistant","content":[{"type":"tool_use","id":"goal-check","name":"UpdateGoal","input":{"status":"complete"}}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"goal-check","content":content,"is_error":true}]}
        ])).unwrap();
        snapshot.message_runs = [(0, run.into()), (1, run.into())].into();
        assert_eq!(
            tool_call_states(&snapshot)[&(run, "goal-check".into())],
            ToolCallState::Warning
        );
        assert!(transcript_text(&snapshot).contains("完整理由仅在详情中保留"));
        let display = DisplayProfile::default();
        let runs = archived_tool_result(
            "UpdateGoal",
            None,
            &result,
            Path::new(""),
            display,
            &CallFacts::default(),
        );
        let mut scrollback = super::super::scrollback::Scrollback::new();
        scrollback.append_tool_result("goal-check", &runs, true);
        let frame = scrollback.frame(30, 100);
        assert_eq!(
            frame
                .lines
                .iter()
                .filter(|line| line.contains("还缺少"))
                .count(),
            1
        );
        assert!(frame.lines.iter().any(|line| line.ends_with('…')));
    }
}

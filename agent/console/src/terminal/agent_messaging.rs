//! Agent communication in the conversation, as Claude Code draws its teammates: a Subagent call
//! names its child in the child's Agent Color, a finished child and the report it delivered read
//! as one `Teammate @handle finished` block, and any other Agent message folds into one dim
//! `› Message from @handle` line. Ctrl+O shows each of them in full.

use std::collections::{HashMap, HashSet};

use serde_json::Value;
use tickloop_agent_protocol::{
    AgentColor, AgentSummary, ChatSnapshot, Content, FamilyCompletion, LocalMessage, LoopState,
    SUBAGENT_MISSING_REPORT, SUBAGENT_REPORT_RECEIPT, SubagentNotificationDelivery,
    SubagentReportMessage, VerifiedUserRelay,
};
use uuid::Uuid;

use super::{
    display::{DisplayProfile, Glyph},
    presentation::{OutputDetail, RoleRuns, clipped, compact_duration, tool_name},
    safe_line, safe_text,
    style::Role,
};

/// The Agent Color of every Agent a View has learned of, by Chat; `None` for one without a
/// colour, such as a root Agent. A colour never changes once assigned.
pub(super) type AgentColors = HashMap<Uuid, Option<AgentColor>>;

/// Report lines a finished child's block shows before Ctrl+O.
const REPORT_PREVIEW_LINES: usize = 2;
/// Characters of one previewed line before the rest is left to Ctrl+O.
const PREVIEW_LINE_CHARACTERS: usize = 160;
/// Characters of a failed child's reason in its heading before Ctrl+O, as Claude Code's.
const FAILURE_PREVIEW_CHARACTERS: usize = 200;
/// Characters of a sender's handle in a folded message line, as Claude Code's.
const MESSAGE_HANDLE_CHARACTERS: usize = 20;
/// Characters of a Subagent call's description in its row.
const DESCRIPTION_CHARACTERS: usize = 80;
const EXPAND_HINT: &str = "(ctrl+o to expand)";
/// Claude Code's result of an Agent call that went to the background.
pub(super) const BACKGROUNDED: &str = "Backgrounded agent";
const ENDED_WITHOUT_REPORT: &str = "Ended without a report";
/// Claude Code's name for a named family member: a child here has a handle and an Agent Color,
/// takes messages and can be continued, as a Claude Code teammate.
const TEAMMATE: &str = "Teammate";
const MESSAGE_FROM: &str = "Message from";
/// A block's body under its heading, as Claude Code pads a notification's or message's body.
const BODY_INDENT: &str = "  ";

/// Agent Colors as the Agent catalog reports them.
pub(super) fn catalog_colors(rows: &[AgentSummary]) -> AgentColors {
    rows.iter()
        .map(|row| {
            (
                row.chat_id,
                row.subagent
                    .as_ref()
                    .and_then(|registration| registration.color),
            )
        })
        .collect()
}

/// Every child `snapshot`'s conversation names in its Agent Color: the children it delegated to
/// and the ones whose completions reached it.
pub(super) fn named_agents(snapshot: &ChatSnapshot) -> HashSet<Uuid> {
    let mut agents = snapshot
        .family_completions
        .iter()
        .map(|completion| completion.child_chat_id)
        .collect::<HashSet<_>>();
    agents.extend(
        snapshot
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(|content| match content {
                Content::ToolResult { content, .. } => launched_child(&content.text()),
                _ => None,
            }),
    );
    agents
}

/// The child a Subagent result names.
pub(super) fn launched_child(result: &str) -> Option<Uuid> {
    if !result.contains("\"child_chat_id\"") {
        return None;
    }
    serde_json::from_str::<Value>(result).ok()?["child_chat_id"]
        .as_str()?
        .parse()
        .ok()
}

fn agent_role(colors: &AgentColors, chat: Option<Uuid>) -> Option<Role> {
    colors.get(&chat?).copied().flatten().map(Role::agent)
}

/// `⏺ Subagent(@handle · description)`: the child's handle in its Agent Color and its task. Until
/// the call's result names the child, the handle is the one the call asked for.
pub(super) fn subagent_call_runs(
    input: &Value,
    launch: Option<&Value>,
    colors: &AgentColors,
    display: DisplayProfile,
) -> RoleRuns {
    let (handle, description) = subagent_identity(input, launch);
    let child = launch.and_then(|launch| launch["child_chat_id"].as_str()?.parse().ok());
    let mut runs = vec![
        ("\n".into(), None),
        (
            format!("{} ", display.glyph(Glyph::ToolCall)),
            Some(Role::MutedText),
        ),
        (
            tool_name(tickloop_agent_protocol::SUBAGENT_TOOL_NAME),
            Some(Role::ToolName),
        ),
        ("(".into(), None),
    ];
    if let Some(handle) = handle {
        runs.push((format!("@{handle}"), agent_role(colors, child)));
        runs.push((" · ".into(), None));
    }
    runs.push((description, None));
    runs.push((")".into(), None));
    runs.push(("\n".into(), None));
    runs
}

/// `@handle · description`, as the detailed transcript names a Subagent call.
pub(super) fn subagent_call_label(input: &Value, launch: Option<&Value>) -> String {
    match subagent_identity(input, launch) {
        (Some(handle), description) => format!("@{handle} · {description}"),
        (None, description) => description,
    }
}

fn subagent_identity(input: &Value, launch: Option<&Value>) -> (Option<String>, String) {
    let field = |key: &str| {
        launch
            .and_then(|launch| launch[key].as_str())
            .or_else(|| input[key].as_str())
            .filter(|text| !text.trim().is_empty())
    };
    let handle = launch
        .and_then(|launch| launch["handle"].as_str())
        .or_else(|| input["name"].as_str())
        .filter(|handle| !handle.trim().is_empty())
        .map(safe_line);
    let description = field("description")
        .or_else(|| input["task"].as_str())
        .unwrap_or_default();
    let description = clipped(
        &description.split_whitespace().collect::<Vec<_>>().join(" "),
        DESCRIPTION_CHARACTERS,
    )
    .0;
    (handle, description)
}

/// What a finished child reported to its caller: the report it delivered as an Agent message,
/// or the reply it ended with when no report was asked of it. `delivered` finds the report.
fn reported(
    result: &str,
    delivered: impl FnOnce() -> Option<SubagentReportMessage>,
) -> Option<SubagentReportMessage> {
    match result {
        SUBAGENT_REPORT_RECEIPT => delivered(),
        SUBAGENT_MISSING_REPORT => None,
        text if text.trim().is_empty() => None,
        text => Some(SubagentReportMessage {
            note: None,
            report: text.to_owned(),
        }),
    }
}

/// The report the child of run `child_run` delivered to `snapshot`'s Agent, whether the Agent
/// already took it in or it still waits in the input queue.
pub(super) fn delivered_report(
    snapshot: &ChatSnapshot,
    child_run: Uuid,
) -> Option<SubagentReportMessage> {
    let admitted = snapshot
        .message_ids
        .iter()
        .filter(|(_, id)| **id == child_run)
        .filter_map(|(index, _)| snapshot.messages.get(*index))
        .flat_map(|message| &message.content)
        .filter_map(|content| match content {
            Content::Text { text } => Some(text.as_str()),
            _ => None,
        });
    let queued = snapshot
        .pending_inputs
        .iter()
        .filter(|pending| {
            pending
                .agent_message
                .as_ref()
                .is_some_and(|source| source.message_id == child_run)
        })
        .map(|pending| pending.input.as_str());
    admitted
        .chain(queued)
        .find_map(|text| match LocalMessage::parse(text)? {
            LocalMessage::AgentCommunication {
                message,
                is_result: true,
                ..
            } => Some(
                SubagentReportMessage::parse(&message).unwrap_or(SubagentReportMessage {
                    note: None,
                    report: message,
                }),
            ),
            _ => None,
        })
}

/// The report a Subagent call's result stands for, read from `snapshot`.
pub(super) fn subagent_report(
    snapshot: &ChatSnapshot,
    launch: &Value,
) -> Option<SubagentReportMessage> {
    let child_run = launch["child_run_id"]
        .as_str()
        .and_then(|run| run.parse().ok());
    reported(launch["result"].as_str().unwrap_or_default(), || {
        delivered_report(snapshot, child_run?)
    })
}

/// A Subagent call's result under its row: Claude Code's `Backgrounded agent` for a child that
/// went to the background, the first lines of a foreground child's report, or how the child
/// failed.
pub(super) fn subagent_result_runs(
    launch: &Value,
    is_error: bool,
    report: Option<&SubagentReportMessage>,
    glyph: &str,
) -> RoleRuns {
    let mut rows = Rows::new(glyph);
    let status = launch["status"].as_str();
    if status == Some("async_launched") {
        rows.row(vec![
            (BACKGROUNDED.into(), None),
            (format!(" {EXPAND_HINT}"), Some(Role::MutedText)),
        ]);
        return rows.runs;
    }
    if status == Some("completed") && !is_error {
        match report {
            Some(report) => rows.report(report),
            None => rows.row(vec![(ENDED_WITHOUT_REPORT.into(), Some(Role::MutedText))]),
        }
        return rows.runs;
    }
    let label = match status {
        Some("interrupted") => "Interrupted",
        Some("stopped") => "Stopped",
        _ => "Failed",
    };
    let error = launch["error"].as_str().unwrap_or_default();
    let text = if error.is_empty() {
        label.to_owned()
    } else {
        format!("{label}: {error}")
    };
    rows.lines(&text, Some(Role::Error));
    rows.runs
}

/// What an Agent message in a conversation is.
pub(super) enum AgentInput<'a> {
    /// A child's report or completion notice. A background child's shows as its completion
    /// block; a foreground child's is already under its call.
    Result(&'a FamilyCompletion),
    /// A child's report whose run has not ended yet; its completion block shows it once it has.
    PendingResult,
    Message,
}

/// What the Agent message `message_id` of `snapshot`, reading as `local`, is.
pub(super) fn agent_input<'a>(
    snapshot: &'a ChatSnapshot,
    message_id: Option<Uuid>,
    local: &LocalMessage,
) -> AgentInput<'a> {
    let is_result = match local {
        LocalMessage::AgentCommunication { is_result, .. } => *is_result,
        LocalMessage::TaskNotification { .. } => true,
        _ => false,
    };
    let completion = message_id.and_then(|id| {
        snapshot
            .family_completions
            .iter()
            .find(|completion| completion.child_run_id == id)
    });
    match completion {
        Some(completion) => AgentInput::Result(completion),
        None if is_result => AgentInput::PendingResult,
        None => AgentInput::Message,
    }
}

/// The Agent inputs of `snapshot` its saved history shows: every Agent message and every child's
/// completion, except a report whose run has not ended yet, which its completion shows later.
pub(super) fn shown_agent_inputs(snapshot: &ChatSnapshot) -> impl Iterator<Item = Uuid> + '_ {
    snapshot
        .agent_message_origins
        .iter()
        .filter(|(id, source)| source["is_result"] != true || has_completion(snapshot, **id))
        .map(|(id, _)| *id)
        .chain(
            snapshot
                .family_completions
                .iter()
                .map(|completion| completion.child_run_id),
        )
}

fn has_completion(snapshot: &ChatSnapshot, child_run: Uuid) -> bool {
    snapshot
        .family_completions
        .iter()
        .any(|completion| completion.child_run_id == child_run)
}

/// A finished child as Claude Code draws a teammate that finished: `⏺ Teammate @handle finished`
/// with the dot in its outcome's colour and the handle in its Agent Color, a failure's reason and
/// the run's time dim behind it, then the report indented under it: its first lines dim, the last
/// ending in `…` when more is left to Ctrl+O, or the whole report in full.
pub(super) fn completion_runs(
    completion: &FamilyCompletion,
    snapshot: &ChatSnapshot,
    colors: &AgentColors,
    detail: OutputDetail,
    display: DisplayProfile,
) -> (RoleRuns, RoleRuns) {
    let (word, dot) = match completion.state {
        LoopState::Failed => ("failed", Role::Error),
        LoopState::Interrupted => ("was interrupted", Role::Warning),
        _ => ("finished", Role::Success),
    };
    let mut heading = vec![
        ("\n".into(), None),
        (display.glyph(Glyph::ToolCall).into(), Some(dot)),
        (format!(" {TEAMMATE} "), None),
        (
            format!("@{}", safe_line(&completion.handle)),
            agent_role(colors, Some(completion.child_chat_id)),
        ),
        (format!(" {word}"), None),
    ];
    if let Some(error) = &completion.error {
        let reason = safe_text(&error.split_whitespace().collect::<Vec<_>>().join(" "));
        let reason = match detail {
            OutputDetail::Preview => clipped(&reason, FAILURE_PREVIEW_CHARACTERS).0,
            OutputDetail::Full => reason,
        };
        heading.push((format!(": {reason}"), Some(Role::MutedText)));
    }
    if let Some(started) = completion.started_at {
        heading.push((
            format!(
                " · {}",
                compact_duration(completion.finished_at.saturating_sub(started))
            ),
            Some(Role::MutedText),
        ));
    }
    heading.push(("\n".into(), None));
    let mut body = Body::default();
    let report = reported(&completion.result, || {
        delivered_report(snapshot, completion.child_run_id)
    });
    match &report {
        Some(report) => body.report(report, detail),
        None if completion.error.is_none() && completion.result == SUBAGENT_MISSING_REPORT => {
            body.row(vec![(ENDED_WITHOUT_REPORT.into(), Some(Role::MutedText))]);
        }
        None => {}
    }
    if let SubagentNotificationDelivery::Rejected(rejection) = &completion.notification {
        body.row(vec![(
            format!(
                "Notification not delivered: {}",
                safe_line(&rejection.message)
            ),
            Some(Role::MutedText),
        )]);
    }
    (heading, body.runs)
}

/// Any other Agent message, as Claude Code folds a teammate's message: one dim
/// `› Message from @handle (ctrl+o to expand)` line, a long handle cut short. In full, the heading
/// names the whole handle, and any verified user relay and the whole message sit under it.
pub(super) fn message_runs(
    message: &AgentMessage<'_>,
    detail: OutputDetail,
    display: DisplayProfile,
) -> (RoleRuns, RoleRuns) {
    let mark = display.glyph(Glyph::AgentMessage);
    let handle = safe_line(message.sender_handle);
    if matches!(detail, OutputDetail::Preview) {
        let handle = clipped(&handle, MESSAGE_HANDLE_CHARACTERS).0;
        let heading = vec![
            ("\n".into(), None),
            (
                format!("{mark} {MESSAGE_FROM} @{handle} {EXPAND_HINT}\n"),
                Some(Role::MutedText),
            ),
        ];
        return (heading, RoleRuns::new());
    }
    let heading = vec![
        ("\n".into(), None),
        (
            format!("{mark} {MESSAGE_FROM} @{handle}\n"),
            Some(Role::MutedText),
        ),
    ];
    let mut body = Body::default();
    if let Some(relay) = message.relay {
        body.row(vec![(
            format!(
                "Verified user relay · {}",
                if relay.forwarded_verbatim {
                    "verbatim"
                } else {
                    "paraphrased"
                }
            ),
            Some(Role::MutedText),
        )]);
        body.lines(&relay.original_text, Some(Role::MutedText));
    }
    body.lines(message.text, None);
    (heading, body.runs)
}

/// An Agent message as [`message_runs`] reads it.
pub(super) struct AgentMessage<'a> {
    pub(super) sender_handle: &'a str,
    pub(super) relay: Option<&'a VerifiedUserRelay>,
    pub(super) text: &'a str,
}

impl<'a> AgentMessage<'a> {
    /// `local` as an Agent message; `None` for another local message.
    pub(super) fn of(local: &'a LocalMessage) -> Option<Self> {
        let LocalMessage::AgentCommunication {
            sender_handle,
            relay,
            message,
            ..
        } = local
        else {
            return None;
        };
        Some(Self {
            sender_handle,
            relay: relay.as_ref(),
            text: message,
        })
    }
}

/// Rows under a call's `⎿`, the first behind the connector and the rest under its content.
struct Rows<'a> {
    runs: RoleRuns,
    glyph: &'a str,
}

impl<'a> Rows<'a> {
    fn new(glyph: &'a str) -> Self {
        Self {
            runs: RoleRuns::new(),
            glyph,
        }
    }

    fn row(&mut self, spans: RoleRuns) {
        if self.runs.is_empty() {
            self.runs.push(("  ".into(), None));
            self.runs.push((self.glyph.into(), Some(Role::MutedText)));
            self.runs.push(("  ".into(), None));
        } else {
            self.runs.push(("     ".into(), None));
        }
        self.runs.extend(spans);
        self.runs.push(("\n".into(), None));
    }

    fn lines(&mut self, text: &str, role: Option<Role>) {
        for line in safe_text(text).lines() {
            self.row(vec![(clipped(line, PREVIEW_LINE_CHARACTERS).0, role)]);
        }
    }

    /// A foreground child's report under its call: its note in the warning colour, then its first
    /// lines with the expand hint after the last when any were left out.
    fn report(&mut self, report: &SubagentReportMessage) {
        if let Some(note) = &report.note {
            self.lines(note, Some(Role::Warning));
        }
        let text = safe_text(report.report.trim_end());
        let mut lines = text.lines().skip_while(|line| line.trim().is_empty());
        let mut shown = Vec::new();
        let mut truncated = false;
        for line in lines.by_ref().take(REPORT_PREVIEW_LINES) {
            let (line, clipped) = clipped(line, PREVIEW_LINE_CHARACTERS);
            truncated |= clipped;
            shown.push(line);
        }
        truncated |= lines.next().is_some();
        let last = shown.len().saturating_sub(1);
        for (index, line) in shown.into_iter().enumerate() {
            let mut spans = vec![(line, None)];
            if truncated && index == last {
                spans.push((format!(" {EXPAND_HINT}"), Some(Role::MutedText)));
            }
            self.row(spans);
        }
    }
}

/// Rows indented under a block's heading with no connector, as Claude Code pads the body of a
/// teammate's notification or message.
#[derive(Default)]
struct Body {
    runs: RoleRuns,
}

impl Body {
    fn row(&mut self, spans: RoleRuns) {
        self.runs.push((BODY_INDENT.into(), None));
        self.runs.extend(spans);
        self.runs.push(("\n".into(), None));
    }

    fn lines(&mut self, text: &str, role: Option<Role>) {
        for line in safe_text(text).lines() {
            self.row(vec![(line.to_owned(), role)]);
        }
    }

    /// A report's note in the warning colour, then its lines: all of them in full, otherwise its
    /// first lines dim, the last ending in `…` when more of the report is left to Ctrl+O.
    fn report(&mut self, report: &SubagentReportMessage, detail: OutputDetail) {
        let text = safe_text(report.report.trim_end());
        if matches!(detail, OutputDetail::Full) {
            if let Some(note) = &report.note {
                self.lines(note, Some(Role::Warning));
            }
            for line in text.lines().skip_while(|line| line.trim().is_empty()) {
                self.row(vec![(line.to_owned(), None)]);
            }
            return;
        }
        if let Some(note) = &report.note {
            for line in safe_text(note).lines() {
                self.row(vec![(
                    clipped(line, PREVIEW_LINE_CHARACTERS).0,
                    Some(Role::Warning),
                )]);
            }
        }
        let mut lines = text.lines().filter(|line| !line.trim().is_empty());
        let shown = lines
            .by_ref()
            .take(REPORT_PREVIEW_LINES)
            .map(|line| clipped(line, PREVIEW_LINE_CHARACTERS))
            .collect::<Vec<_>>();
        let has_more = lines.next().is_some();
        let last = shown.len().saturating_sub(1);
        for (index, (mut line, was_clipped)) in shown.into_iter().enumerate() {
            if has_more && index == last && !was_clipped {
                line.push('…');
            }
            self.row(vec![(line, Some(Role::MutedText))]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::presentation::joined;
    use serde_json::json;
    use tickloop_agent_protocol::{FamilyMessageRejection, FamilyMessageRejectionCode};

    fn completion(state: LoopState, result: &str, error: Option<&str>) -> FamilyCompletion {
        FamilyCompletion {
            child_chat_id: Uuid::new_v4(),
            child_loop_id: Uuid::new_v4(),
            child_run_id: Uuid::new_v4(),
            handle: "audit".into(),
            state,
            result: result.into(),
            error: error.map(str::to_owned),
            started_at: Some(1_000),
            finished_at: 64_000,
            triggering_message_ids: Vec::new(),
            notification: SubagentNotificationDelivery::Queued,
            usage: Default::default(),
            run_status: None,
        }
    }

    /// A parent conversation holding the report the child of `child_run` delivered.
    fn delivered(child_run: Uuid, report: &SubagentReportMessage) -> ChatSnapshot {
        let message = format!(
            "<agent-message>{}</agent-message>",
            json!({"source":{"message_id":child_run,"sender_handle":"audit","sender_label":"audit","is_result":true},"message":report.text()})
        );
        serde_json::from_value(json!({
            "id":Uuid::new_v4(),"folder":"/work","messages":[
                {"role":"user","content":[{"type":"text","text":message}]}
            ],"chat_loop":{"id":"00000000-0000-0000-0000-000000000000","state":"Idle"},"message_ids":{"0":child_run},"agent_message_origins":{child_run.to_string():{"is_result":true}}
        }))
        .unwrap()
    }

    fn block(
        completion: &FamilyCompletion,
        snapshot: &ChatSnapshot,
        detail: OutputDetail,
    ) -> (RoleRuns, RoleRuns) {
        completion_runs(
            completion,
            snapshot,
            &AgentColors::default(),
            detail,
            DisplayProfile::default(),
        )
    }

    #[test]
    fn a_finished_child_reads_as_a_teammate_block_in_its_outcome_colour() {
        let mut finished = completion(LoopState::Idle, SUBAGENT_REPORT_RECEIPT, None);
        let warning = "SECURITY WARNING: auto review flagged this subagent's report.";
        let report = SubagentReportMessage {
            note: Some(warning.into()),
            report: "First finding.\n\nSecond finding.\nThird finding.".into(),
        };
        let snapshot = delivered(finished.child_run_id, &report);
        let colors = AgentColors::from([(finished.child_chat_id, Some(AgentColor::Cyan))]);
        let (heading, body) = completion_runs(
            &finished,
            &snapshot,
            &colors,
            OutputDetail::Preview,
            DisplayProfile::default(),
        );
        assert_eq!(joined(&heading), "\n⏺ Teammate @audit finished · 1m 3s\n");
        assert_eq!(heading[1].1, Some(Role::Success));
        assert!(heading.contains(&("@audit".into(), Some(Role::agent(AgentColor::Cyan)))));
        assert!(heading.contains(&(" · 1m 3s".into(), Some(Role::MutedText))));
        assert_eq!(
            joined(&body),
            format!("  {warning}\n  First finding.\n  Second finding.…\n")
        );
        assert!(body.contains(&(warning.into(), Some(Role::Warning))));
        assert!(body.contains(&("Second finding.…".into(), Some(Role::MutedText))));
        let (_, full) = block(&finished, &snapshot, OutputDetail::Full);
        assert_eq!(
            joined(&full),
            format!("  {warning}\n  First finding.\n  \n  Second finding.\n  Third finding.\n")
        );
        assert!(full.contains(&("Third finding.".into(), None)));

        finished.result = "Only line.".into();
        let (_, whole) = block(&finished, &snapshot, OutputDetail::Preview);
        assert_eq!(
            joined(&whole),
            "  Only line.\n",
            "nothing left out, no ellipsis"
        );

        for (state, word, dot) in [
            (LoopState::Failed, "failed", Role::Error),
            (LoopState::Interrupted, "was interrupted", Role::Warning),
        ] {
            let ended = completion(state, SUBAGENT_MISSING_REPORT, Some("Model\nunavailable"));
            let (heading, body) = block(&ended, &snapshot, OutputDetail::Preview);
            assert_eq!(
                joined(&heading),
                format!("\n⏺ Teammate @audit {word}: Model unavailable · 1m 3s\n")
            );
            assert_eq!(heading[1].1, Some(dot));
            assert!(heading.contains(&(": Model unavailable".into(), Some(Role::MutedText))));
            assert!(body.is_empty(), "the reason stands in the heading");
        }
    }

    #[test]
    fn a_child_without_a_report_says_so_and_a_refused_notice_stays_visible() {
        let other = SubagentReportMessage {
            note: None,
            report: "Another child's report".into(),
        };
        let snapshot = delivered(Uuid::new_v4(), &other);
        let silent = completion(LoopState::Idle, SUBAGENT_MISSING_REPORT, None);
        let (_, body) = block(&silent, &snapshot, OutputDetail::Preview);
        assert_eq!(joined(&body), "  Ended without a report\n");

        let mut refused = completion(LoopState::Idle, "Plain reply text", None);
        refused.started_at = None;
        refused.notification = SubagentNotificationDelivery::Rejected(FamilyMessageRejection {
            code: FamilyMessageRejectionCode::StaleTarget,
            message: "The receiving parent is unavailable".into(),
        });
        let (heading, body) = block(&refused, &snapshot, OutputDetail::Preview);
        assert_eq!(joined(&heading), "\n⏺ Teammate @audit finished\n");
        assert_eq!(
            joined(&body),
            "  Plain reply text\n  Notification not delivered: The receiving parent is unavailable\n"
        );
    }

    #[test]
    fn another_agent_message_folds_into_one_dim_line_and_reads_in_full() {
        let relay = VerifiedUserRelay {
            original_chat_id: Uuid::new_v4(),
            original_message_id: Uuid::new_v4(),
            original_text: "Human instruction".into(),
            forwarded_verbatim: true,
        };
        let message = AgentMessage {
            sender_handle: "feature-dev-code-reviewer",
            relay: Some(&relay),
            text: "First line\nSecond line",
        };
        let display = DisplayProfile::default();
        let (heading, body) = message_runs(&message, OutputDetail::Preview, display);
        assert_eq!(
            joined(&heading),
            "\n› Message from @feature-dev-code-rev… (ctrl+o to expand)\n"
        );
        assert!(
            heading
                .iter()
                .all(|(text, role)| text == "\n" || *role == Some(Role::MutedText)),
            "the whole line is dim"
        );
        assert!(body.is_empty());

        let (heading, body) = message_runs(&message, OutputDetail::Full, display);
        assert_eq!(
            joined(&heading),
            "\n› Message from @feature-dev-code-reviewer\n"
        );
        assert_eq!(
            joined(&body),
            "  Verified user relay · verbatim\n  Human instruction\n  First line\n  Second line\n"
        );
        assert!(body.contains(&("Second line".into(), None)));
    }

    #[test]
    fn a_subagent_call_names_the_handle_it_asked_for_until_its_result_names_the_child() {
        let child = Uuid::new_v4();
        let colors = AgentColors::from([(child, Some(AgentColor::Cyan))]);
        let display = DisplayProfile::default();
        let input = json!({"task":"Audit the bounded evidence files","name":"audit","description":"Audit evidence"});
        let asked = subagent_call_runs(&input, None, &colors, display);
        assert_eq!(joined(&asked), "\n⏺ Subagent(@audit · Audit evidence)\n");
        assert!(asked.contains(&("@audit".into(), None)));

        let launch = json!({"status":"async_launched","handle":"audit-2","description":"Audit evidence","child_chat_id":child});
        let named = subagent_call_runs(&input, Some(&launch), &colors, display);
        assert_eq!(joined(&named), "\n⏺ Subagent(@audit-2 · Audit evidence)\n");
        assert!(named.contains(&("@audit-2".into(), Some(Role::agent(AgentColor::Cyan)))));
        let backgrounded = subagent_result_runs(&launch, false, None, "⎿");
        assert_eq!(
            joined(&backgrounded),
            "  ⎿  Backgrounded agent (ctrl+o to expand)\n"
        );
        assert!(backgrounded.contains(&("Backgrounded agent".into(), None)));

        let unnamed =
            subagent_call_runs(&json!({"task":"Read\nthe   notes"}), None, &colors, display);
        assert_eq!(joined(&unnamed), "\n⏺ Subagent(Read the notes)\n");
        let failed = json!({"status":"failed","error":"Model unavailable"});
        let rows = subagent_result_runs(&failed, true, None, "⎿");
        assert_eq!(joined(&rows), "  ⎿  Failed: Model unavailable\n");
        assert_eq!(rows[3].1, Some(Role::Error));
    }
}

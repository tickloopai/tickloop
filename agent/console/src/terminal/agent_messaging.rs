//! Agent communication in the conversation, as Claude Code draws an Agent call, a teammate's
//! message and an Agent notification: a Subagent call names its child in the child's Agent
//! Color, a finished child and the report it delivered read as one block, and any other Agent
//! message reads as one line. Ctrl+O shows each of them in full.

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
/// Characters of a Subagent call's description in its row.
const DESCRIPTION_CHARACTERS: usize = 80;
const EXPAND_HINT: &str = "(ctrl+o to expand)";
const STARTED_IN_BACKGROUND: &str = "Started in background";
const ENDED_WITHOUT_REPORT: &str = "Ended without a report";
/// Between an Agent message's handle and its preview, as wide as Claude Code's gap.
const MESSAGE_GAP: &str = "  ";

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

/// Every other Agent `snapshot`'s conversation names: the children it delegated to and the
/// members whose messages or completions reached it.
pub(super) fn named_agents(snapshot: &ChatSnapshot) -> HashSet<Uuid> {
    let mut agents = snapshot
        .family_completions
        .iter()
        .map(|completion| completion.child_chat_id)
        .collect::<HashSet<_>>();
    agents.extend(
        snapshot
            .agent_message_origins
            .values()
            .filter_map(|source| source["sender_chat_id"].as_str()?.parse::<Uuid>().ok()),
    );
    agents.extend(
        snapshot
            .pending_inputs
            .iter()
            .filter_map(|pending| Some(pending.agent_message.as_ref()?.sender_chat_id)),
    );
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

/// A Subagent call's result under its row: that the child started in the background, the first
/// lines of a foreground child's report, or how the child failed.
pub(super) fn subagent_result_runs(
    launch: &Value,
    is_error: bool,
    report: Option<&SubagentReportMessage>,
    glyph: &str,
) -> RoleRuns {
    let mut rows = Rows::new(glyph);
    let status = launch["status"].as_str();
    if status == Some("async_launched") {
        rows.row(vec![(STARTED_IN_BACKGROUND.into(), Some(Role::MutedText))]);
        return rows.runs;
    }
    if status == Some("completed") && !is_error {
        match report {
            Some(report) => rows.report(report, OutputDetail::Preview),
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
    rows.lines(&text, Some(Role::Error), OutputDetail::Preview);
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

/// A finished child as one block: `⏺ @handle finished · <elapsed>` with the dot in its outcome's
/// colour, then `⎿` with the first lines of its report. Ctrl+O shows the whole report.
pub(super) fn completion_runs(
    completion: &FamilyCompletion,
    snapshot: &ChatSnapshot,
    colors: &AgentColors,
    detail: OutputDetail,
    display: DisplayProfile,
) -> (RoleRuns, RoleRuns) {
    let (word, dot) = match completion.state {
        LoopState::Failed => ("failed", Role::Error),
        LoopState::Interrupted => ("interrupted", Role::Warning),
        LoopState::Stopped | LoopState::Expired => ("stopped", Role::Warning),
        _ => ("finished", Role::Success),
    };
    let elapsed = completion
        .started_at
        .map(|started| {
            format!(
                " · {}",
                compact_duration(completion.finished_at.saturating_sub(started))
            )
        })
        .unwrap_or_default();
    let heading = vec![
        ("\n".into(), None),
        (display.glyph(Glyph::ToolCall).into(), Some(dot)),
        (" ".into(), None),
        (
            format!("@{}", safe_line(&completion.handle)),
            agent_role(colors, Some(completion.child_chat_id)),
        ),
        (format!(" {word}{elapsed}\n"), None),
    ];
    let mut rows = Rows::new(display.glyph(Glyph::ToolResult));
    let report = reported(&completion.result, || {
        delivered_report(snapshot, completion.child_run_id)
    });
    match &report {
        Some(report) => rows.report(report, detail),
        None if completion.error.is_none() && completion.result == SUBAGENT_MISSING_REPORT => {
            rows.row(vec![(ENDED_WITHOUT_REPORT.into(), Some(Role::MutedText))]);
        }
        None => {}
    }
    if let Some(error) = &completion.error {
        rows.lines(error, Some(Role::Error), detail);
    }
    if let SubagentNotificationDelivery::Rejected(rejection) = &completion.notification {
        rows.row(vec![(
            format!(
                "Notification not delivered: {}",
                safe_line(&rejection.message)
            ),
            Some(Role::MutedText),
        )]);
    }
    (heading, rows.runs)
}

/// Any other Agent message: `✉ @handle  <first line>  (ctrl+o to expand)`. In full, the
/// sender's label and any verified user relay head the whole message under a `⎿`.
pub(super) fn message_runs(
    message: &AgentMessage<'_>,
    colors: &AgentColors,
    detail: OutputDetail,
    display: DisplayProfile,
) -> (RoleRuns, RoleRuns) {
    let mut heading = vec![
        ("\n".into(), None),
        (
            display.glyph(Glyph::AgentMessage).into(),
            Some(Role::MutedText),
        ),
        (" ".into(), None),
        (
            format!("@{}", safe_line(message.sender_handle)),
            agent_role(colors, message.sender_chat),
        ),
    ];
    let body = safe_text(message.text);
    if matches!(detail, OutputDetail::Preview) {
        let mut lines = body.lines().filter(|line| !line.trim().is_empty());
        let (first, clipped) = clipped(
            lines.next().unwrap_or_default().trim(),
            PREVIEW_LINE_CHARACTERS,
        );
        heading.push((format!("{MESSAGE_GAP}{first}"), None));
        if clipped || lines.next().is_some() || message.relay.is_some() {
            heading.push((format!("{MESSAGE_GAP}{EXPAND_HINT}"), Some(Role::MutedText)));
        }
        heading.push(("\n".into(), None));
        return (heading, RoleRuns::new());
    }
    if !message.sender_label.is_empty() {
        heading.push((
            format!(" · {}", safe_line(message.sender_label)),
            Some(Role::MutedText),
        ));
    }
    heading.push(("\n".into(), None));
    let mut rows = Rows::new(display.glyph(Glyph::ToolResult));
    if let Some(relay) = message.relay {
        rows.row(vec![(
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
        rows.lines(&relay.original_text, Some(Role::MutedText), detail);
    }
    rows.lines(&body, None, detail);
    (heading, rows.runs)
}

/// An Agent message as [`message_runs`] reads it.
pub(super) struct AgentMessage<'a> {
    pub(super) sender_handle: &'a str,
    pub(super) sender_label: &'a str,
    pub(super) sender_chat: Option<Uuid>,
    pub(super) relay: Option<&'a VerifiedUserRelay>,
    pub(super) text: &'a str,
}

impl<'a> AgentMessage<'a> {
    /// `local` as the Agent message `message_id` of `snapshot`; `None` for another local message.
    pub(super) fn of(
        local: &'a LocalMessage,
        snapshot: &ChatSnapshot,
        message_id: Option<Uuid>,
    ) -> Option<Self> {
        let LocalMessage::AgentCommunication {
            sender_handle,
            sender_label,
            relay,
            message,
            ..
        } = local
        else {
            return None;
        };
        Some(Self {
            sender_handle,
            sender_label,
            sender_chat: message_id
                .and_then(|id| snapshot.agent_message_origins.get(&id))
                .and_then(|source| source["sender_chat_id"].as_str()?.parse().ok()),
            relay: relay.as_ref(),
            text: message,
        })
    }
}

/// Rows under a `⎿`, the first behind the connector and the rest under its content.
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

    fn lines(&mut self, text: &str, role: Option<Role>, detail: OutputDetail) {
        let text = safe_text(text);
        for line in text.lines() {
            let line = match detail {
                OutputDetail::Preview => clipped(line, PREVIEW_LINE_CHARACTERS).0,
                OutputDetail::Full => line.to_owned(),
            };
            self.row(vec![(line, role)]);
        }
    }

    /// A report's note in the warning colour, then its lines: all of them in full, otherwise the
    /// first few with the expand hint after the last when any were left out.
    fn report(&mut self, report: &SubagentReportMessage, detail: OutputDetail) {
        if let Some(note) = &report.note {
            self.lines(note, Some(Role::Warning), detail);
        }
        let text = safe_text(report.report.trim_end());
        let mut lines = text.lines().skip_while(|line| line.trim().is_empty());
        if matches!(detail, OutputDetail::Full) {
            for line in lines {
                self.row(vec![(line.to_owned(), None)]);
            }
            return;
        }
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
        }
    }

    /// A parent conversation holding the report the child of `child_run` delivered.
    fn delivered(child_run: Uuid, report: &SubagentReportMessage) -> ChatSnapshot {
        let message = format!(
            "<agent-message>{}</agent-message>",
            json!({"source":{"message_id":child_run,"sender_handle":"audit","sender_label":"audit","is_result":true},"message":report.text()})
        );
        serde_json::from_value(json!({
            "id":Uuid::new_v4(),"workdir":"/work","messages":[
                {"role":"user","content":[{"type":"text","text":message}]}
            ],"loops":[],"message_ids":{"0":child_run},"agent_message_origins":{child_run.to_string():{"is_result":true}}
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
    fn a_finished_child_and_its_report_read_as_one_block_in_its_outcome_colour() {
        let finished = completion(LoopState::Completed, SUBAGENT_REPORT_RECEIPT, None);
        let warning = "SECURITY WARNING: auto review flagged this subagent's report.";
        let report = SubagentReportMessage {
            note: Some(warning.into()),
            report: "First finding.\nSecond finding.\nThird finding.".into(),
        };
        let snapshot = delivered(finished.child_run_id, &report);
        let (heading, response) = block(&finished, &snapshot, OutputDetail::Preview);
        assert_eq!(joined(&heading), "\n⏺ @audit finished · 1m 3s\n");
        assert_eq!(heading[1].1, Some(Role::Success));
        assert_eq!(
            joined(&response),
            format!(
                "  ⎿  {warning}\n     First finding.\n     Second finding. (ctrl+o to expand)\n"
            )
        );
        assert!(response.contains(&(warning.into(), Some(Role::Warning))));
        let (_, full) = block(&finished, &snapshot, OutputDetail::Full);
        assert!(joined(&full).ends_with("     Third finding.\n"));
        assert!(!joined(&full).contains("expand"));

        for (state, word, dot) in [
            (LoopState::Failed, "failed", Role::Error),
            (LoopState::Interrupted, "interrupted", Role::Warning),
            (LoopState::Stopped, "stopped", Role::Warning),
            (LoopState::Expired, "stopped", Role::Warning),
        ] {
            let ended = completion(state, SUBAGENT_MISSING_REPORT, Some("Model unavailable"));
            let (heading, response) = block(&ended, &snapshot, OutputDetail::Preview);
            assert_eq!(joined(&heading), format!("\n⏺ @audit {word} · 1m 3s\n"));
            assert_eq!(heading[1].1, Some(dot));
            assert_eq!(joined(&response), "  ⎿  Model unavailable\n");
            assert_eq!(response[3].1, Some(Role::Error));
        }
    }

    #[test]
    fn a_child_without_a_report_says_so_and_a_refused_notice_stays_visible() {
        let other = SubagentReportMessage {
            note: None,
            report: "Another child's report".into(),
        };
        let snapshot = delivered(Uuid::new_v4(), &other);
        let silent = completion(LoopState::Completed, SUBAGENT_MISSING_REPORT, None);
        let (_, response) = block(&silent, &snapshot, OutputDetail::Preview);
        assert_eq!(joined(&response), "  ⎿  Ended without a report\n");

        let mut refused = completion(LoopState::Completed, "Plain reply text", None);
        refused.started_at = None;
        refused.notification = SubagentNotificationDelivery::Rejected(FamilyMessageRejection {
            code: FamilyMessageRejectionCode::StaleTarget,
            message: "The receiving parent is stopped".into(),
        });
        let (heading, response) = block(&refused, &snapshot, OutputDetail::Preview);
        assert_eq!(joined(&heading), "\n⏺ @audit finished\n");
        assert_eq!(
            joined(&response),
            "  ⎿  Plain reply text\n     Notification not delivered: The receiving parent is stopped\n"
        );
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
        assert_eq!(
            joined(&subagent_result_runs(&launch, false, None, "⎿")),
            "  ⎿  Started in background\n"
        );

        let unnamed =
            subagent_call_runs(&json!({"task":"Read\nthe   notes"}), None, &colors, display);
        assert_eq!(joined(&unnamed), "\n⏺ Subagent(Read the notes)\n");
        let failed = json!({"status":"failed","error":"Model unavailable"});
        let rows = subagent_result_runs(&failed, true, None, "⎿");
        assert_eq!(joined(&rows), "  ⎿  Failed: Model unavailable\n");
        assert_eq!(rows[3].1, Some(Role::Error));
    }
}

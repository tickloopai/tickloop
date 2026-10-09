use super::style::{Role, RoleLine};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tickloop_agent_protocol::{
    ApprovalAttemptOutcome, ApprovalFailureKind, ApprovalFallbackReason, ApprovalHistoryView,
    ApprovalPath, ApprovalRecord, ApprovalResult, ApprovalReviewerKind, AutoReviewerPreference,
    DenialCategory, DenialPersistence, PermissionApprovalSource,
};
use unicode_width::UnicodeWidthChar;
use uuid::Uuid;

pub(super) enum DenialsEffect {
    None,
    Back,
    Refresh,
    Retry(Uuid),
}

#[derive(Default)]
pub(super) struct DenialsSurface {
    view: Option<ApprovalHistoryView>,
    selected: usize,
    scroll: usize,
    detail: Option<ApprovalRecord>,
    detail_scroll: usize,
    error: Option<String>,
}

impl DenialsSurface {
    pub(super) fn replace_view(&mut self, view: ApprovalHistoryView) {
        let selected_id = self
            .view
            .as_ref()
            .and_then(|view| view.records.get(self.selected))
            .map(|record| record.id);
        self.selected = selected_id
            .and_then(|id| view.records.iter().position(|record| record.id == id))
            .unwrap_or(self.selected)
            .min(view.records.len().saturating_sub(1));
        self.view = Some(view);
        self.error = None;
    }

    pub(super) fn set_error(&mut self, error: String) {
        self.error = Some(error);
    }

    pub(super) fn is_detail(&self) -> bool {
        self.detail.is_some()
    }

    pub(super) fn handle(&mut self, key: KeyEvent) -> DenialsEffect {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        if let Some(record) = &self.detail {
            match key.code {
                KeyCode::Esc | KeyCode::Left => self.detail = None,
                KeyCode::Char('r') if !control && record.outcome == ApprovalResult::Deny => {
                    return DenialsEffect::Retry(record.id);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.detail_scroll = self.detail_scroll.saturating_add(1)
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.detail_scroll = self.detail_scroll.saturating_sub(1)
                }
                KeyCode::Char('n') if control => {
                    self.detail_scroll = self.detail_scroll.saturating_add(1)
                }
                KeyCode::Char('p') if control => {
                    self.detail_scroll = self.detail_scroll.saturating_sub(1)
                }
                KeyCode::PageDown => self.detail_scroll = self.detail_scroll.saturating_add(10),
                KeyCode::PageUp => self.detail_scroll = self.detail_scroll.saturating_sub(10),
                KeyCode::Home => self.detail_scroll = 0,
                KeyCode::End => self.detail_scroll = usize::MAX,
                _ => {}
            }
            return DenialsEffect::None;
        }
        let count = self.view.as_ref().map_or(0, |view| view.records.len());
        match key.code {
            KeyCode::Esc => return DenialsEffect::Back,
            KeyCode::Char('r') if !control => return DenialsEffect::Refresh,
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = (self.selected + 1).min(count.saturating_sub(1))
            }
            KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
            KeyCode::Char('n') if control => {
                self.selected = (self.selected + 1).min(count.saturating_sub(1))
            }
            KeyCode::Char('p') if control => self.selected = self.selected.saturating_sub(1),
            KeyCode::Home => self.selected = 0,
            KeyCode::End => self.selected = count.saturating_sub(1),
            KeyCode::PageDown => self.selected = (self.selected + 10).min(count.saturating_sub(1)),
            KeyCode::PageUp => self.selected = self.selected.saturating_sub(10),
            KeyCode::Enter => {
                self.detail = self
                    .view
                    .as_ref()
                    .and_then(|view| view.records.get(self.selected))
                    .cloned();
                self.detail_scroll = 0;
            }
            _ => {}
        }
        DenialsEffect::None
    }

    pub(super) fn content_rows(&self, width: usize) -> usize {
        let body = if let Some(record) = &self.detail {
            styled_detail_lines(record, width).len()
        } else {
            self.view.as_ref().map_or(1, |view| {
                if view.records.is_empty() {
                    2
                } else {
                    view.records.len()
                }
            })
        };
        self.header_lines().len() + body + 1
    }

    pub(super) fn minimum_frame_rows(&self) -> usize {
        self.header_lines().len() + 2
    }

    pub(super) fn styled_lines(&mut self, width: usize, height: usize) -> Vec<RoleLine> {
        if height == 0 {
            return Vec::new();
        }
        let footer = if height > 1 {
            Some(RoleLine::role(
                if self.detail.is_some() {
                    if self
                        .detail
                        .as_ref()
                        .is_some_and(|record| record.outcome == ApprovalResult::Deny)
                    {
                        "r retry · Esc back"
                    } else {
                        "Esc back"
                    }
                } else if self.view.is_none() && self.error.is_some() {
                    "r refresh · Esc back"
                } else {
                    "Enter details · r refresh · Esc back"
                },
                Role::MutedText,
            ))
        } else {
            None
        };
        let mut header = self.header_lines();
        let mut body_height = height.saturating_sub(header.len() + usize::from(footer.is_some()));

        if body_height == 0 && self.detail.is_none() {
            while body_height == 0 && !header.is_empty() {
                header.pop();
                body_height = height.saturating_sub(header.len() + usize::from(footer.is_some()));
            }
        }

        let body = if let Some(record) = &self.detail {
            let lines = styled_detail_lines(record, width);
            self.detail_scroll = self
                .detail_scroll
                .min(lines.len().saturating_sub(body_height));
            lines
                .into_iter()
                .skip(self.detail_scroll)
                .take(body_height)
                .collect::<Vec<_>>()
        } else if let Some(view) = &self.view {
            if view.records.is_empty() {
                vec![
                    RoleLine::role("No permission decisions yet", Role::Strong),
                    RoleLine::role("Tool permission results will appear here.", Role::MutedText),
                ]
                .into_iter()
                .take(body_height)
                .collect()
            } else {
                let visible_records = body_height.max(1);
                self.scroll = self.scroll.min(self.selected).max(
                    self.selected
                        .saturating_sub(visible_records.saturating_sub(1)),
                );
                view.records
                    .iter()
                    .enumerate()
                    .skip(self.scroll)
                    .take(visible_records)
                    .map(|(index, record)| list_line(index, record, index == self.selected))
                    .collect()
            }
        } else {
            vec![self.error.as_ref().map_or_else(
                || RoleLine::role("Loading history…", Role::MutedText),
                |error| {
                    RoleLine::role(
                        format!("History unavailable: {}", super::safe_line(error)),
                        Role::Error,
                    )
                },
            )]
            .into_iter()
            .take(body_height)
            .collect()
        };

        header
            .into_iter()
            .chain(body)
            .chain(footer)
            .map(|mut line| {
                line.truncate(width);
                line
            })
            .take(height)
            .collect()
    }

    fn header_lines(&self) -> Vec<RoleLine> {
        let Some(view) = &self.view else {
            return Vec::new();
        };
        let persistence = match &view.persistence {
            DenialPersistence::Disabled => "session only".to_owned(),
            DenialPersistence::Saved => "saved".to_owned(),
            DenialPersistence::Unsaved { reason } => {
                format!("unsaved · {}", super::safe_line(reason))
            }
        };
        let mut status = RoleLine::role(format!("{} records", view.records.len()), Role::Strong);
        status.push(format!(" · {persistence}"), Some(Role::MutedText));
        let mut lines = vec![status];
        if let Some(warning) = &view.load_warning {
            lines.push(RoleLine::role(
                format!("History warning: {}", super::safe_line(warning)),
                Role::Warning,
            ));
        }
        if let Some(error) = &self.error {
            lines.push(RoleLine::role(
                format!("History error: {}", super::safe_line(error)),
                Role::Error,
            ));
        }
        lines
    }
}

fn category(category: DenialCategory) -> &'static str {
    match category {
        DenialCategory::Rule => "Rule denied",
        DenialCategory::Plan => "Plan restriction",
        DenialCategory::Human => "User denied",
        DenialCategory::Automatic => "Auto denied",
        DenialCategory::NoHuman => "Human confirmation unavailable",
        DenialCategory::FinalAdmission => "Final check denied",
    }
}

fn list_line(index: usize, record: &ApprovalRecord, selected: bool) -> RoleLine {
    let mut operation = RoleLine::default();
    operation.push(
        if selected { "❯ " } else { "  " },
        Some(if selected {
            Role::Accent
        } else {
            Role::MutedText
        }),
    );
    operation.push(format!("{} ", index + 1), Some(Role::MutedText));
    let (outcome, outcome_role) = match record.outcome {
        ApprovalResult::Allow => ("Allow", Role::Success),
        ApprovalResult::Deny => ("Deny", Role::Error),
    };
    operation.push(
        outcome,
        Some(if selected { Role::Strong } else { outcome_role }),
    );
    operation.push(" · ", Some(Role::MutedText));
    operation.push(path_label(record.path), Some(Role::MutedText));
    operation.push(" · ", Some(Role::MutedText));
    let summary = display_operation_summary(&record.tool, &record.input_summary.text);
    operation.push(
        super::safe_line(&format!("{} · {summary}", record.tool)),
        Some(if selected { Role::Strong } else { Role::Text }),
    );
    if record.input_summary.truncated {
        operation.push(" · summary shortened", Some(Role::MutedText));
    }

    operation
}

fn display_operation_summary<'a>(tool: &str, summary: &'a str) -> &'a str {
    const BUILTIN_TOOLS: &[&str] = &[
        "Bash",
        "Read",
        "Write",
        "Edit",
        "Glob",
        "Grep",
        "WebFetch",
        "WebSearch",
        "TaskStop",
        "AskUserQuestion",
        "StartGoal",
        "TodoWrite",
        "EnterPlanMode",
        "ExitPlanMode",
    ];
    if BUILTIN_TOOLS.contains(&tool) {
        let prefix = format!("{tool}: ");
        summary.strip_prefix(&prefix).unwrap_or(summary)
    } else {
        summary
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum DetailGroup {
    Summary,
    Request,
    Review,
    Rules,
    Reference,
}

struct DetailLine {
    text: String,
    group: DetailGroup,
    request_payload: bool,
}

fn detail_entry(group: DetailGroup, text: String) -> DetailLine {
    DetailLine {
        text,
        group,
        request_payload: false,
    }
}

fn detail_lines(record: &ApprovalRecord) -> Vec<DetailLine> {
    let timestamp =
        time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(record.at_ms) * 1_000_000)
            .ok()
            .and_then(|time| {
                time.format(&time::format_description::well_known::Rfc3339)
                    .ok()
            })
            .unwrap_or_else(|| format!("{} ms since epoch", record.at_ms));
    let outcome = match record.outcome {
        ApprovalResult::Allow => "Allow",
        ApprovalResult::Deny => "Deny",
    };
    let summary = display_operation_summary(&record.tool, &record.input_summary.text);
    let mut lines = vec![detail_entry(
        DetailGroup::Summary,
        format!("Permission decision: {outcome} · {timestamp}"),
    )];
    if let Some(denial_category) = record.denial_category {
        lines.push(detail_entry(
            DetailGroup::Summary,
            format!("Denial category: {}", category(denial_category)),
        ));
    }
    lines.push(detail_entry(
        DetailGroup::Summary,
        format!("Operation: {} · {summary}", record.tool),
    ));
    lines.push(detail_entry(
        DetailGroup::Summary,
        format!("Decision path: {}", path_label(record.path)),
    ));

    lines.push(detail_entry(DetailGroup::Request, "Request:".into()));
    if let Some(details) = &record.request_details {
        for line in details.text.lines() {
            lines.push(DetailLine {
                text: format!("  {line}"),
                group: DetailGroup::Request,
                request_payload: true,
            });
        }
        if details.truncated {
            lines.push(DetailLine {
                text: "  [request details truncated]".into(),
                group: DetailGroup::Request,
                request_payload: true,
            });
        }
    } else {
        lines.push(detail_entry(
            DetailGroup::Request,
            "Request: not saved in this history record".into(),
        ));
    }

    lines.push(detail_entry(
        DetailGroup::Review,
        format!(
            "{}: {}",
            if record.outcome == ApprovalResult::Deny {
                "Reason"
            } else {
                "Decision note"
            },
            record.reason
        ),
    ));
    if let Some(source) = record.approval_source {
        lines.push(detail_entry(
            DetailGroup::Review,
            format!(
                "Approval source: {}",
                match source {
                    PermissionApprovalSource::PermissionPolicy => "permission policy",
                    PermissionApprovalSource::ExplicitAsk => "explicit Ask rule",
                    PermissionApprovalSource::SafetyCheck => "host safety check",
                    PermissionApprovalSource::AutoReviewer => "Auto reviewer",
                }
            ),
        ));
    }
    if record.input_summary.truncated {
        lines.push(detail_entry(
            DetailGroup::Review,
            "Operation summary is truncated; omitted content is not available here.".into(),
        ));
    }
    if let Some(review) = &record.review {
        if let Some(preferred) = review.preferred_reviewer {
            lines.push(detail_entry(
                DetailGroup::Review,
                format!(
                    "Preferred reviewer: {}",
                    match preferred {
                        AutoReviewerPreference::Llm => "LLM",
                        AutoReviewerPreference::Jev => "Jev (TypeSafe)",
                    }
                ),
            ));
        }
        if let Some(reviewer) = &review.reviewer {
            lines.push(detail_entry(
                DetailGroup::Review,
                format!(
                    "Review method: {}",
                    match reviewer.kind {
                        ApprovalReviewerKind::Llm => "LLM",
                        ApprovalReviewerKind::Jev => "Judgment",
                    }
                ),
            ));
            lines.push(detail_entry(
                DetailGroup::Review,
                format!(
                    "Actual reviewer: {} · requested model: {}",
                    match reviewer.kind {
                        ApprovalReviewerKind::Llm => "LLM",
                        ApprovalReviewerKind::Jev => "Jev (TypeSafe)",
                    },
                    reviewer.requested_model
                ),
            ));
            if let Some(model) = &reviewer.reported_model {
                lines.push(detail_entry(
                    DetailGroup::Review,
                    format!("Reported model: {model}"),
                ));
            }
        }
        lines.push(detail_entry(
            DetailGroup::Review,
            format!("Review outcome: {:?}", review.outcome),
        ));
        if review.fallback_from == Some(ApprovalReviewerKind::Jev) {
            lines.push(detail_entry(
                DetailGroup::Review,
                "Review path: Jev → LLM fallback".into(),
            ));
            let fallback_reason = review.fallback_reason.or_else(|| {
                review
                    .fallback_failure
                    .map(ApprovalFallbackReason::TechnicalFailure)
            });
            if let Some(reason) = fallback_reason {
                let label = match reason {
                    ApprovalFallbackReason::TechnicalFailure(failure) => failure_label(failure),
                    ApprovalFallbackReason::CoolingDown => "Jev is cooling down after a failure",
                    ApprovalFallbackReason::ProbeInProgress => {
                        "another approval is probing Jev recovery"
                    }
                    ApprovalFallbackReason::ConnectionChanged => {
                        "Jev connection changed before this approval"
                    }
                };
                lines.push(detail_entry(
                    DetailGroup::Review,
                    format!("Fallback cause: {label}"),
                ));
            }
            if !review
                .attempts
                .iter()
                .any(|attempt| attempt.reviewer.kind == ApprovalReviewerKind::Jev)
            {
                lines.push(detail_entry(
                    DetailGroup::Review,
                    "No Jev request was sent for this approval.".into(),
                ));
            }
        }
        for (index, attempt) in review.attempts.iter().enumerate() {
            let reviewer = match attempt.reviewer.kind {
                ApprovalReviewerKind::Llm => "LLM",
                ApprovalReviewerKind::Jev => "Jev",
            };
            let outcome = match attempt.outcome {
                ApprovalAttemptOutcome::Allow => "allowed",
                ApprovalAttemptOutcome::Block => "denied",
                ApprovalAttemptOutcome::NeedsHuman => "requires a human",
                ApprovalAttemptOutcome::TechnicalFailure => "call failed",
            };
            lines.push(detail_entry(
                DetailGroup::Review,
                format!(
                    "Attempt {}: {reviewer} · {} · {outcome} · {} ms",
                    index + 1,
                    attempt.reviewer.requested_model,
                    attempt.elapsed_ms
                ),
            ));
            if let Some(failure) = attempt.failure {
                lines.push(detail_entry(
                    DetailGroup::Review,
                    format!("Failure: {}", failure_label(failure)),
                ));
            }
            if let Some(usage) = &attempt.usage {
                lines.push(detail_entry(
                    DetailGroup::Review,
                    format!(
                        "Reported tokens: input {} · output {}",
                        usage
                            .input_tokens
                            .map_or_else(|| "unavailable".into(), |count| count.to_string()),
                        usage
                            .output_tokens
                            .map_or_else(|| "unavailable".into(), |count| count.to_string())
                    ),
                ));
            }
        }
        match &review.diagnostics {
            None => lines.push(detail_entry(
                DetailGroup::Review,
                "Review diagnostics: not recorded".into(),
            )),
            Some(diagnostics) => {
                for diagnostic in diagnostics {
                    lines.push(detail_entry(DetailGroup::Review, diagnostic.to_string()));
                }
            }
        }
        if let Some(rule) = &review.block_rule {
            lines.push(detail_entry(
                DetailGroup::Rules,
                format!(
                    "Auto rule {} · {:?} · {:?}: {}{}",
                    rule.id,
                    rule.kind,
                    rule.source,
                    rule.text.text,
                    if rule.text.truncated {
                        " [truncated]"
                    } else {
                        ""
                    }
                ),
            ));
        }
        if let Some(finding) = &review.block_finding {
            lines.push(detail_entry(
                DetailGroup::Rules,
                format!(
                    "Task scope: {:?} · applicability: {:?} · clearance: {:?}",
                    finding.task_scope, finding.applicability, finding.clearance
                ),
            ));
        }
        lines.push(detail_entry(
            DetailGroup::Review,
            format!(
                "Review versions: {} · {} · settings {}",
                review.context_version,
                review.template_version,
                review
                    .auto_settings_revision
                    .as_deref()
                    .unwrap_or("unavailable")
            ),
        ));
    }
    lines.push(detail_entry(
        DetailGroup::Rules,
        format!("Policy revision: {}", record.policy_revision),
    ));
    for rule in &record.matching_rules {
        lines.push(detail_entry(
            DetailGroup::Rules,
            format!(
                "Rule {} · {:?} · {:?}: {}{}",
                rule.id,
                rule.source,
                rule.decision,
                rule.matcher.text,
                if rule.matcher.truncated {
                    " [truncated]"
                } else {
                    ""
                }
            ),
        ));
    }
    if record.matching_rules_truncated {
        lines.push(detail_entry(
            DetailGroup::Rules,
            "Additional matching rules were omitted from this bounded history record.".into(),
        ));
    }
    lines.extend([
        detail_entry(DetailGroup::Reference, format!("Chat: {}", record.chat_id)),
        detail_entry(
            DetailGroup::Reference,
            format!("Loop: {} · Run: {}", record.loop_id, record.run_id),
        ),
        detail_entry(
            DetailGroup::Reference,
            format!(
                "Operation ID: {} · Call: {}",
                record.operation_id, record.call_id
            ),
        ),
        detail_entry(DetailGroup::Reference, format!("Record: {}", record.id)),
    ]);
    if record.outcome == ApprovalResult::Deny {
        lines.push(detail_entry(
            DetailGroup::Reference,
            "Retry creates a new request with this record as reference. The Agent prepares a new operation and checks current permissions.".into(),
        ));
    }
    lines
}

fn styled_detail_lines(record: &ApprovalRecord, width: usize) -> Vec<RoleLine> {
    let mut lines = Vec::new();
    let mut previous_group = None;
    for detail in detail_lines(record) {
        if previous_group.is_some_and(|previous| previous != detail.group) {
            lines.push(RoleLine::default());
        }
        previous_group = Some(detail.group);
        let safe = super::safe_line(&detail.text);
        let line = if detail.request_payload {
            RoleLine::role(safe, Role::Text)
        } else {
            detail_line(&safe)
        };
        lines.extend(wrap_role_line(line, width));
    }
    lines
}

fn detail_line(text: &str) -> RoleLine {
    if text == "Request:" {
        return RoleLine::role(text, Role::Accent);
    }
    if let Some((label, value)) = text.split_once(": ") {
        let role = match label {
            "Reason" | "Review outcome" | "Task scope" => Role::Warning,
            "Fallback cause" | "Failure" => Role::Error,
            "Operation" | "Tool" | "Request" => Role::Accent,
            _ => Role::MutedText,
        };
        let value_role = match label {
            "Reason" | "Review outcome" | "Task scope" => Role::Warning,
            "Fallback cause" | "Failure" => Role::Error,
            "Permission decision" => {
                if value.starts_with("Allow") {
                    Role::Success
                } else {
                    Role::Error
                }
            }
            _ if label.starts_with("Attempt ")
                || label.starts_with("Auto rule ")
                || label.starts_with("Rule ") =>
            {
                Role::MutedText
            }
            "Approval source" | "Preferred reviewer" | "Actual reviewer" | "Reported model"
            | "Reported tokens" | "Review versions" | "Policy revision" | "Chat" | "Loop"
            | "Operation ID" | "Record" => Role::MutedText,
            "Decision path" | "Decision note" | "Review path" => Role::MutedText,
            "Request" => Role::MutedText,
            "Denial category" => Role::Warning,
            _ => Role::Text,
        };
        let mut line = RoleLine::default();
        line.push(format!("{label}: "), Some(role));
        if label == "Permission decision"
            && let Some((outcome, timestamp)) = value.split_once(" · ")
        {
            line.push(
                outcome,
                Some(if outcome == "Allow" {
                    Role::Success
                } else {
                    Role::Error
                }),
            );
            line.push(format!(" · {timestamp}"), Some(Role::MutedText));
            return line;
        }
        if label == "Denial category" {
            line.push(value, Some(Role::Warning));
            return line;
        }
        line.push(value, Some(value_role));
        return line;
    }
    let role = if text.starts_with("Review path:")
        || text.starts_with("Auto rule ")
        || text.starts_with("Rule ")
    {
        Role::Warning
    } else {
        Role::MutedText
    };
    RoleLine::role(text, role)
}

fn path_label(path: ApprovalPath) -> &'static str {
    match path {
        ApprovalPath::Direct => "No review",
        ApprovalPath::AutoLlm => "LLM",
        ApprovalPath::AutoJev => "Jev",
        ApprovalPath::Human => "Manual",
    }
}

fn wrap_role_line(line: RoleLine, width: usize) -> Vec<RoleLine> {
    let width = width.max(1);
    let continuation_indent = line
        .text()
        .chars()
        .take_while(|character| *character == ' ')
        .count()
        .min(width.saturating_sub(1));
    let mut rows = Vec::new();
    let mut current = RoleLine::default();
    let mut used = 0;
    for span in line.spans {
        for character in span.text.chars() {
            let next = character.width().unwrap_or(0);
            if used + next > width && !current.spans.is_empty() {
                rows.push(std::mem::take(&mut current));
                current.push(" ".repeat(continuation_indent), Some(Role::Text));
                used = continuation_indent;
            }
            current.push(character.to_string(), span.role);
            used += next;
        }
    }
    rows.push(current);
    rows
}

fn failure_label(failure: ApprovalFailureKind) -> &'static str {
    match failure {
        ApprovalFailureKind::Configuration => "configuration unavailable",
        ApprovalFailureKind::Authentication => "authentication failed",
        ApprovalFailureKind::Connection => "connection failed",
        ApprovalFailureKind::Service => "service error",
        ApprovalFailureKind::Timeout => "time budget exhausted",
        ApprovalFailureKind::InvalidResponse => "invalid response",
        ApprovalFailureKind::ContextLimit => "complete context exceeds capacity",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tickloop_agent_protocol::{ApprovalHistoryView, DisplaySummary};
    use unicode_width::UnicodeWidthStr;

    fn record(outcome: ApprovalResult) -> ApprovalRecord {
        ApprovalRecord {
            id: Uuid::new_v4(),
            chat_id: Uuid::new_v4(),
            loop_id: Uuid::new_v4(),
            run_id: Uuid::new_v4(),
            operation_id: Uuid::new_v4(),
            call_id: "call".into(),
            at_ms: 1,
            tool: "Bash".into(),
            input_summary: DisplaySummary {
                text: "Bash: cargo test".into(),
                truncated: false,
            },
            request_details: None,
            outcome,
            path: ApprovalPath::Direct,
            denial_category: None,
            approval_source: None,
            reason: "Operation allowed".into(),
            policy_revision: 1,
            matching_rules: Vec::new(),
            matching_rules_truncated: false,
            review: None,
        }
    }

    fn view(records: Vec<ApprovalRecord>) -> ApprovalHistoryView {
        ApprovalHistoryView {
            records,
            persistence: DenialPersistence::Saved,
            load_warning: None,
        }
    }

    #[test]
    fn recent_activity_keeps_the_selected_single_line_visible_at_narrow_width() {
        let mut surface = DenialsSurface::default();
        surface.replace_view(view(vec![
            record(ApprovalResult::Deny),
            record(ApprovalResult::Allow),
            record(ApprovalResult::Deny),
            record(ApprovalResult::Allow),
        ]));
        for _ in 0..3 {
            surface.handle(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        }

        let lines = surface.styled_lines(20, 3);
        assert_eq!(lines.len(), 3);
        assert!(lines[1].text().starts_with("❯ 4 Allow"));
        assert!(lines.iter().all(|line| line.text().width() <= 20));
        assert!(
            !lines
                .iter()
                .any(|line| line.text().contains("Operation allowed"))
        );
    }

    #[test]
    fn missing_history_shows_refresh_error_and_allowed_detail_cannot_be_retried() {
        let mut surface = DenialsSurface::default();
        surface.set_error("service unavailable".into());
        let lines = surface.styled_lines(60, 3);
        assert!(
            lines[0]
                .text()
                .contains("History unavailable: service unavailable")
        );
        assert!(lines.last().unwrap().text().contains("r refresh"));
        assert!(matches!(
            surface.handle(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE)),
            DenialsEffect::Refresh
        ));

        surface.replace_view(view(vec![record(ApprovalResult::Allow)]));
        surface.handle(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(
            surface.handle(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE)),
            DenialsEffect::None
        ));
        assert!(
            !surface
                .styled_lines(60, 4)
                .last()
                .unwrap()
                .text()
                .contains("retry")
        );
    }

    #[test]
    fn request_details_keep_lines_indentation_and_escape_terminal_controls() {
        let mut approved = record(ApprovalResult::Allow);
        approved.request_details = Some(DisplaySummary {
            text: "{\n  \"command\": \"cargo test\",\n  \"note\": \"Review: user data \u{1b}\"\n}"
                .into(),
            truncated: false,
        });
        let mut surface = DenialsSurface::default();
        surface.replace_view(view(vec![approved]));
        surface.handle(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        let lines = surface.styled_lines(30, 60);
        let operation = lines
            .iter()
            .position(|line| line.text().starts_with("Operation: Bash · cargo test"))
            .unwrap();
        let request_header = lines
            .iter()
            .position(|line| line.text() == "Request:")
            .unwrap();
        let review = lines
            .iter()
            .position(|line| line.text().starts_with("Decision note:"))
            .unwrap();
        assert!(operation < request_header && request_header < review);

        let payload = &lines[request_header + 1..review];
        assert!(payload.iter().any(|line| line.text().starts_with("    ")));
        assert!(payload.iter().any(|line| line.text().contains("Review:")));
        assert!(
            payload
                .iter()
                .flat_map(|line| &line.spans)
                .all(|span| span.role == Some(Role::Text))
        );
        assert!(payload.iter().all(|line| !line.text().contains('\u{1b}')));
        assert!(payload.iter().any(|line| line.text().contains("\\u{1b}")));
        assert!(lines.iter().all(|line| line.text().width() <= 30));
    }
}

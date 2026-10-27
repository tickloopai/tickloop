use std::collections::VecDeque;

use serde_json::Value;
use uuid::Uuid;

use super::{Agent, AgentError};
use tickloop_harness_base::private_fs::unix_time_milliseconds;
use tickloop_harness_protocol::{
    ApprovalAssessmentDetails, ApprovalHistoryView, ApprovalOutcome, ApprovalPath, ApprovalRecord,
    ApprovalResult, AutoBlockKind, ChatId, DenialAutoFinding, DenialAutoRuleSummary,
    DenialCategory, DenialPersistence, DenialRecord, DenialReference, DenialReviewFacts,
    DenialRuleSource, DenialRuleSummary, DenialsView, DisplaySummary, MAX_RECENT_DENIALS,
    OperationRecord, PermissionApprovalSource, PermissionPolicy, PermissionRule, Receipt,
    SoftDenyClearance,
};
use tickloop_harness_tools::permissions::permission_rule_content;
use tickloop_harness_tools::{BASH_TOOL_NAME, WEB_FETCH_TOOL_NAME};

const MAX_DISPLAY_CHARS: usize = 160;
const MAX_REASON_CHARS: usize = 240;
const MAX_REQUEST_DETAIL_CHARS: usize = 4096;
const MAX_REQUEST_VALUE_CHARS: usize = 768;
const MAX_REQUEST_COLLECTION_ITEMS: usize = 24;
const MAX_REQUEST_DEPTH: usize = 6;
const AUTO_REVIEW_LOG_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug)]
pub(super) enum DirectApprovalBasis {
    ReadOnly,
    HostTool,
    Policy,
    FullAccess,
    UnrestrictedHost,
}

pub(super) struct AllowedDecision<'a> {
    pub mcp_server: Option<&'a str>,
    pub path: ApprovalPath,
    pub direct_basis: DirectApprovalBasis,
    pub approval_source: Option<PermissionApprovalSource>,
    pub assessment: Option<&'a ApprovalAssessmentDetails>,
}

pub(super) fn allowed_approval_record(
    chat_id: ChatId,
    operation: &OperationRecord,
    permission_input: &Value,
    policy: &PermissionPolicy,
    decision: AllowedDecision<'_>,
) -> ApprovalRecord {
    let sanitized = DenialCandidate::new(
        operation,
        permission_input,
        DenialCategory::FinalAdmission,
        decision.approval_source,
        policy,
        decision.assessment,
        decision.mcp_server,
    )
    .finish(chat_id, operation);
    let mut record = ApprovalRecord::from_denial(sanitized);
    record.outcome = ApprovalResult::Allow;
    record.path = decision.path;
    record.denial_category = None;
    record.reason = match decision.path {
        ApprovalPath::Human if decision.assessment.is_some() => {
            "Human approved this operation after automatic review"
        }
        ApprovalPath::Human => "Human approved this operation",
        ApprovalPath::AutoLlm => "LLM auto approval allowed this operation",
        ApprovalPath::AutoJev => "Jev auto approval allowed this operation",
        ApprovalPath::Direct => match decision.direct_basis {
            DirectApprovalBasis::ReadOnly => "Read-only operation allowed by current permissions",
            DirectApprovalBasis::HostTool => "Host tool allowed by current permissions",
            DirectApprovalBasis::Policy => {
                "Operation allowed by current permissions and execution mode"
            }
            DirectApprovalBasis::FullAccess => {
                "Full Access mode allowed this operation without ordinary approval"
            }
            DirectApprovalBasis::UnrestrictedHost => {
                "Startup unrestricted host policy allowed this operation without ordinary approval"
            }
        },
    }
    .into();
    record
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub(super) struct DenialWindow {
    pub records: VecDeque<ApprovalRecord>,
    #[serde(skip)]
    pub human_prompts: VecDeque<(tickloop_harness_protocol::RunId, String, Option<String>)>,
    pub revision: u64,
    pub saved_revision: u64,
    pub save_error: Option<String>,
    pub load_warning: Option<String>,
}

impl DenialWindow {
    pub fn remember_human_prompt(
        &mut self,
        request: &tickloop_harness_protocol::PermissionRequest,
    ) {
        if let Some(run) = request.execution.run_id() {
            self.human_prompts.push_front((
                run,
                request.execution.call_key().to_owned(),
                request.approval_reason.as_deref().map(|reason| {
                    if looks_like_secret(reason) {
                        "[REDACTED]".into()
                    } else {
                        bounded(
                            &reason.split_whitespace().collect::<Vec<_>>().join(" "),
                            MAX_REQUEST_DETAIL_CHARS,
                        )
                        .text
                    }
                }),
            ));
            self.human_prompts.truncate(MAX_RECENT_DENIALS);
        }
    }

    pub fn add_approval(&mut self, mut record: ApprovalRecord) {
        if let Some(index) = self
            .human_prompts
            .iter()
            .position(|(run, call, _)| *run == record.run_id && *call == record.call_id)
        {
            record.human_reason = self
                .human_prompts
                .remove(index)
                .and_then(|(_, _, reason)| reason);
        }
        if self
            .records
            .iter()
            .any(|existing| existing.operation_id == record.operation_id)
        {
            return;
        }
        self.records.push_front(record);
        self.records.truncate(MAX_RECENT_DENIALS);
        self.revision += 1;
    }

    pub fn view(&self, saves_chats: bool) -> DenialsView {
        let history = self.history_view(saves_chats);
        DenialsView {
            records: history
                .records
                .iter()
                .filter_map(ApprovalRecord::denial)
                .collect(),
            persistence: history.persistence,
            load_warning: history.load_warning,
        }
    }

    pub fn history_view(&self, saves_chats: bool) -> ApprovalHistoryView {
        let persistence = if !saves_chats {
            DenialPersistence::Disabled
        } else if self.saved_revision == self.revision {
            DenialPersistence::Saved
        } else {
            DenialPersistence::Unsaved {
                reason: self
                    .save_error
                    .clone()
                    .unwrap_or_else(|| "history save is pending".into()),
            }
        };
        ApprovalHistoryView {
            records: self.records.iter().cloned().collect(),
            persistence,
            load_warning: self.load_warning.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct DenialCandidate {
    pub category: DenialCategory,
    pub approval_source: Option<PermissionApprovalSource>,
    pub reason: String,
    pub policy_revision: u64,
    pub matching_rules: Vec<DenialRuleSummary>,
    pub matching_rules_truncated: bool,
    pub review: Option<DenialReviewFacts>,
}

impl DenialCandidate {
    pub fn new(
        operation: &OperationRecord,
        permission_input: &Value,
        category: DenialCategory,
        approval_source: Option<PermissionApprovalSource>,
        policy: &PermissionPolicy,
        assessment: Option<&ApprovalAssessmentDetails>,
        mcp_server: Option<&str>,
    ) -> Self {
        let (matching_rules, matching_rules_truncated) =
            matching_rule_summaries(policy, operation, permission_input, mcp_server);
        Self {
            category,
            approval_source,
            reason: denial_reason(category, assessment),
            policy_revision: policy.revision,
            matching_rules,
            matching_rules_truncated,
            review: assessment.map(|details| DenialReviewFacts {
                diagnostics: details.diagnostics.as_ref().map(|diagnostics| {
                    diagnostics
                        .iter()
                        .take(32)
                        .cloned()
                        .map(|mut diagnostic| {
                            use tickloop_harness_protocol::ApprovalDiagnostic;
                            match &mut diagnostic {
                                ApprovalDiagnostic::ApplicabilityUncertain { rule_id }
                                | ApprovalDiagnostic::ClearanceUncertain { rule_id } => {
                                    *rule_id = bounded(rule_id, 80).text
                                }
                                ApprovalDiagnostic::JevLowConfidence { question_id } => {
                                    *question_id = bounded(question_id, 96).text
                                }
                                _ => {}
                            }
                            diagnostic
                        })
                        .collect()
                }),
                preferred_reviewer: details.preferred_reviewer,
                reviewer: details.reviewer.clone().map(|mut reviewer| {
                    reviewer.requested_model = bounded(&reviewer.requested_model, 80).text;
                    reviewer.reported_model = reviewer
                        .reported_model
                        .map(|model| bounded(&model, 80).text);
                    reviewer
                }),
                auto_settings_revision: details
                    .auto_settings_revision
                    .as_deref()
                    .map(|revision| bounded(revision, 80).text),
                context_version: details.context_version.clone(),
                template_version: details.review_template_version.clone(),
                outcome: details.outcome,
                attempts: details
                    .attempts
                    .iter()
                    .take(4)
                    .cloned()
                    .map(|mut attempt| {
                        attempt.reviewer.requested_model =
                            bounded(&attempt.reviewer.requested_model, 80).text;
                        attempt.reviewer.reported_model = attempt
                            .reviewer
                            .reported_model
                            .map(|model| bounded(&model, 80).text);
                        attempt
                    })
                    .collect(),
                fallback_from: details.fallback_from,
                fallback_failure: details.fallback_failure,
                fallback_reason: details.fallback_reason,
                block_kind: details.block_kind,
                block_rule: details
                    .block_rule
                    .as_ref()
                    .map(|rule| DenialAutoRuleSummary {
                        id: bounded(&rule.id, 80).text,
                        kind: rule.kind,
                        source: rule.source.clone(),
                        text: safe_rule_text(&rule.text),
                    }),
                block_finding: details.block_evidence.as_ref().map(|evidence| {
                    let finding = evidence.finding.as_ref();
                    DenialAutoFinding {
                        task_scope: evidence.task_scope,
                        applicability: finding.map(|finding| finding.applicability),
                        clearance: finding.and_then(|finding| finding.clearance.clone()).map(
                            |clearance| match clearance {
                                SoftDenyClearance::SoftAllow(id) => {
                                    SoftDenyClearance::SoftAllow(bounded(&id, 80).text)
                                }
                                other => other,
                            },
                        ),
                    }
                }),
            }),
        }
    }

    pub fn project_rules_unavailable(mut self) -> Self {
        self.reason = "Project permission rules became unavailable before final admission".into();
        self
    }

    pub fn finish(self, chat_id: ChatId, operation: &OperationRecord) -> DenialRecord {
        DenialRecord {
            id: tickloop_ids::generate(),
            chat_id,
            loop_id: operation.loop_id,
            run_id: operation.run_id,
            operation_id: operation.id,
            call_id: bounded(&operation.call_id, MAX_DISPLAY_CHARS).text,
            at_ms: unix_time_milliseconds(),
            tool: bounded(&operation.name, MAX_DISPLAY_CHARS).text,
            input_summary: operation_summary(&operation.name, &operation.input),
            request_details: Some(safe_request_details(&operation.input)),
            category: self.category,
            approval_source: self.approval_source,
            reason: self.reason,
            policy_revision: self.policy_revision,
            matching_rules: self.matching_rules,
            matching_rules_truncated: self.matching_rules_truncated,
            review: self.review,
        }
    }
}

fn matching_rule_summaries(
    policy: &PermissionPolicy,
    operation: &OperationRecord,
    permission_input: &Value,
    mcp_server: Option<&str>,
) -> (Vec<DenialRuleSummary>, bool) {
    let rules = policy
        .managed_constraints
        .iter()
        .map(|rule| (DenialRuleSource::ManagedConstraint, rule))
        .chain(
            policy
                .project_constraints
                .iter()
                .map(|rule| (DenialRuleSource::ProjectConstraint, rule)),
        )
        .chain(policy.rules.iter().map(|rule| {
            (
                if rule.persistent {
                    DenialRuleSource::ProjectUser
                } else {
                    DenialRuleSource::ChatSession
                },
                rule,
            )
        }));
    let mut matched = rules
        .filter(|(_, rule)| match mcp_server {
            Some(server) => tickloop_harness_tools::permissions::mcp_rule_matches_call(
                rule,
                &operation.name,
                server,
                permission_input,
                operation.loop_id.into(),
            ),
            None => tickloop_harness_tools::permissions::rule_matches_call(
                rule,
                &operation.name,
                permission_input,
                operation.loop_id.into(),
            ),
        })
        .take(13)
        .map(|(source, rule)| DenialRuleSummary {
            id: rule.id,
            decision: rule.decision,
            source,
            matcher: rule_summary(rule),
        })
        .collect::<Vec<_>>();
    let truncated = matched.len() > 12;
    matched.truncate(12);
    (matched, truncated)
}

fn rule_summary(rule: &PermissionRule) -> DisplaySummary {
    let tool = bounded(&rule.tool, MAX_DISPLAY_CHARS);
    match rule.specifier.as_deref() {
        Some(specifier) if rule.tool == WEB_FETCH_TOOL_NAME => {
            bounded(&format!("{}({specifier})", tool.text), MAX_DISPLAY_CHARS)
        }
        Some(specifier) => {
            let shell = shell_summary(specifier);
            bounded(&format!("{}({})", tool.text, shell.text), MAX_DISPLAY_CHARS)
        }
        None if rule.input.is_some() => bounded(
            &format!("{} (exact input; values omitted)", tool.text),
            MAX_DISPLAY_CHARS,
        ),
        None => bounded(&format!("{} (any input)", tool.text), MAX_DISPLAY_CHARS),
    }
}

fn operation_summary(tool: &str, input: &Value) -> DisplaySummary {
    match tool {
        "Write" | "Edit" | "Read" => {
            let path = input
                .get("file_path")
                .and_then(Value::as_str)
                .and_then(|path| std::path::Path::new(path).file_name())
                .and_then(|name| name.to_str())
                .filter(|name| !contains_sensitive_label(name))
                .map(|name| bounded(name, 80).text)
                .unwrap_or_else(|| "[path omitted]".into());
            bounded(
                &format!("{tool}: {path}; content and other values omitted"),
                MAX_DISPLAY_CHARS,
            )
        }
        BASH_TOOL_NAME => {
            let command = input
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let summary = shell_summary(command);
            bounded(
                &format!("{BASH_TOOL_NAME}: {}; arguments omitted", summary.text),
                MAX_DISPLAY_CHARS,
            )
        }
        WEB_FETCH_TOOL_NAME => {
            let content =
                permission_rule_content(tool, input).unwrap_or_else(|| "[domain omitted]".into());
            bounded(
                &format!("{WEB_FETCH_TOOL_NAME}: {content}; path and prompt omitted"),
                MAX_DISPLAY_CHARS,
            )
        }
        _ => {
            let mut keys = input
                .as_object()
                .map(|object| {
                    object
                        .keys()
                        .filter(|key| !contains_sensitive_label(key))
                        .take(6)
                        .map(|key| bounded(key, 24).text)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            keys.sort();
            bounded(
                &format!("{tool}: fields [{}]; values omitted", keys.join(", ")),
                MAX_DISPLAY_CHARS,
            )
        }
    }
}

fn safe_request_details(input: &Value) -> DisplaySummary {
    let mut truncated = false;
    let sanitized = sanitize_request_value(input, 0, false, &mut truncated);
    let formatted = serde_json::to_string_pretty(&sanitized)
        .unwrap_or_else(|_| "[request details unavailable]".into());
    let mut summary = bounded_request_text(&formatted);
    summary.truncated |= truncated;
    summary
}

fn bounded_request_text(formatted: &str) -> DisplaySummary {
    let mut text = formatted
        .chars()
        .take(MAX_REQUEST_DETAIL_CHARS + 1)
        .collect::<String>();
    let truncated = text.chars().count() > MAX_REQUEST_DETAIL_CHARS;
    if truncated {
        text = text.chars().take(MAX_REQUEST_DETAIL_CHARS).collect();
        text.push_str("… [truncated]");
    }
    DisplaySummary { text, truncated }
}

fn sanitize_request_value(
    value: &Value,
    depth: usize,
    is_content: bool,
    truncated: &mut bool,
) -> Value {
    if depth >= MAX_REQUEST_DEPTH {
        *truncated = true;
        return Value::String("[nested input omitted]".into());
    }
    match value {
        Value::Object(fields) => {
            let mut safe = serde_json::Map::new();
            for (index, (key, value)) in
                fields.iter().take(MAX_REQUEST_COLLECTION_ITEMS).enumerate()
            {
                if is_sensitive_field(key) {
                    safe.insert(
                        format!("redacted_field_{}", index + 1),
                        Value::String("[REDACTED]".into()),
                    );
                } else {
                    let key = bounded(key, 80).text;
                    let is_content = is_content_field(&key);
                    safe.insert(
                        key,
                        sanitize_request_value(value, depth + 1, is_content, truncated),
                    );
                }
            }
            if fields.len() > MAX_REQUEST_COLLECTION_ITEMS {
                *truncated = true;
                safe.insert("remaining_fields".into(), Value::String("[omitted]".into()));
            }
            Value::Object(safe)
        }
        Value::Array(items) => {
            if items.iter().take(MAX_REQUEST_COLLECTION_ITEMS).any(|item| {
                item.as_str()
                    .is_some_and(|text| is_sensitive_flag(text) || looks_like_secret(text))
            }) {
                return Value::String("[REDACTED: sensitive arguments]".into());
            }
            let mut safe = items
                .iter()
                .take(MAX_REQUEST_COLLECTION_ITEMS)
                .map(|item| sanitize_request_value(item, depth + 1, false, truncated))
                .collect::<Vec<_>>();
            if items.len() > MAX_REQUEST_COLLECTION_ITEMS {
                *truncated = true;
                safe.push(Value::String("[remaining items omitted]".into()));
            }
            Value::Array(safe)
        }
        Value::String(text) if looks_like_secret(text) => Value::String("[REDACTED]".into()),
        Value::String(text) => {
            let preview = bounded(
                text,
                if is_content {
                    MAX_REQUEST_VALUE_CHARS
                } else {
                    320
                },
            );
            *truncated |= preview.truncated;
            Value::String(preview.text)
        }
        primitive => primitive.clone(),
    }
}

fn is_content_field(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().as_str(),
        "content" | "body" | "text"
    )
}

fn is_sensitive_field(key: &str) -> bool {
    let normalized = key.to_ascii_lowercase().replace(['-', '_'], "");
    [
        "secret",
        "token",
        "password",
        "passwd",
        "credential",
        "apikey",
        "privatekey",
        "authorization",
        "authentication",
        "auth",
        "cookie",
        "sessionkey",
        "accesskey",
        "clientkey",
        "jwt",
    ]
    .iter()
    .any(|label| normalized.contains(label))
        || normalized == "key"
}

fn is_sensitive_flag(text: &str) -> bool {
    if !text.trim_start().starts_with('-') {
        return false;
    }
    let normalized = text
        .trim_matches(|character: char| {
            !character.is_ascii_alphanumeric() && character != '-' && character != '_'
        })
        .split('=')
        .next()
        .unwrap_or_default();
    is_sensitive_field(normalized) || matches!(normalized, "-u" | "--user")
}

fn looks_like_secret(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    if [
        "authorization:",
        "proxy-authorization:",
        "x-api-key:",
        "bearer ",
        "basic ",
        "-----begin private key",
        "-----begin openssh private key",
        "github_pat_",
        "apikey_",
        "ghp_",
        "gho_",
        "ghu_",
        "ghs_",
        "ghr_",
        "xoxb-",
        "xoxp-",
        "secret-",
        "secret_",
    ]
    .iter()
    .any(|pattern| lower.contains(pattern))
    {
        return true;
    }
    if let Some(scheme_end) = lower.find("://") {
        let authority = lower[scheme_end + 3..]
            .split('/')
            .next()
            .unwrap_or_default();
        if authority.contains('@') {
            return true;
        }
    }
    if lower.contains("curl -u ") || lower.contains("--user ") {
        return true;
    }
    lower.split_whitespace().any(|part| {
        let token = part.trim_matches(|character: char| {
            !character.is_ascii_alphanumeric() && character != '-' && character != '_'
        });
        token
            .split_once('=')
            .is_some_and(|(name, _)| is_sensitive_field(name))
            || is_sensitive_flag(token)
            || token
                .strip_prefix("sk-")
                .is_some_and(|suffix| suffix.len() >= 8)
            || (token.starts_with("eyj")
                && token.len() >= 40
                && token.bytes().filter(|byte| *byte == b'.').count() == 2)
            || (token.starts_with("akia") && token.len() == 20)
    })
}

fn shell_summary(command: &str) -> DisplaySummary {
    let mut tokens = command.split_whitespace();
    let executable = tokens.next().unwrap_or_default();
    let supported = [
        "git", "cargo", "npm", "pnpm", "yarn", "python", "python3", "node", "ls", "cat", "rg",
        "find", "mkdir", "rm", "cp", "mv", "curl", "bash", "sh",
    ];
    if !supported.contains(&executable) {
        return DisplaySummary {
            text: "command details omitted".into(),
            truncated: true,
        };
    }
    let verb = tokens.next().filter(|value| {
        let verbs: &[&str] = match executable {
            "git" => &[
                "status", "diff", "log", "show", "add", "commit", "checkout", "switch", "merge",
                "pull", "push", "clone", "reset", "clean",
            ],
            "cargo" => &[
                "build", "check", "test", "fmt", "clippy", "run", "install", "update",
            ],
            "npm" | "pnpm" | "yarn" => &["install", "run", "test", "build", "publish"],
            "python" | "python3" => &["-m"],
            "rg" => &["--files"],
            _ => &[],
        };
        verbs.contains(value)
    });
    bounded(
        &verb.map_or_else(
            || executable.to_owned(),
            |verb| format!("{executable} {verb}"),
        ),
        48,
    )
}

fn denial_reason(
    category: DenialCategory,
    assessment: Option<&ApprovalAssessmentDetails>,
) -> String {
    match category {
        DenialCategory::Rule => "A permission rule denied this operation".into(),
        DenialCategory::Plan => "Plan mode denied this operation".into(),
        DenialCategory::Human => "Human approval was declined".into(),
        DenialCategory::Automatic => match assessment.and_then(|details| details.block_kind) {
            Some(AutoBlockKind::TaskScope) => "Auto review found this operation outside the task or in conflict with a later restriction".into(),
            Some(AutoBlockKind::HardDeny) => "An Auto Hard deny rule applies".into(),
            Some(AutoBlockKind::SoftDeny) => "An Auto Soft deny rule applies without clearance".into(),
            None => "Auto review blocked this operation".into(),
        },
        DenialCategory::NoHuman => match assessment.map(|details| details.outcome) {
            Some(ApprovalOutcome::ContextLimit) => "Auto review exceeded its context limit and human approval was unavailable".into(),
            Some(ApprovalOutcome::Unavailable) => "Auto review was unavailable and human approval was unavailable".into(),
            _ => "Human approval was required but unavailable".into(),
        },
        DenialCategory::FinalAdmission => "Operation was refused during final admission because authorization or environment changed".into(),
    }
}

fn safe_rule_text(text: &str) -> DisplaySummary {
    if contains_sensitive_label(text) {
        return DisplaySummary {
            text: "[rule text omitted: sensitive value]".into(),
            truncated: true,
        };
    }
    bounded(text, MAX_REASON_CHARS)
}

fn contains_sensitive_label(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    [
        "secret",
        "token",
        "password",
        "credential",
        "apikey",
        "api_key",
        "private",
        "auth",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn bounded(value: &str, max_chars: usize) -> DisplaySummary {
    let mut text = value
        .chars()
        .filter(|character| !character.is_control())
        .take(max_chars + 1)
        .collect::<String>();
    let truncated = text.chars().count() > max_chars;
    if truncated {
        text = text.chars().take(max_chars).collect();
        text.push_str("… [truncated]");
    }
    DisplaySummary { text, truncated }
}

impl Agent {
    pub(super) async fn log_auto_review(
        &self,
        receipt: &Receipt,
        operation: &OperationRecord,
        policy: &PermissionPolicy,
        assessment: &super::approval_assessment::ApprovalAssessment,
        decision: tickloop_harness_protocol::Decision,
        reason: Option<&str>,
    ) -> Option<String> {
        if matches!(assessment.verdict, super::AutoReview::Allow) {
            return None;
        }
        let transcript = self
            .inner
            .recorder
            .as_ref()?
            .local_chat_path(receipt.chat_id)?;
        let directory = transcript.with_extension("auto-review");
        let path = directory.join(format!("{}.log", operation.id));
        let record = DenialCandidate::new(
            operation,
            &operation.input,
            DenialCategory::Automatic,
            Some(PermissionApprovalSource::AutoReviewer),
            policy,
            Some(&assessment.details),
            None,
        )
        .finish(receipt.chat_id, operation);
        let diagnostic = serde_json::json!({
            "version": AUTO_REVIEW_LOG_VERSION,
            "at_ms": record.at_ms,
            "chat_id": record.chat_id,
            "loop_id": record.loop_id,
            "run_id": record.run_id,
            "operation_id": record.operation_id,
            "call_id": record.call_id,
            "tool": record.tool,
            "decision": decision,
            "reason": safe_request_details(&serde_json::json!({
                "reviewer_reason": assessment.details.reason,
                "resolution_reason": reason,
            })),
            "request_details": record.request_details,
            "original_input_json_bytes": serde_json::to_vec(&operation.input).expect("input serializes").len(),
            "context_metrics": assessment.context_metrics,
            "review": record.review,
        });
        let result = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            use tickloop_harness_base::private_fs::{
                PRIVATE_FILE_MODE, PrivateDirectory, replace_file_atomically,
            };
            let parent = directory.parent().expect("transcript has a parent");
            PrivateDirectory::anchored(parent.into(), directory.file_name().unwrap()).create()?;
            let bytes = serde_json::to_vec_pretty(&diagnostic)?;
            replace_file_atomically(&path, &bytes, PRIVATE_FILE_MODE)?;
            let mut logs = std::fs::read_dir(&directory)?
                .filter_map(Result::ok)
                .filter_map(|entry| {
                    let path = entry.path();
                    if path.extension()?.to_str()? != "log"
                        || Uuid::parse_str(path.file_stem()?.to_str()?).is_err()
                    {
                        return None;
                    }
                    let metadata = entry.metadata().ok()?;
                    (metadata.is_file() && !entry.file_type().ok()?.is_symlink())
                        .then(|| (metadata.modified().ok(), path))
                })
                .collect::<Vec<_>>();
            logs.sort();
            for (_, old) in logs
                .iter()
                .take(logs.len().saturating_sub(MAX_RECENT_DENIALS))
            {
                std::fs::remove_file(old)?;
            }
            Ok(())
        })
        .await;
        if matches!(result, Ok(Ok(()))) {
            None
        } else {
            Some("The automatic-review diagnostic log could not be saved.".into())
        }
    }

    pub(super) fn record_approval(
        &self,
        chat: &mut super::Chat,
        receipt: &Receipt,
        record: ApprovalRecord,
    ) {
        let operation_id = record.operation_id;
        chat.denial_window.add_approval(record);
        if let Some(record) = chat
            .denial_window
            .records
            .iter()
            .find(|record| record.operation_id == operation_id)
            .cloned()
        {
            self.emit(
                chat,
                receipt,
                tickloop_harness_protocol::EventKind::ApprovalRecorded {
                    record: Box::new(record),
                },
            );
        }
    }

    pub(super) async fn record_final_denial(&self, record: DenialRecord) {
        let mut chats = self.inner.chats.lock().await;
        let Some(chat) = chats.get_mut(&record.chat_id) else {
            return;
        };
        let receipt = Receipt {
            chat_id: record.chat_id,
            loop_id: record.loop_id,
            run_id: record.run_id,
        };
        self.record_approval(chat, &receipt, ApprovalRecord::from_denial(record));
        self.save(&chat.snapshot);
    }

    pub async fn recent_denials(&self, chat_id: ChatId) -> Result<DenialsView, AgentError> {
        let chats = self.inner.chats.lock().await;
        let chat = chats.get(&chat_id).ok_or(AgentError::NotFound)?;
        Ok(chat.denial_window.view(self.saves_chats()))
    }

    pub async fn approval_history_view(
        &self,
        chat_id: ChatId,
    ) -> Result<ApprovalHistoryView, AgentError> {
        let chats = self.inner.chats.lock().await;
        let chat = chats.get(&chat_id).ok_or(AgentError::NotFound)?;
        Ok(chat.denial_window.history_view(self.saves_chats()))
    }

    pub async fn request_denial_retry(
        &self,
        chat_id: ChatId,
        record_id: Uuid,
    ) -> Result<Receipt, AgentError> {
        let reference = {
            let chats = self.inner.chats.lock().await;
            let chat = chats.get(&chat_id).ok_or(AgentError::NotFound)?;
            chat.denial_window
                .records
                .iter()
                .find(|record| record.id == record_id && record.outcome == ApprovalResult::Deny)
                .map(|record| DenialReference {
                    record_id,
                    tool: record.tool.clone(),
                    operation: record.input_summary.clone(),
                    category: record.denial_category.expect("denial has a category"),
                })
        };
        let reference =
            reference.ok_or_else(|| AgentError::Invalid("denial record is unavailable".into()))?;
        self.submit_internal(
            chat_id,
            "Please reconsider my previously denied task under the current rules. Treat the historical denial only as reference, and prepare any operation again.".into(),
            super::SubmissionOptions { denial_reference: Some(reference), ..Default::default() },
        ).await
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use uuid::Uuid;

    use super::{operation_summary, rule_summary, safe_request_details};
    use tickloop_harness_protocol::{Decision, PermissionRule};

    #[test]
    fn web_fetch_records_name_the_domain_that_rules_and_grants_match() {
        let summary = operation_summary(
            "WebFetch",
            &json!({"url": "https://docs.example.com/private/report?id=7", "prompt": "Summarize"}),
        );
        assert_eq!(
            summary.text,
            "WebFetch: domain:docs.example.com; path and prompt omitted"
        );
        assert!(!summary.truncated);
        let rule = PermissionRule {
            id: Uuid::new_v4(),
            decision: Decision::Deny,
            tool: "WebFetch".into(),
            specifier: Some("domain:docs.example.com".into()),
            input: None,
            loop_id: None,
            persistent: true,
        };
        assert_eq!(
            rule_summary(&rule).text,
            "WebFetch(domain:docs.example.com)"
        );
    }

    #[test]
    fn request_details_show_plain_input_and_redact_nested_credentials() {
        let input = json!({
            "command": "printf hello > report.txt",
            "path": "/tmp/report.txt",
            "content": "Ordinary report preview",
            "nested": {
                "token": "nested-token-value",
                "headers": {"Authorization": "Bearer header-value"},
                "argv": ["curl", "--api-key", "argument-secret"],
            },
            "runner": "API_KEY=assignment-secret command",
            "request": "curl --token flag-secret",
            "curl_short": "curl -u name:short-password https://example.test",
            "curl_long": "curl --user name:long-password https://example.test",
            "curl_header": "curl --header='X-Api-Key: header-secret-value' https://example.test",
            "user": "ordinary-user",
            "url": "https://user:url-password@example.test/data",
            "opaque": "apikey_jevfixturevalue",
        });
        let details = safe_request_details(&input);
        assert!(details.text.contains("printf hello > report.txt"));
        assert!(details.text.contains("/tmp/report.txt"));
        assert!(details.text.contains("Ordinary report preview"));
        assert!(details.text.contains("ordinary-user"));
        assert!(details.text.contains('\n'));
        for secret in [
            "nested-token-value",
            "header-value",
            "argument-secret",
            "assignment-secret",
            "flag-secret",
            "short-password",
            "long-password",
            "header-secret-value",
            "url-password",
            "apikey_jevfixturevalue",
        ] {
            assert!(!details.text.contains(secret), "leaked {secret}");
        }
        assert!(details.text.contains("REDACTED"));
    }

    #[test]
    fn request_details_bound_strings_collections_and_depth() {
        let input = json!({
            "content": "x".repeat(10000),
            "arguments": (0..100).collect::<Vec<_>>(),
            "nested": {"a":{"b":{"c":{"d":{"e":{"f":"hidden"}}}}}},
        });
        let details = safe_request_details(&input);
        assert!(details.truncated);
        assert!(details.text.chars().count() <= 4120);
        assert!(!details.text.contains("hidden"));
        assert!(details.text.contains("omitted"));
    }
}

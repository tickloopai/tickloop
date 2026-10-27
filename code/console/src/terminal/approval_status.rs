use std::collections::HashMap;

use tickloop_harness::protocol::{
    ApprovalPath, ApprovalRecord, ApprovalResult, ApprovalReviewerKind, ChatSnapshot, Event,
    EventKind, PermissionApprovalSource,
};
use uuid::Uuid;

use super::{presentation::RoleRuns, safe_text, style::Role};

#[derive(Clone, Debug)]
pub(super) enum ApprovalStatus {
    Reviewing(ApprovalReviewerKind),
    Waiting {
        source: PermissionApprovalSource,
        reason: Option<String>,
        reviewer: Option<ApprovalReviewerKind>,
    },
    Settled(Box<ApprovalRecord>),
}

#[derive(Default)]
pub(super) struct ToolApprovals {
    states: HashMap<(Uuid, String), ApprovalStatus>,
    reviewers: HashMap<(Uuid, String), ApprovalReviewerKind>,
}

impl ToolApprovals {
    pub(super) fn from_snapshot(snapshot: &ChatSnapshot) -> Self {
        let mut approvals = Self::default();
        for event in &snapshot.events {
            approvals.record(event);
        }
        for record in &snapshot.approval_records {
            approvals.states.insert(
                (record.run_id.uuid(), record.call_id.clone()),
                ApprovalStatus::Settled(Box::new(record.clone())),
            );
        }
        approvals
    }

    pub(super) fn get(&self, run: Uuid, call: &str) -> Option<&ApprovalStatus> {
        self.states.get(&(run, call.to_owned()))
    }

    pub(super) fn record(&mut self, event: &Event) {
        let run = event.run_id.uuid();
        match &event.kind {
            EventKind::ApprovalReviewStarted { call_id, reviewer } => {
                let key = (run, call_id.clone());
                self.reviewers.insert(key.clone(), *reviewer);
                self.states
                    .insert(key, ApprovalStatus::Reviewing(*reviewer));
            }
            EventKind::PermissionRequested { request } => {
                let key = (run, request.execution.call_key().to_owned());
                let reviewer = self.reviewers.get(&key).copied();
                self.states.insert(
                    key,
                    ApprovalStatus::Waiting {
                        source: request.approval_source,
                        reason: request.approval_reason.clone(),
                        reviewer,
                    },
                );
            }
            EventKind::ApprovalRecorded { record } => {
                self.states.insert(
                    (run, record.call_id.clone()),
                    ApprovalStatus::Settled(record.clone()),
                );
            }
            EventKind::ToolCompleted { call_id, .. } => {
                let key = (run, call_id.clone());
                if !matches!(self.states.get(&key), Some(ApprovalStatus::Settled(_))) {
                    self.states.remove(&key);
                }
            }
            EventKind::Completed { .. } | EventKind::Failed(_) | EventKind::Interrupted => {
                self.states.retain(|(id, _), state| {
                    *id != run || matches!(state, ApprovalStatus::Settled(_))
                });
                self.reviewers.retain(|(id, _), _| *id != run);
            }
            _ => {}
        }
    }
}

fn single_line(text: &str) -> String {
    safe_text(text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn reviewer_name(reviewer: ApprovalReviewerKind) -> &'static str {
    match reviewer {
        ApprovalReviewerKind::Jev => "Jev",
        ApprovalReviewerKind::Llm => "LLM",
    }
}

impl ApprovalStatus {
    pub(super) fn pending_label(&self) -> Option<String> {
        match self {
            Self::Reviewing(reviewer) => {
                Some(format!("Auto review ({})…", reviewer_name(*reviewer)))
            }
            Self::Waiting { .. } => Some("Waiting for your approval…".into()),
            Self::Settled(_) => None,
        }
    }

    pub(super) fn denied(&self) -> bool {
        matches!(self, Self::Settled(record) if record.outcome == ApprovalResult::Deny)
    }

    pub(super) fn auto_approved(&self) -> bool {
        matches!(self, Self::Settled(record) if record.outcome == ApprovalResult::Allow && matches!(record.path, ApprovalPath::AutoJev | ApprovalPath::AutoLlm))
    }

    pub(super) fn human_context(&self) -> Option<String> {
        let (source, reason, reviewer) = match self {
            Self::Waiting {
                source,
                reason,
                reviewer,
            } => (*source, reason.as_deref(), *reviewer),
            Self::Settled(record) if record.path == ApprovalPath::Human => (
                record.approval_source.unwrap_or_default(),
                record.human_reason.as_deref(),
                record
                    .review
                    .as_ref()
                    .and_then(|review| review.reviewer.as_ref())
                    .map(|reviewer| reviewer.kind),
            ),
            _ => return None,
        };
        let source = match source {
            PermissionApprovalSource::AutoReviewer => reviewer
                .map(|reviewer| format!("Auto review ({}) asked you", reviewer_name(reviewer)))
                .unwrap_or_else(|| "Auto review asked you".into()),
            PermissionApprovalSource::ExplicitAsk => "Explicit Ask".into(),
            PermissionApprovalSource::PermissionPolicy => "Permission policy".into(),
            PermissionApprovalSource::SafetyCheck => "Safety check".into(),
        };
        Some(match reason.filter(|reason| !reason.is_empty()) {
            Some(reason) => format!("{source}: {}", single_line(reason)),
            None => source,
        })
    }

    pub(super) fn result_label(&self) -> Option<String> {
        self.pending_label().or_else(|| match self {
            Self::Settled(record) if record.outcome == ApprovalResult::Deny => match record.path {
                ApprovalPath::AutoJev | ApprovalPath::AutoLlm => Some(format!(
                    "Denied by Auto review ({}): {}",
                    if record.path == ApprovalPath::AutoJev {
                        "Jev"
                    } else {
                        "LLM"
                    },
                    single_line(&record.reason)
                )),
                ApprovalPath::Human => Some("Denied by you".into()),
                ApprovalPath::Direct => None,
            },
            _ => None,
        })
    }

    pub(super) fn allowed_label(&self) -> Option<&'static str> {
        let Self::Settled(record) = self else {
            return None;
        };
        if record.outcome != ApprovalResult::Allow {
            return None;
        }
        match record.path {
            ApprovalPath::AutoJev => Some("Allowed by Auto review (Jev)"),
            ApprovalPath::AutoLlm => Some("Allowed by Auto review (LLM)"),
            ApprovalPath::Human => Some("Approved by you"),
            ApprovalPath::Direct => None,
        }
    }

    pub(super) fn decorate(&self, mut runs: RoleRuns, glyph: &str) -> RoleRuns {
        if let Some(text) = self.result_label() {
            runs = result_runs(&text, glyph);
        }
        if let Some(label) = self.allowed_label() {
            runs.extend(result_runs(label, glyph));
        }
        if let Some(context) = self.human_context() {
            runs.push((format!("     {context}\n"), Some(Role::MutedText)));
        }
        runs
    }
}

pub(super) fn result_runs(text: &str, glyph: &str) -> RoleRuns {
    vec![(format!("  {glyph}  {text}\n"), Some(Role::MutedText))]
}

pub(super) fn clips_line(text: &str) -> bool {
    [
        "Denied by Auto review (",
        "Auto review (",
        "Auto review asked you",
        "Explicit Ask",
        "Permission policy",
        "Safety check",
    ]
    .iter()
    .any(|prefix| {
        text.trim_start_matches([' ', '⎿', '↳', '└'])
            .starts_with(prefix)
    })
}

#[cfg(test)]
mod tests;

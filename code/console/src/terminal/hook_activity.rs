//! How hook runs show: a line in the conversation when a hook has something to tell the user,
//! and their records in the Recent activity of `/permissions`.

use tickloop_harness::protocol::hooks::{
    HookBackgroundPhase, HookDecision, HookRunOutcome, HookRunRecord, HookSkipNotice, HookSource,
};

use super::style::Role;

/// `PreToolUse:Bash`, or the event alone outside tool calls, as Claude Code names a hook.
pub(super) fn hook_label(record: &HookRunRecord) -> String {
    match &record.tool_name {
        Some(tool) => format!("{}:{tool}", record.event.name()),
        None => record.event.name().to_owned(),
    }
}

pub(super) fn source_label(source: &HookSource) -> String {
    match source {
        HookSource::User => "your settings".into(),
        HookSource::Plugin { id } => format!("plugin {id}"),
        HookSource::Repository => "this repository".into(),
    }
}

fn outcome_label(outcome: HookRunOutcome) -> &'static str {
    match outcome {
        HookRunOutcome::Success => "ran",
        HookRunOutcome::Blocking => "blocked",
        HookRunOutcome::NonBlockingError => "failed",
        HookRunOutcome::TimedOut => "timed out",
        HookRunOutcome::Cancelled => "cancelled",
        HookRunOutcome::NotRun => "not run",
    }
}

/// How a run went, with a background run's start and end told apart.
pub(super) fn run_label(record: &HookRunRecord) -> String {
    match record.background {
        Some(HookBackgroundPhase::Started) => "started in the background".into(),
        Some(HookBackgroundPhase::Ended) => {
            format!("{} in the background", outcome_label(record.outcome))
        }
        None => outcome_label(record.outcome).into(),
    }
}

pub(super) fn decision_label(decision: HookDecision) -> &'static str {
    match decision {
        HookDecision::Allow => "allow",
        HookDecision::Ask => "ask",
        HookDecision::Deny => "deny",
        HookDecision::Block => "block",
        HookDecision::IgnoredAllow => "allow (ignored: only your own hooks can allow)",
    }
}

/// The lines a finished hook run shows in the conversation: its `systemMessage`, and why it
/// blocked or failed. A quiet success and a background hook's start show nothing; their records
/// stay in Recent activity. A background hook that exits 2 shows as the notification it sends.
pub(super) fn run_notices(record: &HookRunRecord) -> Vec<(String, Role)> {
    if record.background == Some(HookBackgroundPhase::Started) {
        return Vec::new();
    }
    let label = hook_label(record);
    let mut notices = Vec::new();
    if let Some(message) = &record.system_message {
        notices.push((format!("{label} hook: {message}"), Role::Warning));
    }
    let reason = record.reason.as_deref().unwrap_or("no reason given");
    match record.outcome {
        HookRunOutcome::Blocking if record.background.is_some() => {}
        HookRunOutcome::Blocking => {
            notices.push((format!("{label} hook blocked: {reason}"), Role::Warning))
        }
        HookRunOutcome::NonBlockingError | HookRunOutcome::TimedOut | HookRunOutcome::NotRun => {
            notices.push((
                format!("{label} hook {}: {reason}", run_label(record)),
                Role::Warning,
            ))
        }
        HookRunOutcome::Success | HookRunOutcome::Cancelled => {}
    }
    notices
}

/// The line that tells the user matching hooks were not run.
pub(super) fn skip_notice(notice: &HookSkipNotice) -> String {
    let event = match &notice.tool_name {
        Some(tool) => format!("{}:{tool}", notice.event.name()),
        None => notice.event.name().to_owned(),
    };
    let mut parts = Vec::new();
    if notice.unconfirmed > 0 {
        parts.push(format!(
            "{} not confirmed yet · review in /hooks",
            plural(notice.unconfirmed)
        ));
    }
    if notice.unconfigured > 0 {
        parts.push(format!(
            "{} waiting for plugin configuration",
            plural(notice.unconfigured)
        ));
    }
    format!("{event} hooks not run: {}", parts.join(" · "))
}

fn plural(count: usize) -> String {
    if count == 1 {
        "1 hook".into()
    } else {
        format!("{count} hooks")
    }
}

#[cfg(test)]
mod tests {
    use tickloop_harness::protocol::hooks::HookEvent;

    use super::*;

    fn record(outcome: HookRunOutcome) -> HookRunRecord {
        HookRunRecord {
            id: uuid::Uuid::nil(),
            chat_id: uuid::Uuid::nil().into(),
            loop_id: uuid::Uuid::nil().into(),
            run_id: uuid::Uuid::nil().into(),
            at_ms: 0,
            event: HookEvent::PreToolUse,
            source: HookSource::User,
            command: "guard.sh".into(),
            tool_name: Some("Bash".into()),
            call_id: Some("call".into()),
            duration_ms: 12,
            outcome,
            exit_code: Some(2),
            decision: Some(HookDecision::Deny),
            reason: Some("builds are frozen".into()),
            system_message: None,
            updated_input: None,
            output: String::new(),
            background: None,
        }
    }

    #[test]
    fn a_quiet_success_shows_nothing_and_a_block_says_why() {
        assert!(run_notices(&record(HookRunOutcome::Success)).is_empty());
        let blocked = run_notices(&record(HookRunOutcome::Blocking));
        assert_eq!(
            blocked[0].0,
            "PreToolUse:Bash hook blocked: builds are frozen"
        );
        let mut message = record(HookRunOutcome::Success);
        message.system_message = Some("Formatted 2 files".into());
        assert_eq!(
            run_notices(&message)[0].0,
            "PreToolUse:Bash hook: Formatted 2 files"
        );
    }

    #[test]
    fn a_background_hook_shows_its_end_but_not_its_start() {
        let mut started = record(HookRunOutcome::Success);
        started.background = Some(HookBackgroundPhase::Started);
        started.system_message = Some("checking".into());
        assert!(run_notices(&started).is_empty());
        assert_eq!(run_label(&started), "started in the background");

        let mut failed = record(HookRunOutcome::NonBlockingError);
        failed.background = Some(HookBackgroundPhase::Ended);
        assert_eq!(
            run_notices(&failed)[0].0,
            "PreToolUse:Bash hook failed in the background: builds are frozen"
        );
        let mut rewoke = record(HookRunOutcome::Blocking);
        rewoke.background = Some(HookBackgroundPhase::Ended);
        assert!(
            run_notices(&rewoke).is_empty(),
            "its notification already shows"
        );
    }

    #[test]
    fn skipped_hooks_point_at_the_hooks_panel() {
        let notice = HookSkipNotice {
            event: HookEvent::PreToolUse,
            tool_name: Some("Bash".into()),
            unconfirmed: 2,
            unconfigured: 1,
        };
        assert_eq!(
            skip_notice(&notice),
            "PreToolUse:Bash hooks not run: 2 hooks not confirmed yet · review in /hooks · 1 hook waiting for plugin configuration"
        );
    }
}

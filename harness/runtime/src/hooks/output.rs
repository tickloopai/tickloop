//! What one hook run said, read as Claude Code 2.1.302 reads it, and what the runs an event
//! matched decide together.
//!
//! Exit status 0 is success: standard output starting with `{` is its JSON answer, anything else
//! is plain text. Status 2 blocks with standard error as the reason, after any JSON answer it
//! also printed. Any other status only tells the user, though a JSON answer it printed still
//! decides, as in Claude Code 2.1.302; a timeout or an answer that cannot be read only tells
//! the user. A first line `{"async": true}` asks to go on in the background, and what follows
//! it is the answer.

use serde_json::{Map, Value};
use tickloop_harness_protocol::hooks::{HookDecision, HookEvent, HookRunOutcome, HookSource};

const BLOCKED_BY_HOOK: &str = "Blocked by hook";
const NO_STDERR: &str = "No stderr output";
/// The longest first line of output read as a request to go on in the background.
const BACKGROUND_REQUEST_BYTES: usize = 4 * 1024;

/// A permission a `PreToolUse` hook gave the call, strongest last.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum HookPermission {
    Allow,
    Ask,
    Deny,
}

/// How a hook process ended, as the execution environment reported it.
pub(crate) struct HookExit<'a> {
    pub code: Option<i32>,
    pub timed_out: bool,
    pub stdout: &'a str,
    pub stderr: &'a str,
}

/// What one run said.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct HookResponse {
    pub outcome: HookRunOutcome,
    pub permission: Option<HookPermission>,
    /// Why it gave its permission.
    pub permission_reason: Option<String>,
    /// An event's blocking error, or `[command]: stderr` for status 2. Explicit PreToolUse
    /// denials use `permission_reason`, so an absent reason stays absent.
    pub blocking_error: Option<String>,
    pub system_message: Option<String>,
    pub updated_input: Option<Value>,
    pub additional_context: Option<String>,
    /// Standard output that was not a JSON answer, from a run that exited 0.
    pub plain_text: Option<String>,
    pub prevent_continuation: bool,
    pub stop_reason: Option<String>,
    pub suppress_output: bool,
    /// Why the run is a non-blocking error, for the user only.
    pub error: Option<String>,
}

impl HookResponse {
    pub(crate) fn failed(outcome: HookRunOutcome, error: String) -> Self {
        Self {
            outcome,
            error: Some(error),
            ..Self::success()
        }
    }

    pub(super) fn success() -> Self {
        Self {
            outcome: HookRunOutcome::Success,
            permission: None,
            permission_reason: None,
            blocking_error: None,
            system_message: None,
            updated_input: None,
            additional_context: None,
            plain_text: None,
            prevent_continuation: false,
            stop_reason: None,
            suppress_output: false,
            error: None,
        }
    }

    /// The decision its record shows; an allow counts only from the user's own hooks.
    pub(crate) fn decision(&self, source: &HookSource) -> Option<HookDecision> {
        match self.permission {
            Some(HookPermission::Deny) => Some(HookDecision::Deny),
            Some(HookPermission::Ask) => Some(HookDecision::Ask),
            Some(HookPermission::Allow) if *source == HookSource::User => Some(HookDecision::Allow),
            Some(HookPermission::Allow) => Some(HookDecision::IgnoredAllow),
            None => self.blocking_error.as_ref().map(|_| HookDecision::Block),
        }
    }

    /// Why it decided, blocked or failed, as its record says.
    pub(super) fn reason(&self) -> Option<String> {
        self.blocking_error
            .clone()
            .or(self.permission_reason.clone())
            .or(self.error.clone())
            .or(self.stop_reason.clone())
    }
}

enum Answer {
    Json(Map<String, Value>),
    PlainText,
    Unreadable(String),
}

fn read_answer(stdout: &str) -> Answer {
    let trimmed = stdout.trim();
    if !trimmed.starts_with('{') {
        return Answer::PlainText;
    }
    match serde_json::from_str::<Value>(trimmed) {
        Ok(Value::Object(answer)) => Answer::Json(answer),
        Ok(_) => Answer::PlainText,
        Err(_) if !trimmed.ends_with('}') => Answer::PlainText,
        Err(error) => Answer::Unreadable(format!(
            "Hook output looks like a JSON object but is not valid JSON — {error}"
        )),
    }
}

fn text(answer: &Map<String, Value>, key: &str) -> Option<String> {
    answer
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// Applies a JSON answer, or says why it is not valid for `event`.
fn apply_answer(
    response: &mut HookResponse,
    event: HookEvent,
    answer: &Map<String, Value>,
) -> Result<(), String> {
    if answer.get("continue") == Some(&Value::Bool(false)) {
        response.prevent_continuation = true;
        response.stop_reason = text(answer, "stopReason");
    }
    let reason = text(answer, "reason");
    match answer.get("decision").and_then(Value::as_str) {
        None => {}
        Some("approve") => {
            response.permission = Some(HookPermission::Allow);
            response.permission_reason = reason.clone();
        }
        Some("block") => {
            response.permission = Some(HookPermission::Deny);
            response.permission_reason = reason.clone();
            response.blocking_error = (event != HookEvent::PreToolUse)
                .then(|| reason.clone().unwrap_or(BLOCKED_BY_HOOK.into()));
        }
        Some(other) => {
            return Err(format!(
                "Unknown hook decision type: {other}. Valid types are: approve, block"
            ));
        }
    }
    response.system_message = text(answer, "systemMessage");
    response.suppress_output = answer.get("suppressOutput") == Some(&Value::Bool(true));
    let Some(specific) = answer.get("hookSpecificOutput") else {
        return Ok(());
    };
    let Some(specific) = specific.as_object() else {
        return Err("hookSpecificOutput must be an object".into());
    };
    let named = specific
        .get("hookEventName")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if named != event.name() {
        return Err(format!(
            "Hook returned incorrect event name: expected '{}' but got '{named}'",
            event.name()
        ));
    }
    response.additional_context = text(specific, "additionalContext");
    if event != HookEvent::PreToolUse {
        return Ok(());
    }
    let decision_reason = text(specific, "permissionDecisionReason");
    match specific.get("permissionDecision").and_then(Value::as_str) {
        None => {}
        Some("allow") => response.permission = Some(HookPermission::Allow),
        Some("ask") => response.permission = Some(HookPermission::Ask),
        Some("deny") => {
            response.permission = Some(HookPermission::Deny);
            response.blocking_error = None;
        }
        Some(other) => {
            return Err(format!(
                "Unknown hook permissionDecision type: {other}. Valid types are: allow, deny, ask"
            ));
        }
    }
    response.permission_reason = decision_reason;
    match specific.get("updatedInput") {
        None | Some(Value::Null) => {}
        Some(Value::Object(input)) => response.updated_input = Some(Value::Object(input.clone())),
        Some(_) => return Err("updatedInput must be an object".into()),
    }
    Ok(())
}

/// Whether `line` is a JSON object with `"async": true`, with which Claude Code 2.1.302 lets a
/// running command hook go on in the background; any other field, such as `asyncTimeout`, is
/// ignored.
fn asks_for_background(line: &str) -> bool {
    serde_json::from_str::<Value>(line.trim())
        .is_ok_and(|answer| answer.get("async") == Some(&Value::Bool(true)))
}

/// What a hook printed after a first line asking to go on in the background; `None` when its
/// first line asked nothing of the kind.
pub(super) fn after_background_request(stdout: &str) -> Option<&str> {
    let stdout = stdout.trim_start();
    let (first, rest) = stdout.split_once('\n').unwrap_or((stdout, ""));
    asks_for_background(first).then_some(rest)
}

/// Whether the first line of a hook's standard output, read from `stdout` as it arrives, asks
/// to go on in the background. Like Claude Code, a request ending in `}` counts before its line
/// ends; a longer first line asks nothing.
pub(super) async fn first_line_asks_for_background(
    mut stdout: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
) -> bool {
    let mut received = Vec::new();
    while let Some(chunk) = stdout.recv().await {
        received.extend_from_slice(&chunk);
        if let Some(end) = received.iter().position(|byte| *byte == b'\n') {
            return asks_for_background(&String::from_utf8_lossy(&received[..end]));
        }
        if received.len() > BACKGROUND_REQUEST_BYTES {
            return false;
        }
        if chunk.contains(&b'}') && asks_for_background(&String::from_utf8_lossy(&received)) {
            return true;
        }
    }
    false
}

/// Reads what the run of `command` for `event` said.
pub(crate) fn interpret(event: HookEvent, command: &str, exit: HookExit<'_>) -> HookResponse {
    if exit.timed_out {
        return HookResponse::failed(HookRunOutcome::TimedOut, "Hook timed out".into());
    }
    let stderr = exit.stderr.trim();
    match exit.code {
        Some(0) => {
            let mut response = HookResponse::success();
            match read_answer(exit.stdout) {
                Answer::PlainText => {
                    let plain = exit.stdout.trim();
                    response.plain_text = (!plain.is_empty()).then(|| plain.to_owned());
                    response
                }
                Answer::Unreadable(error) => {
                    HookResponse::failed(HookRunOutcome::NonBlockingError, error)
                }
                Answer::Json(answer) => match apply_answer(&mut response, event, &answer) {
                    Ok(()) => {
                        if response.blocking_error.is_some()
                            || response.permission == Some(HookPermission::Deny)
                        {
                            response.outcome = HookRunOutcome::Blocking;
                        }
                        response
                    }
                    Err(error) => HookResponse::failed(HookRunOutcome::NonBlockingError, error),
                },
            }
        }
        Some(2) => {
            let mut response = HookResponse::success();
            if let Answer::Json(answer) = read_answer(exit.stdout)
                && apply_answer(&mut response, event, &answer).is_err()
            {
                response = HookResponse::success();
            }
            response.outcome = HookRunOutcome::Blocking;
            if response.blocking_error.is_none()
                && response.permission != Some(HookPermission::Deny)
            {
                let stderr = if stderr.is_empty() { NO_STDERR } else { stderr };
                response.blocking_error = Some(format!("[{command}]: {stderr}"));
            }
            if event == HookEvent::PreToolUse {
                if response.permission != Some(HookPermission::Deny) {
                    response.permission_reason = None;
                }
                response.permission = Some(HookPermission::Deny);
            }
            response
        }
        code => {
            let error = format!(
                "Failed with non-blocking status code: {}",
                if stderr.is_empty() {
                    code.map_or_else(|| "no status".to_owned(), |code| code.to_string())
                } else {
                    stderr.to_owned()
                }
            );
            let mut response = HookResponse::success();
            match read_answer(exit.stdout) {
                Answer::Json(answer) if apply_answer(&mut response, event, &answer).is_ok() => {
                    response.outcome = if response.blocking_error.is_some()
                        || response.permission == Some(HookPermission::Deny)
                    {
                        HookRunOutcome::Blocking
                    } else {
                        HookRunOutcome::NonBlockingError
                    };
                    response.error = Some(error);
                    response
                }
                _ => HookResponse::failed(HookRunOutcome::NonBlockingError, error),
            }
        }
    }
}

/// What the runs an event matched decide together, in configuration order.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct HookVerdict {
    /// Deny over ask over allow; an allow counts only from the user's own hooks.
    pub permission: Option<HookPermission>,
    pub permission_reason: Option<String>,
    pub blocking_errors: Vec<String>,
    /// The last `updatedInput` of a run that did not deny.
    pub updated_input: Option<Value>,
    pub additional_context: Vec<String>,
    pub plain_text: Vec<String>,
    pub prevent_continuation: bool,
    pub stop_reason: Option<String>,
    /// A run was cancelled: the turn was interrupted or stopped.
    pub cancelled: bool,
}

impl HookVerdict {
    /// The verdict of `responses`, and a warning when several rewrote the input.
    pub(crate) fn merge<'a>(
        responses: impl IntoIterator<Item = (&'a HookSource, &'a HookResponse)>,
    ) -> (Self, Option<String>) {
        let mut verdict = Self::default();
        let mut rewrites = 0;
        let mut rewritten_by_user = true;
        for (source, response) in responses {
            verdict.cancelled |= response.outcome == HookRunOutcome::Cancelled;
            let permission = response.permission.filter(|permission| {
                *permission != HookPermission::Allow || *source == HookSource::User
            });
            if permission > verdict.permission {
                verdict.permission = permission;
                verdict.permission_reason = response.permission_reason.clone();
            } else if permission.is_some()
                && permission == verdict.permission
                && verdict.permission_reason.is_none()
            {
                verdict.permission_reason = response.permission_reason.clone();
            }
            verdict
                .blocking_errors
                .extend(response.blocking_error.clone());
            if let Some(input) = &response.updated_input
                && response.permission != Some(HookPermission::Deny)
            {
                rewrites += 1;
                rewritten_by_user = *source == HookSource::User;
                verdict.updated_input = Some(input.clone());
            }
            verdict
                .additional_context
                .extend(response.additional_context.clone());
            verdict.plain_text.extend(response.plain_text.clone());
            if response.prevent_continuation {
                verdict.prevent_continuation = true;
                verdict.stop_reason = verdict.stop_reason.take().or(response.stop_reason.clone());
            }
        }
        // A user's allow covers the input the user's hooks saw or wrote, never another
        // source's rewrite of it.
        if verdict.permission == Some(HookPermission::Allow) && !rewritten_by_user {
            verdict.permission = None;
            verdict.permission_reason = None;
        }
        let warning = (rewrites > 1).then(|| {
            format!(
                "{rewrites} hooks rewrote the input; the last one in configuration order applies"
            )
        });
        (verdict, warning)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn exit(code: i32, stdout: &str, stderr: &str) -> HookExit<'static> {
        HookExit {
            code: Some(code),
            timed_out: false,
            stdout: Box::leak(stdout.to_owned().into_boxed_str()),
            stderr: Box::leak(stderr.to_owned().into_boxed_str()),
        }
    }

    fn pre(code: i32, stdout: &str, stderr: &str) -> HookResponse {
        interpret(
            HookEvent::PreToolUse,
            "guard.sh",
            exit(code, stdout, stderr),
        )
    }

    #[test]
    fn exit_two_blocks_with_standard_error() {
        let response = pre(2, "", "rm -rf is not allowed\n");
        assert_eq!(response.outcome, HookRunOutcome::Blocking);
        assert_eq!(response.permission, Some(HookPermission::Deny));
        assert_eq!(
            response.blocking_error.as_deref(),
            Some("[guard.sh]: rm -rf is not allowed")
        );
        assert_eq!(
            pre(2, "", "").blocking_error.as_deref(),
            Some("[guard.sh]: No stderr output")
        );
    }

    #[test]
    fn other_statuses_only_tell_the_user_unless_their_json_answer_decides() {
        let failed = pre(1, "not json", "boom");
        assert_eq!(failed.outcome, HookRunOutcome::NonBlockingError);
        assert_eq!(failed.permission, None);
        assert_eq!(
            failed.error.as_deref(),
            Some("Failed with non-blocking status code: boom")
        );
        let answered = pre(1, r#"{"decision":"block","reason":"no"}"#, "boom");
        assert_eq!(answered.outcome, HookRunOutcome::Blocking);
        assert_eq!(answered.permission, Some(HookPermission::Deny));
        assert_eq!(answered.permission_reason.as_deref(), Some("no"));
        assert!(answered.blocking_error.is_none());
        let quiet = pre(1, r#"{"systemMessage":"careful"}"#, "boom");
        assert_eq!(quiet.outcome, HookRunOutcome::NonBlockingError);
        assert_eq!(quiet.system_message.as_deref(), Some("careful"));
        let timed_out = interpret(
            HookEvent::PostToolUse,
            "slow",
            HookExit {
                code: None,
                timed_out: true,
                stdout: "",
                stderr: "",
            },
        );
        assert_eq!(timed_out.outcome, HookRunOutcome::TimedOut);
    }

    #[test]
    fn json_answers_carry_the_pre_tool_use_permission() {
        let deny = pre(
            0,
            &json!({"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"no"}}).to_string(),
            "",
        );
        assert_eq!(deny.permission, Some(HookPermission::Deny));
        assert_eq!(deny.permission_reason.as_deref(), Some("no"));
        assert!(deny.blocking_error.is_none());
        assert_eq!(deny.outcome, HookRunOutcome::Blocking);
        let ask = pre(
            0,
            &json!({"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"ask","permissionDecisionReason":"check","updatedInput":{"command":"ls"}}}).to_string(),
            "",
        );
        assert_eq!(ask.permission, Some(HookPermission::Ask));
        assert_eq!(ask.permission_reason.as_deref(), Some("check"));
        assert_eq!(ask.updated_input, Some(json!({"command":"ls"})));
    }

    #[test]
    fn the_older_decision_field_maps_to_allow_and_deny() {
        let approve = pre(0, r#"{"decision":"approve","reason":"fine"}"#, "");
        assert_eq!(approve.permission, Some(HookPermission::Allow));
        let block = pre(
            0,
            r#"{"decision":"block","reason":"nope","systemMessage":"Blocked"}"#,
            "",
        );
        assert_eq!(block.permission, Some(HookPermission::Deny));
        assert_eq!(block.permission_reason.as_deref(), Some("nope"));
        assert!(block.blocking_error.is_none());
        assert_eq!(block.system_message.as_deref(), Some("Blocked"));
        let unknown = pre(0, r#"{"decision":"maybe"}"#, "");
        assert_eq!(unknown.outcome, HookRunOutcome::NonBlockingError);
    }

    #[test]
    fn a_mismatched_event_name_or_broken_json_is_an_error() {
        let mismatched = interpret(
            HookEvent::PostToolUse,
            "fmt",
            exit(
                0,
                r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","additionalContext":"x"}}"#,
                "",
            ),
        );
        assert_eq!(mismatched.outcome, HookRunOutcome::NonBlockingError);
        assert!(mismatched.additional_context.is_none());
        let broken = pre(0, "{\"decision\": }", "");
        assert_eq!(broken.outcome, HookRunOutcome::NonBlockingError);
        let plain = pre(0, "formatted 3 files\n", "");
        assert_eq!(plain.outcome, HookRunOutcome::Success);
        assert_eq!(plain.plain_text.as_deref(), Some("formatted 3 files"));
    }

    #[test]
    fn a_first_line_with_async_true_asks_for_the_background_and_the_rest_answers() {
        let answer = "{\"systemMessage\":\"ready\"}\n";
        assert_eq!(
            after_background_request(&format!(
                "\n{{\"async\": true, \"asyncTimeout\": 180000}}\n{answer}"
            )),
            Some(answer)
        );
        assert_eq!(after_background_request("{\"async\":true}"), Some(""));
        for stdout in [
            "{\"async\": false}\n{}",
            "{\"systemMessage\":\"async\"}",
            "[true]\n",
            "async\n",
            "",
        ] {
            assert_eq!(after_background_request(stdout), None, "{stdout}");
        }
    }

    async fn asks_as_it_arrives(chunks: &[&[u8]], ends: bool) -> Option<bool> {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        for chunk in chunks {
            sender.send(chunk.to_vec()).unwrap();
        }
        if ends {
            drop(sender);
            return Some(first_line_asks_for_background(receiver).await);
        }
        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            first_line_asks_for_background(receiver),
        )
        .await
        .ok()
    }

    #[tokio::test]
    async fn the_first_line_is_read_as_it_arrives() {
        assert_eq!(
            asks_as_it_arrives(&[b"{\"as", b"ync\": true}\n"], false).await,
            Some(true)
        );
        assert_eq!(
            asks_as_it_arrives(&[b"{\"async\": true}"], false).await,
            Some(true),
            "a complete request counts before its line ends"
        );
        assert_eq!(
            asks_as_it_arrives(&[b"{\"decision\": \"block\"}\n{\"async\": true}\n"], false).await,
            Some(false)
        );
        assert_eq!(
            asks_as_it_arrives(&[b"working"], false).await,
            None,
            "an unfinished line waits"
        );
        assert_eq!(asks_as_it_arrives(&[b"working"], true).await, Some(false));
        let long = vec![b' '; BACKGROUND_REQUEST_BYTES + 1];
        assert_eq!(asks_as_it_arrives(&[&long], false).await, Some(false));
    }

    #[test]
    fn continue_false_stops_with_its_reason() {
        let stopped = interpret(
            HookEvent::PostToolUse,
            "stop",
            exit(0, r#"{"continue":false,"stopReason":"Done for today"}"#, ""),
        );
        assert!(stopped.prevent_continuation);
        assert_eq!(stopped.stop_reason.as_deref(), Some("Done for today"));
    }

    #[test]
    fn merging_ranks_deny_over_ask_over_a_users_allow() {
        let user = HookSource::User;
        let plugin = HookSource::Plugin {
            id: "guard@market".into(),
        };
        let allow = pre(0, r#"{"decision":"approve"}"#, "");
        let ask = pre(
            0,
            r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"ask"}}"#,
            "",
        );
        let deny = pre(2, "", "blocked");
        let (verdict, _) = HookVerdict::merge([(&user, &allow), (&plugin, &ask)]);
        assert_eq!(verdict.permission, Some(HookPermission::Ask));
        let (verdict, _) = HookVerdict::merge([(&user, &deny), (&plugin, &ask), (&user, &allow)]);
        assert_eq!(verdict.permission, Some(HookPermission::Deny));
        let (verdict, _) = HookVerdict::merge([(&plugin, &allow)]);
        assert_eq!(verdict.permission, None);
        assert_eq!(allow.decision(&plugin), Some(HookDecision::IgnoredAllow));
        let (verdict, _) = HookVerdict::merge([(&user, &allow)]);
        assert_eq!(verdict.permission, Some(HookPermission::Allow));
    }

    #[test]
    fn a_users_allow_does_not_admit_input_another_source_rewrote() {
        let user = HookSource::User;
        let repository = HookSource::Repository;
        let allow = pre(0, r#"{"decision":"approve"}"#, "");
        let rewrite = pre(
            0,
            r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","updatedInput":{"command":"b"}}}"#,
            "",
        );
        let (verdict, _) = HookVerdict::merge([(&user, &allow), (&repository, &rewrite)]);
        assert_eq!(verdict.permission, None);
        assert_eq!(verdict.updated_input, Some(json!({"command":"b"})));
        let (verdict, _) = HookVerdict::merge([(&user, &allow), (&user, &rewrite)]);
        assert_eq!(verdict.permission, Some(HookPermission::Allow));
    }

    #[test]
    fn the_last_rewrite_in_configuration_order_wins_with_a_warning() {
        let user = HookSource::User;
        let first = pre(
            0,
            r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","updatedInput":{"command":"a"}}}"#,
            "",
        );
        let second = pre(
            0,
            r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","updatedInput":{"command":"b"}}}"#,
            "",
        );
        let (verdict, warning) = HookVerdict::merge([(&user, &first), (&user, &second)]);
        assert_eq!(verdict.updated_input, Some(json!({"command":"b"})));
        assert!(warning.is_some());
    }
}

//! `PreToolUse`, `PostToolUse` and `PostToolUseFailure` in the tool call pipeline.
//!
//! `PreToolUse` runs before the call is prepared: a deny refuses it unprepared, an ask sends it
//! to a person, a user hook's allow admits it after the deterministic checks, and an
//! `updatedInput` is what the call is prepared with. What the hooks tell the model joins the
//! call's result, after it, as Claude Code's hook attachments follow the result there.

use serde_json::{Map, Value, json};
use tickloop_harness_protocol::hooks::HookEvent;
use tickloop_harness_protocol::{Receipt, ToolResult};
use tokio_util::sync::CancellationToken;

use super::{HookCall, HookPermission, HookVerdict};
use crate::Agent;

const SYSTEM_REMINDER_OPENING: &str = "<system-reminder>\n";
const SYSTEM_REMINDER_CLOSING: &str = "\n</system-reminder>";
/// Claude Code's stop reason when a hook ends the turn without giving one.
const DEFAULT_STOP_REASON: &str = "Execution stopped by hook";

/// Text for the model that follows a call's result.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct ToolHookFeedback(Vec<String>);

impl ToolHookFeedback {
    pub(crate) fn extend(&mut self, other: Self) {
        self.0.extend(other.0);
    }

    /// `result` followed by each piece of feedback as a system reminder.
    pub(crate) fn appended_to(&self, result: String) -> String {
        self.0.iter().fold(result, |mut text, feedback| {
            text.push_str("\n\n");
            text.push_str(SYSTEM_REMINDER_OPENING);
            text.push_str(feedback);
            text.push_str(SYSTEM_REMINDER_CLOSING);
            text
        })
    }

    fn context(event: HookEvent, tool: &str, verdict: &HookVerdict) -> Self {
        Self(
            verdict
                .additional_context
                .iter()
                .map(|context| {
                    format!("{}:{tool} hook additional context: {context}", event.name())
                })
                .collect(),
        )
    }
}

/// What `PreToolUse` lets the call do.
pub(crate) enum PreToolGate {
    /// The call goes on with `input`, the hooks' rewrite when they gave one.
    Proceed {
        input: Value,
        /// `Ask` sends the call to a person; `Allow`, only ever from the user's own hooks,
        /// admits it after the deterministic checks.
        permission: Option<HookPermission>,
        reason: Option<String>,
        feedback: ToolHookFeedback,
    },
    /// A hook denied the call, or ended the turn: the call's result.
    Refused {
        message: String,
        feedback: ToolHookFeedback,
    },
    /// The turn was interrupted or stopped while the hooks ran: the call does not run.
    Cancelled,
}

fn tool_fields(tool: &str, call_id: &str, input: &Value) -> Map<String, Value> {
    let mut fields = Map::new();
    fields.insert("tool_name".into(), json!(tool));
    fields.insert("tool_input".into(), input.clone());
    fields.insert("tool_use_id".into(), json!(call_id));
    fields
}

/// A tool's result as hooks read it: its JSON when it is JSON, else its text.
fn tool_response(result: &ToolResult) -> Value {
    serde_json::from_str(&result.content).unwrap_or_else(|_| json!(result.content))
}

impl Agent {
    pub(crate) async fn pre_tool_use(
        &self,
        receipt: &Receipt,
        call_id: &str,
        tool: &str,
        input: Value,
        cancel: &CancellationToken,
    ) -> PreToolGate {
        let verdict = self
            .run_hooks(
                receipt,
                HookCall {
                    event: HookEvent::PreToolUse,
                    match_query: Some(tool.into()),
                    fields: tool_fields(tool, call_id, &input),
                    tool: Some((tool.into(), call_id.into())),
                },
                cancel,
            )
            .await;
        if verdict.cancelled || cancel.is_cancelled() {
            return PreToolGate::Cancelled;
        }
        let feedback = ToolHookFeedback::context(HookEvent::PreToolUse, tool, &verdict);
        if verdict.prevent_continuation {
            self.stop_after_batch(receipt).await;
            return PreToolGate::Refused {
                message: format!(
                    "PreToolUse:{tool} hook stopped continuation: {}",
                    verdict
                        .stop_reason
                        .as_deref()
                        .unwrap_or(DEFAULT_STOP_REASON)
                ),
                feedback,
            };
        }
        if verdict.permission == Some(HookPermission::Deny) {
            let message = verdict
                .blocking_errors
                .iter()
                .map(|error| format!("PreToolUse:{tool} hook error: {error}"))
                .collect::<Vec<_>>()
                .join("\n");
            let message = if message.is_empty() {
                format!("Hook PreToolUse:{tool} denied this tool")
            } else {
                message
            };
            return PreToolGate::Refused { message, feedback };
        }
        PreToolGate::Proceed {
            input: verdict.updated_input.clone().unwrap_or(input),
            permission: verdict.permission,
            reason: verdict.permission_reason.clone(),
            feedback,
        }
    }

    /// Runs `PostToolUse` after a call that ran and succeeded, `PostToolUseFailure` after one
    /// that ran and failed; never again for the same call, whatever its outcome.
    pub(crate) async fn post_tool_use(
        &self,
        receipt: &Receipt,
        call_id: &str,
        tool: &str,
        input: &Value,
        result: &ToolResult,
        cancel: &CancellationToken,
    ) -> ToolHookFeedback {
        let event = if result.is_error {
            HookEvent::PostToolUseFailure
        } else {
            HookEvent::PostToolUse
        };
        let mut fields = tool_fields(tool, call_id, input);
        if result.is_error {
            fields.insert("error".into(), json!(result.content));
            fields.insert("is_interrupt".into(), json!(false));
        } else {
            fields.insert("tool_response".into(), tool_response(result));
        }
        let verdict = self
            .run_hooks(
                receipt,
                HookCall {
                    event,
                    match_query: Some(tool.into()),
                    fields,
                    tool: Some((tool.into(), call_id.into())),
                },
                cancel,
            )
            .await;
        let mut feedback = ToolHookFeedback::default();
        if event == HookEvent::PostToolUse {
            feedback.0.extend(
                verdict
                    .blocking_errors
                    .iter()
                    .map(|error| format!("PostToolUse:{tool} hook blocking error: {error}")),
            );
        }
        feedback.extend(ToolHookFeedback::context(event, tool, &verdict));
        if verdict.prevent_continuation {
            self.stop_after_batch(receipt).await;
            feedback.0.push(format!(
                "{}:{tool} hook stopped continuation: {}",
                event.name(),
                verdict
                    .stop_reason
                    .as_deref()
                    .unwrap_or(DEFAULT_STOP_REASON)
            ));
        }
        feedback
    }

    /// Ends the Run once its current batch of calls is done, as a hook's `continue: false` asks.
    async fn stop_after_batch(&self, receipt: &Receipt) {
        if let Some(chat) = self.inner.chats.lock().await.get_mut(&receipt.chat_id) {
            chat.hook_state.stopped_runs.insert(receipt.run_id);
        }
    }
}

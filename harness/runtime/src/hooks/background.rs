//! Hooks declared `async` or `asyncRewake`, run in the background as Claude Code 2.1.302 runs
//! them: their event goes on at once, and nothing they print decides anything; a decision is
//! recorded as ignored. Each run is recorded as it starts and as it ends. A running hook whose
//! first line of output is `{"async": true}` goes on from there as an `async` hook, still under
//! its timeout from when it started.
//!
//! An `async` hook's `systemMessage` and `additionalContext` join the end of the conversation
//! for the model's next request without starting a turn. An `asyncRewake` hook that exits 2
//! notifies the Loop with its output the way a finished background command does: a running turn
//! reads it after its current tool results, and an idle Loop starts a turn for it. Any other
//! ending only shows the user. A background hook keeps its timeout; Esc leaves it running, while
//! `/stop` and shutdown end it.

use std::sync::atomic::Ordering;
use std::time::Instant;

use tickloop_harness_protocol::hooks::{
    HookBackground, HookBackgroundPhase, HookDecision, HookEvent, HookRunOutcome,
};
use tickloop_harness_protocol::{Message, Receipt, local_messages};
use tokio_util::sync::CancellationToken;

use super::{
    ChatHookState, CommandRun, FinishedRun, HookCall, HookContext, HookResponse, Runnable,
    record_hook_run, run_record,
};
use crate::Agent;

/// The exit status with which an `asyncRewake` hook wakes the model.
const REWAKE_EXIT_CODE: i32 = 2;
/// Claude Code's `rewakeSummary` when a hook declares none.
const DEFAULT_REWAKE_SUMMARY: &str = "Stop hook feedback";

impl ChatHookState {
    fn background_cancel(&mut self, shutdown: &CancellationToken) -> CancellationToken {
        self.background
            .get_or_insert_with(|| shutdown.child_token())
            .clone()
    }

    /// Ends the hooks the Chat runs in the background, as `/stop` does.
    pub(crate) fn cancel_background(&mut self) {
        if let Some(cancel) = self.background.take() {
            cancel.cancel();
        }
    }
}

/// `PostToolUse:Bash`, or the event alone outside tool calls, as Claude Code names a hook.
fn hook_label(event: HookEvent, tool: Option<&(String, String)>) -> String {
    match tool {
        Some((name, _)) => format!("{}:{name}", event.name()),
        None => event.name().to_owned(),
    }
}

fn decision_name(decision: HookDecision) -> &'static str {
    match decision {
        HookDecision::Allow | HookDecision::IgnoredAllow => "allow",
        HookDecision::Ask => "ask",
        HookDecision::Deny => "deny",
        HookDecision::Block => "block",
    }
}

/// What an `async` hook gives the model's next request: its `systemMessage` and
/// `additionalContext`, as Claude Code attaches them; `None` when it gave neither.
fn async_notification(label: &str, response: &HookResponse) -> Option<Message> {
    let lines = [
        ("system message", &response.system_message),
        ("additional context", &response.additional_context),
    ]
    .into_iter()
    .filter_map(|(kind, text)| {
        text.as_ref()
            .map(|text| format!("{label} hook {kind}: {text}"))
    })
    .collect::<Vec<_>>();
    (!lines.is_empty()).then(|| {
        local_messages::background_hook_notification(
            &format!("{label} hook finished in the background"),
            &lines.join("\n"),
        )
    })
}

/// What an `asyncRewake` hook that exited 2 tells the model: its `rewakeMessage`, or Claude
/// Code's default, then its standard error, else its standard output.
fn rewake_notification(
    label: &str,
    message: Option<&str>,
    summary: Option<&str>,
    output: &str,
) -> Message {
    let lead = message.map_or_else(
        || format!("Stop hook blocking error from command \"{label}\":"),
        str::to_owned,
    );
    local_messages::background_hook_notification(
        summary.unwrap_or(DEFAULT_REWAKE_SUMMARY),
        &format!("{lead} {output}"),
    )
}

impl Agent {
    /// Records that `runnable` goes on in the background for `call` and returns what ends it
    /// there; `None` when the Chat is gone.
    async fn record_background_start(
        &self,
        receipt: &Receipt,
        call: &HookCall,
        runnable: &Runnable,
    ) -> Option<CancellationToken> {
        let mut chats = self.inner.chats.lock().await;
        let chat = chats.get_mut(&receipt.chat_id)?;
        let cancel = chat.hook_state.background_cancel(&self.inner.shutdown);
        let mut record = run_record(
            receipt,
            call.event,
            call.tool.as_ref(),
            &runnable.hook,
            &FinishedRun::unfinished(HookResponse::success(), Instant::now()),
            None,
        );
        record.background = Some(HookBackgroundPhase::Started);
        record_hook_run(self, chat, receipt, record);
        self.save(&chat.snapshot);
        Some(cancel)
    }

    /// Starts `runnable` in the background for `call` and records its start; its event does not
    /// wait for it.
    pub(super) async fn start_background_hook(
        &self,
        receipt: &Receipt,
        context: &HookContext,
        call: &HookCall,
        runnable: Runnable,
        stdin: Vec<u8>,
    ) {
        let Some(background) = runnable.hook.background.clone() else {
            return;
        };
        let Some(cancel) = self.record_background_start(receipt, call, &runnable).await else {
            return;
        };
        let agent = self.clone();
        let receipt = receipt.clone();
        let context = context.clone();
        let event = call.event;
        let tool = call.tool.clone();
        self.inner.tasks.spawn(async move {
            let run = agent
                .run_one_hook(&receipt, &context, event, &runnable, &stdin, &cancel)
                .await;
            agent
                .finish_background_hook(&receipt, event, tool.as_ref(), &runnable, &background, run)
                .await;
        });
    }

    /// Lets the rest of a hook run that asked for it go on in the background as an `async`
    /// hook's run: records its start, and its end once `execution` ends; `/stop` and shutdown
    /// end it through `execution_cancel`.
    pub(super) async fn continue_in_background(
        &self,
        receipt: &Receipt,
        call: &HookCall,
        runnable: &Runnable,
        started: Instant,
        mut execution: CommandRun,
        execution_cancel: CancellationToken,
    ) {
        let Some(cancel) = self.record_background_start(receipt, call, runnable).await else {
            execution_cancel.cancel();
            return;
        };
        let agent = self.clone();
        let receipt = receipt.clone();
        let runnable = runnable.clone();
        let event = call.event;
        let tool = call.tool.clone();
        self.inner.tasks.spawn(async move {
            let ending = tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    execution_cancel.cancel();
                    execution.await
                }
                ending = &mut execution => ending,
            };
            let run = FinishedRun::read(event, &runnable, started, ending, cancel.is_cancelled());
            agent
                .finish_background_hook(
                    &receipt,
                    event,
                    tool.as_ref(),
                    &runnable,
                    &HookBackground::Async,
                    run,
                )
                .await;
        });
    }

    /// Records how a background run ended and tells the model what it has to say.
    async fn finish_background_hook(
        &self,
        receipt: &Receipt,
        event: HookEvent,
        tool: Option<&(String, String)>,
        runnable: &Runnable,
        background: &HookBackground,
        run: FinishedRun,
    ) {
        if self.inner.closed.load(Ordering::SeqCst) {
            return;
        }
        let label = hook_label(event, tool);
        let response = &run.response;
        let rewakes = matches!(background, HookBackground::AsyncRewake { .. })
            && response.outcome == HookRunOutcome::Blocking
            && run.exit_code == Some(REWAKE_EXIT_CODE);
        let reason = match response.decision(&runnable.hook.source) {
            Some(decision) if !rewakes => Some(format!(
                "{} ignored: the hook ran in the background{}",
                decision_name(decision),
                response
                    .reason()
                    .map(|reason| format!(" ({reason})"))
                    .unwrap_or_default()
            )),
            _ => response.reason(),
        };
        let mut record = run_record(receipt, event, tool, &runnable.hook, &run, reason);
        record.background = Some(HookBackgroundPhase::Ended);
        record.decision = None;
        if record.outcome == HookRunOutcome::Blocking && !rewakes {
            record.outcome = match run.exit_code {
                Some(0) => HookRunOutcome::Success,
                _ => HookRunOutcome::NonBlockingError,
            };
        }
        let notification = match background {
            HookBackground::Async => async_notification(&label, response),
            HookBackground::AsyncRewake { message, summary } if rewakes => {
                Some(rewake_notification(
                    &label,
                    message.as_deref(),
                    summary.as_deref(),
                    &run.blocking_output,
                ))
            }
            HookBackground::AsyncRewake { .. } => None,
        };
        let wakes = {
            let mut chats = self.inner.chats.lock().await;
            let Some(chat) = chats.get_mut(&receipt.chat_id) else {
                return;
            };
            record_hook_run(self, chat, receipt, record);
            let mut wakes = false;
            if let Some(notification) = notification {
                if rewakes {
                    chat.hook_state.session.note_rewake(notification.clone());
                }
                match self.append_local_messages(chat, [notification]) {
                    Ok(()) => {
                        wakes = rewakes && self.queue_task_continuation(chat, Some(receipt.loop_id))
                    }
                    Err(error) => tracing::warn!("{label} background hook not delivered: {error}"),
                }
            }
            self.save(&chat.snapshot);
            wakes
        };
        if wakes && let Err(error) = self.start_next_agent_run(receipt.chat_id).await {
            tracing::warn!("{label} background hook could not wake the Loop: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(message: &Message) -> String {
        match &message.content[0] {
            tickloop_harness_protocol::Content::Text { text } => text.clone(),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_rewake_reads_as_claude_codes_stop_hook_feedback_unless_the_hook_words_it() {
        let label = hook_label(
            HookEvent::PostToolUse,
            Some(&("Bash".into(), "call".into())),
        );
        let default = text(&rewake_notification(&label, None, None, "<found> a token"));
        assert!(
            default.contains("<summary>Stop hook feedback</summary>"),
            "{default}"
        );
        assert!(
            default.contains(
                "Stop hook blocking error from command \"PostToolUse:Bash\": &lt;found&gt; a token"
            ),
            "{default}"
        );
        let worded = text(&rewake_notification(
            &label,
            Some("Review:"),
            Some("Review found issues"),
            "a token",
        ));
        assert!(worded.contains("<summary>Review found issues</summary>"));
        assert!(worded.contains("<result>Review: a token</result>"));
    }

    #[test]
    fn an_async_hook_tells_the_model_only_its_message_and_context() {
        let mut response = HookResponse::success();
        assert!(async_notification("Stop", &response).is_none());
        response.system_message = Some("formatted".into());
        response.additional_context = Some("2 files changed".into());
        response.plain_text = Some("noise".into());
        let notification = text(&async_notification("Stop", &response).unwrap());
        assert!(notification.contains("Stop hook system message: formatted"));
        assert!(notification.contains("Stop hook additional context: 2 files changed"));
        assert!(!notification.contains("noise"));
    }
}

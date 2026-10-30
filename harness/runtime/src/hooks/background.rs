//! Hooks declared `async` or `asyncRewake`, run in the background as Claude Code 2.1.302 runs
//! them: their event goes on at once, and nothing they print decides anything; a decision is
//! recorded as ignored. Each run is recorded as it starts and as it ends. A running hook whose
//! first line of output is `{"async": true}` goes on from there as an `async` hook, still under
//! its timeout from when it started.
//!
//! An `async` hook's `systemMessage` and `additionalContext` join the end of the conversation
//! for the model's next request without starting a turn. An `asyncRewake` hook that exits 2
//! notifies the Loop with its output the way a finished background command does: a running turn
//! reads it after its current tool results, and an idle Loop starts a turn for it. Either
//! notification shows the Chat's views its summary line as the hook ends, as a finished
//! background command's does. Any other ending only shows the user. A background hook keeps its
//! timeout; Esc leaves it running, while `/stop` and shutdown end it. An event that Esc or `/stop`
//! ended before its background hooks started starts none of them, as Claude Code starts none once
//! its event was aborted.
//!
//! `SessionEnd` hooks run as the Worker ends, after shutdown ended every Chat's background hooks:
//! Claude Code 2.1.302 still starts an `async` one, which outlives its process, while the Worker
//! ends every command it started as it exits. They therefore run within the event's own budget,
//! and the Worker waits for them with its other `SessionEnd` hooks; what they print tells no one.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use tickloop_harness_protocol::hooks::{
    HookBackground, HookBackgroundPhase, HookDecision, HookEvent, HookRunOutcome,
};
use tickloop_harness_protocol::{Message, Receipt, TaskNotification, local_messages};
use tokio_util::sync::CancellationToken;
use tokio_util::task::task_tracker::TaskTrackerToken;

use super::{
    ChatHookState, CommandRun, FinishedRun, HookCall, HookContext, HookResponse, Runnable,
    record_hook_run, run_record,
};
use crate::Agent;

/// The exit status with which an `asyncRewake` hook wakes the model.
const REWAKE_EXIT_CODE: i32 = 2;
const MANAGED_HOOK_CLEANUP_BUDGET: Duration = Duration::from_secs(10);
/// Claude Code's `rewakeSummary` when a hook declares none.
const DEFAULT_REWAKE_SUMMARY: &str = "Stop hook feedback";

impl ChatHookState {
    pub(super) fn background_cancel(&mut self, shutdown: &CancellationToken) -> CancellationToken {
        self.background
            .get_or_insert_with(|| shutdown.child_token())
            .clone()
    }

    /// Ends the hooks the Chat runs in the background, as `/stop` does; the events under way
    /// start none after it.
    pub(crate) fn cancel_background(&mut self) {
        self.background_generation += 1;
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

/// What a background hook's end tells the model, under the summary the user sees for it.
struct HookNotice {
    summary: String,
    result: String,
}

impl HookNotice {
    fn message(&self) -> Message {
        local_messages::background_hook_notification(&self.summary, &self.result)
    }
}

/// What an `async` hook gives the model's next request: its `systemMessage` and
/// `additionalContext`, as Claude Code attaches them; `None` when it gave neither.
fn async_notification(label: &str, response: &HookResponse) -> Option<HookNotice> {
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
    (!lines.is_empty()).then(|| HookNotice {
        summary: format!("{label} hook finished in the background"),
        result: lines.join("\n"),
    })
}

/// What an `asyncRewake` hook that exited 2 tells the model: its `rewakeMessage`, or Claude
/// Code's default, then its standard error, else its standard output, under its `rewakeSummary`.
fn rewake_notification(
    label: &str,
    message: Option<&str>,
    summary: Option<&str>,
    output: &str,
) -> HookNotice {
    let lead = message.map_or_else(
        || format!("Stop hook blocking error from command \"{label}\":"),
        str::to_owned,
    );
    HookNotice {
        summary: summary.unwrap_or(DEFAULT_REWAKE_SUMMARY).to_owned(),
        result: format!("{lead} {output}"),
    }
}

/// A background hook that was started and has yet to run to its end.
pub(super) struct BackgroundRun {
    agent: Agent,
    receipt: Receipt,
    context: HookContext,
    event: HookEvent,
    tool: Option<(String, String)>,
    runnable: Runnable,
    background: HookBackground,
    stdin: Vec<u8>,
    cancel: CancellationToken,
    _tracked: TaskTrackerToken,
}

impl BackgroundRun {
    /// Runs the hook to its end, then records how it ended and tells the model what it has to
    /// say.
    pub(super) async fn finish(self) {
        let run = self
            .agent
            .run_one_hook(
                &self.receipt,
                &self.context,
                self.event,
                &self.runnable,
                &self.stdin,
                &self.cancel,
            )
            .await;
        self.agent
            .finish_background_hook(
                &self.receipt,
                self.event,
                self.tool.as_ref(),
                &self.runnable,
                &self.background,
                run,
                self.context.background_generation,
            )
            .await;
    }
}

impl Agent {
    pub(crate) async fn settle_managed_hooks(
        &self,
        chat_id: tickloop_harness_protocol::ChatId,
    ) -> Result<(), String> {
        let tasks = {
            let mut chats = self.inner.chats.lock().await;
            let chat = chats.get_mut(&chat_id).ok_or("Chat is unavailable")?;
            chat.hook_state.cancel_background();
            chat.hook_state.background_tasks.close();
            chat.hook_state.background_tasks.clone()
        };
        tokio::time::timeout(MANAGED_HOOK_CLEANUP_BUDGET, tasks.wait())
            .await
            .map_err(|_| "background Hook cleanup exceeded 10 seconds".to_owned())?;
        let mut chats = self.inner.chats.lock().await;
        let chat = chats.get_mut(&chat_id).ok_or("Chat is unavailable")?;
        if std::mem::take(&mut chat.hook_state.command_outcome_unknown) {
            return Err("Hook command outcome unknown; execution was not retried".into());
        }
        Ok(())
    }

    /// Records that `runnable` goes on in the background for `call` and returns what ends it
    /// there: for `SessionEnd`, `cancel`, the event's own budget; for any other event, the
    /// Chat's background lifetime. `None` when the Chat is gone, the event was ended or stopped
    /// before the hook could go on, or the Worker is ending.
    async fn record_background_start(
        &self,
        receipt: &Receipt,
        context: &HookContext,
        call: &HookCall,
        runnable: &Runnable,
        cancel: &CancellationToken,
    ) -> Option<(CancellationToken, TaskTrackerToken)> {
        let mut chats = self.inner.chats.lock().await;
        let chat = chats.get_mut(&receipt.chat_id)?;
        // Checked under the lock `/stop` takes, so an event it ended, or one prepared before it,
        // never starts a hook with a lifetime of its own after it.
        if cancel.is_cancelled()
            || chat.hook_state.background_generation != context.background_generation
        {
            return None;
        }
        let lifetime = if call.event == HookEvent::SessionEnd {
            cancel.clone()
        } else if self.inner.closed.load(Ordering::SeqCst) {
            return None;
        } else {
            chat.hook_state.background_cancel(&self.inner.shutdown)
        };
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
        Some((lifetime, chat.hook_state.background_tasks.token()))
    }

    /// Starts `runnable` in the background for `call` and records its start; its event does not
    /// wait for it. `None` when it may not start, as [`Self::record_background_start`] tells.
    pub(super) async fn start_background_hook(
        &self,
        receipt: &Receipt,
        context: &HookContext,
        call: &HookCall,
        runnable: Runnable,
        stdin: &[u8],
        cancel: &CancellationToken,
    ) -> Option<BackgroundRun> {
        let background = runnable.hook.background.clone()?;
        let (lifetime, tracked) = self
            .record_background_start(receipt, context, call, &runnable, cancel)
            .await?;
        Some(BackgroundRun {
            agent: self.clone(),
            receipt: receipt.clone(),
            context: context.clone(),
            event: call.event,
            tool: call.tool.clone(),
            runnable,
            background,
            stdin: stdin.to_vec(),
            cancel: lifetime,
            _tracked: tracked,
        })
    }

    /// Lets the rest of a hook run that asked for it go on in the background as an `async`
    /// hook's run: records its start, and its end once `execution` ends; `/stop` and shutdown
    /// end it through `execution_cancel`. A `SessionEnd` hook's rest runs within the event's
    /// budget, and its event waits for it.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn continue_in_background(
        &self,
        receipt: &Receipt,
        context: &HookContext,
        call: &HookCall,
        runnable: &Runnable,
        started: Instant,
        mut execution: CommandRun,
        execution_cancel: CancellationToken,
        cancel: &CancellationToken,
    ) {
        let Some((lifetime, tracked)) = self
            .record_background_start(receipt, context, call, runnable, cancel)
            .await
        else {
            execution_cancel.cancel();
            let _ = execution.await;
            return;
        };
        let agent = self.clone();
        let receipt = receipt.clone();
        let runnable = runnable.clone();
        let event = call.event;
        let tool = call.tool.clone();
        let generation = context.background_generation;
        let rest = async move {
            let _tracked = tracked;
            let ending = tokio::select! {
                biased;
                () = lifetime.cancelled() => {
                    execution_cancel.cancel();
                    execution.await
                }
                ending = &mut execution => ending,
            };
            let run = FinishedRun::read(event, &runnable, started, ending, lifetime.is_cancelled());
            agent
                .finish_background_hook(
                    &receipt,
                    event,
                    tool.as_ref(),
                    &runnable,
                    &HookBackground::Async,
                    run,
                    generation,
                )
                .await;
        };
        if event == HookEvent::SessionEnd {
            rest.await;
        } else {
            self.inner.tasks.spawn(rest);
        }
    }

    /// Records how a background run ended and, unless the Worker is ending, tells the model
    /// what it has to say.
    #[allow(clippy::too_many_arguments)]
    async fn finish_background_hook(
        &self,
        receipt: &Receipt,
        event: HookEvent,
        tool: Option<&(String, String)>,
        runnable: &Runnable,
        background: &HookBackground,
        run: FinishedRun,
        generation: u64,
    ) {
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
        let notice = match background {
            _ if self.inner.closed.load(Ordering::SeqCst) => None,
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
            if let Some(notice) = notice
                && chat.hook_state.background_generation == generation
            {
                let message = notice.message();
                if rewakes {
                    chat.hook_state.session.note_rewake(message.clone());
                }
                let shown = TaskNotification::for_background_hook(receipt.chat_id, &notice.summary);
                match self.show_task_notification(chat, shown, message) {
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
    use crate::{HarnessOptions, HarnessSeams, ToolPolicy};
    use std::sync::Arc;
    use tickloop_harness_protocol::AgentHostOptions;
    use tickloop_harness_test_support::{ScriptedModel, config, harness::ChosenModels};

    fn text(notice: &HookNotice) -> String {
        match &notice.message().content[0] {
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

    #[tokio::test(start_paused = true)]
    async fn managed_settlement_fails_until_a_cancelled_hook_confirms_completion() {
        let agent = Agent::with_seams(
            HarnessSeams {
                search: None,
                transcripts: None,
                environment: Some(Arc::new(
                    crate::environment::memory::MemoryEnvironment::new(),
                )),
                models: ChosenModels::new(Arc::new(ScriptedModel::new(Vec::new()))),
            },
            HarnessOptions {
                tools: ToolPolicy::acp(),
                host: AgentHostOptions::embedded_defaults(),
                ..HarnessOptions::local(config())
            },
        )
        .unwrap();
        let folder = tempfile::tempdir().unwrap();
        let chat_id = agent.create_chat(folder.path().to_owned()).await.unwrap();
        let (cancel, pending) = {
            let mut chats = agent.inner.chats.lock().await;
            let chat = chats.get_mut(&chat_id).unwrap();
            (
                chat.hook_state.background_cancel(&agent.inner.shutdown),
                chat.hook_state.background_tasks.token(),
            )
        };
        let started = tokio::time::Instant::now();
        let error = agent.settle_managed(chat_id, false).await.unwrap_err();
        assert!(cancel.is_cancelled());
        assert!(
            error
                .to_string()
                .contains("background Hook cleanup exceeded 10 seconds"),
            "{error}"
        );
        assert!(started.elapsed() >= MANAGED_HOOK_CLEANUP_BUDGET);
        drop(pending);
        agent.settle_managed(chat_id, false).await.unwrap();
        agent.shutdown().await;
    }
}

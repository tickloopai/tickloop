use std::{io::Write, time::Duration};

use serde_json::json;
use tickloop_agent_client::managed::{ManagedClient, ManagedSession};
use tickloop_agent_protocol::{
    AccessError, AccessErrorCode, Event, EventKind, LaunchSpec, LiveChatSnapshot, LoopState,
    Observation, ObservationChange, ObservationValue, OperationState, Receipt, ResetReason,
    UserInput,
};
use tokio::sync::mpsc;

use crate::{Options, PrintResult, output::Output};

// Runtime settlement includes bounded worker/tool cancellation (up to 65s). Allow that
// contract to complete rather than dropping its RPC while it still holds the execution gate.
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(90);

/// Injectable adapter entry. All mutations go through the managed client, never a model/runtime.
/// `shutdown` yields 130 (SIGINT) or 143 (SIGTERM) per signal: the first cancels the run, the next
/// skips the remaining cleanup. A closed channel disables signals in tests.
/// The injected writer must be bounded; production uses `BoundedOutput`.
pub async fn run_with_client<W: Write>(
    options: &Options,
    launch: LaunchSpec,
    prompt: String,
    client: &ManagedClient,
    writer: &mut W,
    mut shutdown: mpsc::Receiver<i32>,
) -> PrintResult {
    let mut output = Output::new(writer, options.format);
    let mut managed = None;
    let mut result = {
        let work = async {
            let session = client.create(launch).await.map_err(access_failure)?;
            output.execution = Some(session.execution());
            managed = Some(session.clone());
            output
                .event("execution.started", None, json!({}))
                .map_err(write_failure)?;
            let observation = session.observe().await.map_err(access_failure)?;
            let receipt = session
                .submit(UserInput::Text {
                    text: prompt,
                    contexts: Vec::new(),
                    paths: Vec::new(),
                })
                .await
                .map_err(access_failure)?;
            output.receipt = Some(receipt.clone());
            output
                .event("run.started", None, json!({}))
                .map_err(write_failure)?;
            // The feed keeps reconnecting while the daemon is unreachable; the lease bounds that.
            tokio::select! {
                biased;
                error = session.lease_lost() => Err(access_failure(error)),
                result = consume(&session, observation, &receipt, &mut output) => result,
            }
        };
        tokio::select! {
            biased;
            code = next_signal(&mut shutdown) => PrintResult::failure(code, "print invocation cancelled"),
            result = tokio::time::timeout(options.timeout, work) => match result {
                Ok(Ok(result)) | Ok(Err(result)) => result,
                Err(_) => PrintResult::failure(124, "print invocation timed out"),
            },
        }
    };
    if let Some(session) = &managed {
        result.identify(session.execution(), output.receipt.as_ref());
    }
    // A lost lease leaves nothing to settle or release; the daemon's expiry owns that cleanup.
    if let Some(session) = managed.filter(|session| session.lease_error().is_none()) {
        // Cancellation and BrokenPipe must not leave a worker, shell, or subscription running.
        // The lease is the final fallback if even this bounded cleanup cannot reach the daemon.
        let cancel = result.exit_code != 0;
        let cleanup = async {
            let mut failures = Vec::new();
            if cancel {
                match tokio::time::timeout(CLEANUP_TIMEOUT, session.settle(true)).await {
                    Ok(Ok(_)) => {}
                    Ok(Err(error)) => failures.push(format!("cancellation failed: {error}")),
                    Err(_) => failures.push("cancellation timed out".into()),
                }
            }
            // Release is attempted once even after cancellation fails. An unknown mutation is not
            // replayed; the daemon's lease remains responsible when this request cannot be observed.
            match tokio::time::timeout(CLEANUP_TIMEOUT, session.release()).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => failures.push(format!("release failed: {error}")),
                Err(_) => failures.push("release timed out; lease expiry remains active".into()),
            }
            failures
        };
        // Signal handlers replace the default disposition, so another signal must end the wait.
        tokio::select! {
            biased;
            code = next_signal(&mut shutdown) => {
                let skipped = "cleanup skipped after another signal; lease expiry remains active";
                result.error = Some(match result.error.take() {
                    Some(original) => format!("{original}; {skipped}"),
                    None => skipped.into(),
                });
                result.exit_code = code;
                result.status = "cancelled".into();
            }
            failures = cleanup => if !failures.is_empty() {
                result = cleanup_failure(result, failures.join("; "));
            },
        }
    }
    if let Err(error) = output.finish(&result) {
        result.exit_code = 1;
        result.status = "failed".into();
        result.error = Some(format!("cannot write stdout: {error}"));
    }
    result
}

/// A closed channel never yields a signal.
pub(crate) async fn next_signal(shutdown: &mut mpsc::Receiver<i32>) -> i32 {
    match shutdown.recv().await {
        Some(code) => code,
        None => std::future::pending().await,
    }
}

fn cleanup_failure(mut result: PrintResult, message: String) -> PrintResult {
    result.exit_code = 1;
    result.status = "failed".into();
    result.error = Some(match result.error {
        Some(original) => format!("{original}; {message}"),
        None => message,
    });
    result
}

fn access_failure(error: AccessError) -> PrintResult {
    let code = if error.code == AccessErrorCode::UntrustedWorkingDirectory {
        3
    } else {
        1
    };
    PrintResult::failure(code, error.to_string())
}

fn write_failure(error: std::io::Error) -> PrintResult {
    PrintResult::failure(1, format!("cannot write stdout: {error}"))
}

async fn consume<W: Write>(
    session: &ManagedSession,
    mut observation: Observation,
    receipt: &Receipt,
    output: &mut Output<'_, W>,
) -> Result<PrintResult, PrintResult> {
    let mut sequence = 0;
    let mut initial = Some(observation.initial.clone());
    loop {
        if let Some(value) = initial.take() {
            let terminal = match value {
                ObservationValue::Agent(snapshot) => {
                    inspect_snapshot(&snapshot, receipt).map_err(|failure| *failure)?;
                    let fallback = snapshot_terminal(&snapshot, receipt);
                    let mut events = snapshot.events;
                    events.sort_by_key(|event| event.sequence);
                    consume_events(events, receipt, &mut sequence, output)
                        .map_err(|failure| *failure)?
                        .or(fallback)
                }
                _ => return Err(PrintResult::failure(1, "unexpected managed observation")),
            };
            if let Some(terminal) = terminal {
                return finish(session, receipt, terminal).await;
            }
        }
        let update = observation.recv().await.map_err(access_failure)?;
        let terminal = match update.change {
            ObservationChange::Event(event) => {
                consume_events([event], receipt, &mut sequence, output)
                    .map_err(|failure| *failure)?
            }
            ObservationChange::Reset { snapshot, reason } => {
                inspect_snapshot(&snapshot, receipt).map_err(|failure| *failure)?;
                let reason = match reason {
                    ResetReason::EpochChanged => "epoch_changed",
                    ResetReason::Lagged => "lagged",
                    ResetReason::CursorExpired => "cursor_expired",
                    ResetReason::ConversationChanged => "conversation_changed",
                };
                output
                    .event("stream.reset", None, json!({"reason": reason}))
                    .map_err(write_failure)?;
                let text = snapshot.conversation.run_streamed_text(receipt.run_id);
                output
                    .event("response.snapshot", None, json!({"text": text}))
                    .map_err(write_failure)?;
                initial = Some(ObservationValue::Agent(snapshot));
                None
            }
            ObservationChange::Resync { .. } | ObservationChange::Changed => {
                let snapshot = session.snapshot().await.map_err(access_failure)?;
                initial = Some(ObservationValue::Agent(Box::new(snapshot)));
                None
            }
            _ => None,
        };
        if let Some(terminal) = terminal {
            return finish(session, receipt, terminal).await;
        }
    }
}

fn inspect_snapshot(
    snapshot: &LiveChatSnapshot,
    receipt: &Receipt,
) -> Result<(), Box<PrintResult>> {
    let waiting = snapshot
        .permissions
        .iter()
        .any(|request| request.execution.run_id() == Some(receipt.run_id))
        || snapshot
            .questions
            .iter()
            .any(|request| request.run_id == receipt.run_id)
        || snapshot.conversation.loops.iter().any(|goal| {
            goal.id == receipt.loop_id
                && matches!(
                    goal.state,
                    LoopState::WaitingQuestion
                        | LoopState::WaitingPermission
                        | LoopState::WaitingPlan
                )
        });
    if waiting {
        return Err(Box::new(PrintResult::failure(
            3,
            "this run requires human input",
        )));
    }
    Ok(())
}

fn snapshot_terminal(snapshot: &LiveChatSnapshot, receipt: &Receipt) -> Option<Terminal> {
    let goal = snapshot
        .conversation
        .loops
        .iter()
        .find(|goal| goal.id == receipt.loop_id && goal.run_id == Some(receipt.run_id))?;
    match goal.state {
        LoopState::Completed => Some(Terminal::Completed(serde_json::Value::Null)),
        LoopState::Failed => Some(Terminal::Failed(
            "run failed; terminal event left the replay window".into(),
        )),
        LoopState::Interrupted | LoopState::Stopped | LoopState::Expired => {
            Some(Terminal::Interrupted)
        }
        LoopState::WaitingTrigger => Some(Terminal::Failed(
            "one-shot execution retained an event source".into(),
        )),
        _ => None,
    }
}

#[derive(Clone)]
enum Terminal {
    Completed(serde_json::Value),
    Failed(String),
    Interrupted,
}

fn consume_events<W: Write>(
    events: impl IntoIterator<Item = Event>,
    receipt: &Receipt,
    sequence: &mut u64,
    output: &mut Output<'_, W>,
) -> Result<Option<Terminal>, Box<PrintResult>> {
    let mut terminal = None;
    for event in events {
        if event.sequence <= *sequence {
            continue;
        }
        *sequence = event.sequence;
        if event.chat_id != receipt.chat_id
            || event.loop_id != receipt.loop_id
            || event.run_id != receipt.run_id
        {
            continue;
        }
        let (kind, data) = match &event.kind {
            EventKind::Started => ("response.started", json!({})),
            EventKind::ResponseStarted { attempt, .. } => {
                ("response.started", json!({"attempt": attempt}))
            }
            EventKind::Text(text) => ("text.delta", json!({"text": text})),
            EventKind::ToolStarted {
                call_id,
                name,
                input,
            } => (
                "tool.started",
                json!({"call_id": call_id, "name": name, "input": input}),
            ),
            EventKind::ToolCompleted {
                call_id, result, ..
            } => (
                "tool.completed",
                json!({"call_id": call_id, "content": result.content, "is_error": result.is_error}),
            ),
            EventKind::Completed { usage } => {
                let usage = json!({
                    "input_tokens": usage.input_tokens,
                    "output_tokens": usage.output_tokens,
                    "cache_read_input_tokens": usage.cache_read_input_tokens,
                    "cache_write_input_tokens": usage.cache_write_input_tokens,
                });
                terminal = Some(Terminal::Completed(usage.clone()));
                ("run.completed", json!({"usage": usage}))
            }
            EventKind::Failed(error) => {
                terminal = Some(Terminal::Failed(error.clone()));
                ("run.failed", json!({"error": error}))
            }
            EventKind::Interrupted => {
                terminal = Some(Terminal::Interrupted);
                ("run.interrupted", json!({}))
            }
            EventKind::PermissionRequested { .. } | EventKind::QuestionAsked { .. } => {
                return Err(Box::new(PrintResult::failure(
                    3,
                    "this run requires human input",
                )));
            }
            // Thinking is never a response text delta. Permission denials are tool-level facts,
            // not a failed invocation when the model can continue successfully.
            _ => continue,
        };
        output
            .event(kind, Some(event.sequence), data)
            .map_err(write_failure)?;
    }
    Ok(terminal)
}

async fn finish(
    session: &ManagedSession,
    receipt: &Receipt,
    terminal: Terminal,
) -> Result<PrintResult, PrintResult> {
    match terminal {
        Terminal::Failed(error) => Err(PrintResult::failure(1, error)),
        Terminal::Interrupted => Err(PrintResult::failure(130, "run interrupted")),
        Terminal::Completed(usage) => {
            // Completed is only a model-run boundary; the daemon's settlement is authoritative
            // for the whole invocation, including background work and cleanup.
            let snapshot = session.settle(false).await.map_err(access_failure)?;
            inspect_snapshot(&snapshot, receipt).map_err(|failure| *failure)?;
            if snapshot.operations.iter().any(|operation| {
                operation.run_id == receipt.run_id && operation.state == OperationState::Unknown
            }) {
                return Err(PrintResult::failure(
                    1,
                    "a tool operation has an unknown outcome",
                ));
            }
            let mut result = PrintResult::failure(0, "");
            result.status = "success".into();
            result.error = None;
            result.text = snapshot
                .conversation
                .run_streamed_text(receipt.run_id)
                .unwrap_or_default();
            result.usage = Some(usage);
            Ok(result)
        }
    }
}

//! The hook engine: runs the Claude Code `command` hooks an event matches through the Chat's
//! Execution Environment and reads what they say.
//!
//! The Settings Store lists a Folder's hooks with their trust. Only those that may run do: a
//! plugin or repository hook the user has not confirmed, or whose content changed, is skipped
//! with a notice shown once per hook and Chat in this process, in a Subagent and on a host that
//! cannot ask as well; nothing ever waits for a confirmation. A hook's `if` further narrows a
//! tool event to the calls its Permission Rule matches. Matching hooks run in parallel, the same
//! command and `if` once per source, each a one-shot command in the Chat's Folder with the
//! event's input on standard input and only `CLAUDE_PROJECT_DIR` and a plugin's directories
//! added to its environment. The plugin's sensitive configuration values a command was given are
//! removed from everything it prints and every error it gets before anything reads them. Each
//! run is kept in the Chat's activity history, never in what the model reads, and announced as
//! it ends. A hook declared `async` or `asyncRewake` runs in the background instead, and so does
//! the rest of a running hook whose first line of output is `{"async": true}`; see
//! [`background`].

mod background;
mod matcher;
mod output;
mod session_events;
mod tool_events;

use std::collections::{BTreeMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};
use tickloop_harness_base::private_fs::unix_time_milliseconds;
use tickloop_harness_protocol::hooks::{
    ConfiguredHook, HookEvent, HookRunOutcome, HookRunRecord, HookSkipNotice, HookSource, HookTrust,
};
use tickloop_harness_protocol::{EventKind, ExecutionMode, Receipt};
use tokio_util::sync::CancellationToken;

use crate::environment::{
    CommandOutput, CommandSpec, ExecutionAuthority, ExecutionIntent, ExecutionOutcome,
    ExecutionScope, OutputTap,
};
use crate::secret_redaction::SecretRedactor;
use crate::{Agent, Chat};
use output::{HookExit, HookResponse};
pub(crate) use output::{HookPermission, HookVerdict};
pub(crate) use session_events::{
    PromptGate, QueuedPrompt, SessionSource, append_hook_notes, is_user_prompt,
    resume_prompt_inputs,
};
pub(crate) use tool_events::{PreToolGate, ToolHookFeedback};

/// Claude Code's default hook timeout, used when a hook declares none.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);
/// How much of a reason, message or output a hook run record keeps.
const RECORD_TEXT_CHARS: usize = 2_000;
/// The most hook runs a Chat's activity history keeps.
pub(crate) const MAX_RECENT_HOOK_RUNS: usize = 100;
/// The agent type a Subagent reports to hooks: Claude Code's for its default Agent.
const SUBAGENT_AGENT_TYPE: &str = "general-purpose";

/// An event to run hooks for.
pub(crate) struct HookCall {
    pub event: HookEvent,
    /// What matchers are tested against: the tool name for tool events, the source for
    /// `SessionStart` and the trigger for `PreCompact`; `None` for events without matchers.
    pub match_query: Option<String>,
    /// The event's own input fields, after the common ones.
    pub fields: Map<String, Value>,
    /// The call a tool event is for, which its records name.
    pub tool: Option<(String, String)>,
}

/// What the hook engine keeps for a Chat in this process.
#[derive(Clone, Debug, Default)]
pub(crate) struct ChatHookState {
    /// The hooks that may not run whose skipping the Chat was told of.
    announced_skips: HashSet<(HookSource, String)>,
    /// Runs a hook's `continue: false` ends once their current batch of calls is done.
    stopped_runs: HashSet<tickloop_harness_protocol::RunId>,
    /// What the session and turn hooks keep.
    pub(crate) session: session_events::SessionHookState,
    /// Ends the hooks running in the background and the `StopFailure` hooks, at `/stop` or
    /// shutdown; Esc leaves them.
    background: Option<CancellationToken>,
    /// How many times `/stop` ended the hooks in the background: an event prepared before a
    /// stop starts none after it.
    background_generation: u64,
}

impl ChatHookState {
    /// The state of a Chat entering this process, which runs the `SessionStart` hooks for
    /// `source` first when it is a root Chat.
    pub(crate) fn starting_session(is_root: bool, source: SessionSource) -> Self {
        let mut state = Self::default();
        if is_root {
            state.session.start_session(source);
        }
        state
    }

    /// Whether a hook asked to end `run`, forgetting it.
    pub(crate) fn take_stop(&mut self, run: tickloop_harness_protocol::RunId) -> bool {
        self.stopped_runs.remove(&run)
    }
}

/// What a Chat gives every hook it runs.
#[derive(Clone)]
struct HookContext {
    folder: PathBuf,
    session_id: String,
    permission_mode: &'static str,
    subagent_id: Option<String>,
    /// The Chat's background generation as the event began.
    background_generation: u64,
}

/// One run as it ended.
struct FinishedRun {
    response: HookResponse,
    duration: Duration,
    exit_code: Option<i32>,
    /// Its standard output, then its standard error.
    output: String,
    /// What an exit status 2 tells: its standard error, else its standard output.
    blocking_output: String,
    /// It went on in the background, which records it apart; it decides nothing for its event.
    backgrounded: bool,
}

impl FinishedRun {
    /// A run that ended without an exit, saying `response`.
    fn unfinished(response: HookResponse, started: Instant) -> Self {
        Self {
            response,
            duration: started.elapsed(),
            exit_code: None,
            output: String::new(),
            blocking_output: String::new(),
            backgrounded: false,
        }
    }

    /// A run of `runnable` for `event` started at `started` that ended with `ending`, read once
    /// the plugin's sensitive values it was given are removed.
    fn read(
        event: HookEvent,
        runnable: &Runnable,
        started: Instant,
        ending: Result<CommandOutput, String>,
        cancelled: bool,
    ) -> Self {
        let redactor = (!runnable.hook.sensitive_values.is_empty())
            .then(|| SecretRedactor::new(&runnable.hook.sensitive_values));
        let redacted = |text: String| match &redactor {
            Some(redactor) => redactor.redact(text),
            None => text,
        };
        let command = match ending {
            Err(error) if cancelled => {
                return Self::unfinished(
                    HookResponse::failed(HookRunOutcome::Cancelled, redacted(error)),
                    started,
                );
            }
            Err(error) => {
                return Self::unfinished(
                    HookResponse::failed(HookRunOutcome::NotRun, redacted(error)),
                    started,
                );
            }
            Ok(command) => command,
        };
        let stdout = redacted(command.stdout.text());
        let stderr = redacted(command.stderr.text());
        let stdout = output::after_background_request(&stdout).unwrap_or(&stdout);
        let response = output::interpret(
            event,
            &runnable.hook.command,
            HookExit {
                code: command.exit.code,
                timed_out: command.timed_out,
                stdout,
                stderr: &stderr,
            },
        );
        let output = [stdout.trim(), stderr.trim()]
            .into_iter()
            .filter(|stream| !stream.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        let blocking_output = if stderr.trim().is_empty() {
            stdout.trim()
        } else {
            stderr.trim()
        };
        Self {
            response,
            duration: started.elapsed(),
            exit_code: command.exit.code,
            output,
            blocking_output: blocking_output.to_owned(),
            backgrounded: false,
        }
    }

    /// What a run that went on in the background gives its event: nothing.
    fn backgrounded() -> Self {
        Self {
            backgrounded: true,
            ..Self::unfinished(HookResponse::success(), Instant::now())
        }
    }
}

/// A hook command's run as the execution environment reports it.
type CommandRun = Pin<Box<dyn Future<Output = Result<CommandOutput, String>> + Send>>;

/// What the records of an event's runs add after their own reasons.
struct RecordNotes {
    /// For the last run: the hooks together did something only one of them may do.
    warning: Option<String>,
    /// For the last run that blocked: why the block does not hold.
    block_override: Option<String>,
}

/// One matched hook that may run.
#[derive(Clone)]
struct Runnable {
    hook: ConfiguredHook,
    command: String,
}

/// Claude Code's `permission_mode` for an execution mode.
fn permission_mode(mode: ExecutionMode) -> &'static str {
    match mode {
        ExecutionMode::Plan => "plan",
        ExecutionMode::AcceptEdits => "acceptEdits",
        ExecutionMode::Auto => "auto",
        ExecutionMode::FullAccess => "bypassPermissions",
    }
}

fn shortened(text: &str) -> String {
    let trimmed = text.trim();
    match trimmed.char_indices().nth(RECORD_TEXT_CHARS) {
        Some((end, _)) => format!("{}…", &trimmed[..end]),
        None => trimmed.to_owned(),
    }
}

/// The one-shot command that runs `runnable` in the Chat's Folder with `stdin` as its input.
fn command_spec(context: &HookContext, runnable: &Runnable, stdin: &[u8]) -> CommandSpec {
    let mut env = plugin_environment(&runnable.hook);
    env.insert(
        "CLAUDE_PROJECT_DIR".into(),
        context.folder.to_string_lossy().into_owned(),
    );
    CommandSpec {
        command: runnable.command.clone(),
        stdin: stdin.to_vec(),
        env,
        cwd: context.folder.to_string_lossy().into_owned(),
        timeout: match runnable.hook.timeout_secs {
            0 => DEFAULT_TIMEOUT,
            seconds => Duration::from_secs(seconds),
        },
        stdout_tap: None,
    }
}

fn plugin_environment(hook: &ConfiguredHook) -> BTreeMap<String, String> {
    let mut variables = BTreeMap::new();
    if let Some(plugin) = &hook.plugin {
        let root = plugin.root.to_string_lossy().into_owned();
        let data = plugin.data.to_string_lossy().into_owned();
        for prefix in ["TICKLOOP", "CLAUDE"] {
            variables.insert(format!("{prefix}_PLUGIN_ROOT"), root.clone());
            variables.insert(format!("{prefix}_PLUGIN_DATA"), data.clone());
        }
    }
    variables
}

impl Agent {
    /// Runs the hooks `call` matches and returns what they decide together; an empty verdict
    /// when none may run. `cancel` ends the runs, as Esc and `/stop` do.
    pub(crate) async fn run_hooks(
        &self,
        receipt: &Receipt,
        call: HookCall,
        cancel: &CancellationToken,
    ) -> HookVerdict {
        self.run_hooks_overriding_block(receipt, call, cancel, None)
            .await
    }

    /// Runs the hooks `call` matches as [`Self::run_hooks`] does; when they block, the record of
    /// the last run that blocked adds `block_override`, which says why the block does not hold.
    pub(crate) async fn run_hooks_overriding_block(
        &self,
        receipt: &Receipt,
        call: HookCall,
        cancel: &CancellationToken,
        block_override: Option<String>,
    ) -> HookVerdict {
        let Some((context, settings)) = self.hook_context(receipt).await else {
            return HookVerdict::default();
        };
        let folder = context.folder.clone();
        let hooks = match tokio::task::spawn_blocking(move || settings.hooks(Some(&folder))).await {
            Ok(list) => list.hooks,
            Err(_) => return HookVerdict::default(),
        };
        let runnable = self.select_hooks(receipt, &call, hooks).await;
        if runnable.is_empty() {
            return HookVerdict::default();
        }
        let transcript_path = match self.hook_transcript_path(receipt.chat_id).await {
            Ok(Some(path)) => path.to_string_lossy().into_owned(),
            Ok(None) => String::new(),
            Err(error) => {
                tracing::warn!("hook transcript unavailable: {error}");
                String::new()
            }
        };
        let input = hook_input(&context, &call, &transcript_path);
        let stdin = serde_json::to_vec(&input).expect("hook input serializes");
        let (background, runnable): (Vec<_>, Vec<_>) = runnable
            .into_iter()
            .partition(|runnable| runnable.hook.background.is_some());
        let mut session_end_runs = Vec::new();
        for runnable in background {
            let Some(run) = self
                .start_background_hook(receipt, &context, &call, runnable, &stdin, cancel)
                .await
            else {
                continue;
            };
            if call.event == HookEvent::SessionEnd {
                session_end_runs.push(run.finish());
            } else {
                self.inner.tasks.spawn(run.finish());
            }
        }
        let runs = runnable.iter().map(|runnable| {
            self.run_foreground_hook(receipt, &context, &call, runnable, &stdin, cancel)
        });
        let (finished, _) = futures_util::future::join(
            futures_util::future::join_all(runs),
            futures_util::future::join_all(session_end_runs),
        )
        .await;
        let (verdict, warning) = HookVerdict::merge(
            runnable
                .iter()
                .zip(&finished)
                .map(|(runnable, run)| (&runnable.hook.source, &run.response)),
        );
        if let Some(warning) = &warning {
            tracing::warn!("{} hooks: {warning}", call.event.name());
        }
        let block_override = block_override.filter(|_| {
            !verdict.blocking_errors.is_empty()
                && !verdict.prevent_continuation
                && !verdict.cancelled
        });
        self.record_hook_runs(
            receipt,
            &call,
            &runnable,
            finished,
            RecordNotes {
                warning,
                block_override,
            },
        )
        .await;
        verdict
    }

    async fn hook_context(
        &self,
        receipt: &Receipt,
    ) -> Option<(
        HookContext,
        Arc<dyn tickloop_harness_policy::settings::SettingsStore>,
    )> {
        let settings = self.inner.settings.clone()?;
        self.inner.environment.as_ref()?;
        let chats = self.inner.chats.lock().await;
        let chat = chats.get(&receipt.chat_id)?;
        let snapshot = &chat.snapshot;
        Some((
            HookContext {
                folder: snapshot.folder.clone()?,
                session_id: receipt.chat_id.uuid().to_string(),
                permission_mode: permission_mode(snapshot.policy.mode),
                subagent_id: snapshot
                    .delegation
                    .as_ref()
                    .map(|_| receipt.chat_id.uuid().to_string()),
                background_generation: chat.hook_state.background_generation,
            },
            settings,
        ))
    }

    /// The hooks `call` matches that may run, the same command, `if` and way of running once per
    /// source; announces the ones it matches that may not run yet.
    async fn select_hooks(
        &self,
        receipt: &Receipt,
        call: &HookCall,
        hooks: Vec<ConfiguredHook>,
    ) -> Vec<Runnable> {
        let query = call.match_query.as_deref().unwrap_or_default();
        let matched = hooks.into_iter().filter(|hook| {
            hook.event == call.event
                && (!call.event.uses_matcher()
                    || matcher::matches(call.event, hook.matcher.as_deref(), query))
                && hook.condition.as_deref().is_none_or(|condition| {
                    call.event.is_tool_event()
                        && matcher::condition_matches(
                            condition,
                            query,
                            call.fields.get("tool_input").unwrap_or(&Value::Null),
                        )
                })
        });
        let mut runnable = Vec::new();
        let mut commands = HashSet::new();
        let mut skipped = Vec::new();
        for hook in matched {
            if hook.trust == HookTrust::Disabled {
                continue;
            }
            match hook.resolved_command.clone() {
                Some(command) if hook.may_run() => {
                    if commands.insert((
                        hook.source.clone(),
                        command.clone(),
                        hook.condition.clone(),
                        hook.background.clone(),
                    )) {
                        runnable.push(Runnable { hook, command });
                    }
                }
                _ => skipped.push(hook),
            }
        }
        if !skipped.is_empty() {
            self.announce_skipped_hooks(receipt, call, skipped).await;
        }
        runnable
    }

    async fn announce_skipped_hooks(
        &self,
        receipt: &Receipt,
        call: &HookCall,
        skipped: Vec<ConfiguredHook>,
    ) {
        let mut chats = self.inner.chats.lock().await;
        let Some(chat) = chats.get_mut(&receipt.chat_id) else {
            return;
        };
        let mut notice = HookSkipNotice {
            event: call.event,
            tool_name: call.tool.as_ref().map(|(name, _)| name.clone()),
            unconfirmed: 0,
            unconfigured: 0,
        };
        for hook in skipped {
            if !chat
                .hook_state
                .announced_skips
                .insert((hook.source.clone(), hook.key.clone()))
            {
                continue;
            }
            if hook.needs_configuration() && hook.trust == HookTrust::Trusted {
                notice.unconfigured += 1;
            } else {
                notice.unconfirmed += 1;
            }
        }
        if notice.unconfirmed + notice.unconfigured > 0 {
            self.emit(chat, receipt, EventKind::HooksSkipped { notice });
        }
    }

    /// Runs one hook to its end, returning what it said and how long it took.
    async fn run_one_hook(
        &self,
        receipt: &Receipt,
        context: &HookContext,
        event: HookEvent,
        runnable: &Runnable,
        stdin: &[u8],
        cancel: &CancellationToken,
    ) -> FinishedRun {
        let started = Instant::now();
        let spec = command_spec(context, runnable, stdin);
        let ending = self
            .execute_hook_command(receipt, context, spec, cancel)
            .await;
        FinishedRun::read(event, runnable, started, ending, cancel.is_cancelled())
    }

    /// Runs one hook for its event, which waits for it unless its first line of output asks to
    /// go on in the background, as Claude Code 2.1.302 lets a running hook do. The rest of
    /// such a run is an `async` hook's: `cancel` no longer ends it, `/stop` and shutdown do.
    async fn run_foreground_hook(
        &self,
        receipt: &Receipt,
        context: &HookContext,
        call: &HookCall,
        runnable: &Runnable,
        stdin: &[u8],
        cancel: &CancellationToken,
    ) -> FinishedRun {
        let started = Instant::now();
        let (tap, stdout) = OutputTap::new();
        let spec = CommandSpec {
            stdout_tap: Some(tap),
            ..command_spec(context, runnable, stdin)
        };
        let execution_cancel = CancellationToken::new();
        let mut execution: CommandRun = Box::pin({
            let (agent, receipt, context) = (self.clone(), receipt.clone(), context.clone());
            let execution_cancel = execution_cancel.clone();
            async move {
                agent
                    .execute_hook_command(&receipt, &context, spec, &execution_cancel)
                    .await
            }
        });
        let asks_for_background = output::first_line_asks_for_background(stdout);
        tokio::pin!(asks_for_background);
        let ending = tokio::select! {
            biased;
            () = cancel.cancelled() => {
                execution_cancel.cancel();
                let ending = execution.await;
                return FinishedRun::read(call.event, runnable, started, ending, true);
            }
            ending = &mut execution => ending,
            true = &mut asks_for_background => {
                self.continue_in_background(
                    receipt,
                    context,
                    call,
                    runnable,
                    started,
                    execution,
                    execution_cancel,
                    cancel,
                )
                .await;
                return FinishedRun::backgrounded();
            }
        };
        let asked_for_background = ending.as_ref().is_ok_and(|command| {
            output::after_background_request(&command.stdout.text()).is_some()
        });
        if asked_for_background {
            let execution: CommandRun = Box::pin(std::future::ready(ending));
            self.continue_in_background(
                receipt,
                context,
                call,
                runnable,
                started,
                execution,
                execution_cancel,
                cancel,
            )
            .await;
            return FinishedRun::backgrounded();
        }
        FinishedRun::read(call.event, runnable, started, ending, cancel.is_cancelled())
    }

    async fn execute_hook_command(
        &self,
        receipt: &Receipt,
        context: &HookContext,
        spec: CommandSpec,
        cancel: &CancellationToken,
    ) -> Result<crate::environment::CommandOutput, String> {
        let environment = self
            .inner
            .environment
            .clone()
            .ok_or("no execution environment")?;
        let authority = ExecutionAuthority::new(receipt.chat_id);
        let port = environment
            .bind(
                ExecutionScope {
                    chat: receipt.chat_id,
                    folder: Some(context.folder.to_string_lossy().into_owned()),
                },
                authority.clone(),
            )
            .map_err(|error| error.to_string())?;
        let prepared = port
            .prepare(ExecutionIntent::RunCommand(spec))
            .await
            .map_err(|error| match error.code.as_str() {
                "Unsupported" => "the execution environment cannot run hook commands".to_owned(),
                _ => error.to_string(),
            })?;
        let admission = authority
            .admit(&prepared)
            .map_err(|error| error.to_string())?;
        match port.execute(prepared, admission, cancel.clone()).await {
            ExecutionOutcome::Completed(result) => result
                .command
                .map(|output| *output)
                .ok_or_else(|| "the execution environment returned no command output".into()),
            ExecutionOutcome::NotDispatched(error) => Err(error.message),
            ExecutionOutcome::OutcomeUnknown { reason, .. } => Err(reason),
        }
    }

    async fn record_hook_runs(
        &self,
        receipt: &Receipt,
        call: &HookCall,
        runnable: &[Runnable],
        finished: Vec<FinishedRun>,
        notes: RecordNotes,
    ) {
        let mut chats = self.inner.chats.lock().await;
        let Some(chat) = chats.get_mut(&receipt.chat_id) else {
            return;
        };
        let ended = runnable
            .iter()
            .zip(finished)
            .filter(|(_, run)| !run.backgrounded)
            .collect::<Vec<_>>();
        let count = ended.len();
        let last_block = ended
            .iter()
            .rposition(|(_, run)| run.response.blocking_error.is_some());
        for (index, (runnable, run)) in ended.into_iter().enumerate() {
            let mut record = run_record(
                receipt,
                call.event,
                call.tool.as_ref(),
                &runnable.hook,
                &run,
                run.response.reason(),
            );
            if index + 1 == count
                && let Some(warning) = &notes.warning
            {
                add_to_reason(&mut record, warning);
            }
            if Some(index) == last_block
                && let Some(block_override) = &notes.block_override
            {
                add_to_reason(&mut record, block_override);
            }
            record_hook_run(self, chat, receipt, record);
        }
        self.save(&chat.snapshot);
    }
}

fn add_to_reason(record: &mut HookRunRecord, note: &str) {
    record.reason = Some(match record.reason.take() {
        Some(reason) => format!("{reason}\n{note}"),
        None => note.to_owned(),
    });
}

/// The record of `hook`'s `run` for `event`, giving `reason` as why it decided, blocked or
/// failed.
fn run_record(
    receipt: &Receipt,
    event: HookEvent,
    tool: Option<&(String, String)>,
    hook: &ConfiguredHook,
    run: &FinishedRun,
    reason: Option<String>,
) -> HookRunRecord {
    let response = &run.response;
    HookRunRecord {
        id: tickloop_ids::generate(),
        chat_id: receipt.chat_id,
        loop_id: receipt.loop_id,
        run_id: receipt.run_id,
        at_ms: unix_time_milliseconds(),
        event,
        source: hook.source.clone(),
        command: hook.command.clone(),
        tool_name: tool.map(|(name, _)| name.clone()),
        call_id: tool.map(|(_, call_id)| call_id.clone()),
        duration_ms: u64::try_from(run.duration.as_millis()).unwrap_or(u64::MAX),
        outcome: response.outcome,
        exit_code: run.exit_code,
        decision: response.decision(&hook.source),
        reason: reason.as_deref().map(shortened),
        system_message: response.system_message.as_deref().map(shortened),
        updated_input: response
            .updated_input
            .as_ref()
            .map(|input| shortened(&input.to_string())),
        output: if response.suppress_output {
            String::new()
        } else {
            shortened(&run.output)
        },
        background: None,
    }
}

fn record_hook_run(agent: &Agent, chat: &mut Chat, receipt: &Receipt, record: HookRunRecord) {
    let window = &mut chat.denial_window;
    window.hook_runs.push_front(record.clone());
    window.hook_runs.truncate(MAX_RECENT_HOOK_RUNS);
    window.revision += 1;
    agent.emit(
        chat,
        receipt,
        EventKind::HookRan {
            record: Box::new(record),
        },
    );
}

/// Claude Code's hook input: the common fields, then the event's own.
fn hook_input(context: &HookContext, call: &HookCall, transcript_path: &str) -> Value {
    let mut input = json!({
        "session_id": context.session_id,
        "transcript_path": transcript_path,
        "cwd": context.folder.to_string_lossy(),
        "permission_mode": context.permission_mode,
        "hook_event_name": call.event.name(),
    });
    let object = input.as_object_mut().expect("hook input is an object");
    if let Some(agent_id) = &context.subagent_id {
        object.insert("agent_id".into(), json!(agent_id));
        object.insert("agent_type".into(), json!(SUBAGENT_AGENT_TYPE));
    }
    object.extend(call.fields.clone());
    input
}

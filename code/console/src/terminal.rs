use std::{
    collections::{HashMap, HashSet, VecDeque},
    io::IsTerminal,
    os::unix::fs::MetadataExt,
    path::PathBuf,
    sync::Arc,
};

use tickhost_connect::herdr::{HerdrContext, SocketHerdr, current_pane, endpoint_instance};

use tickloop_code_settings::{
    LoadedSettings, UserSettings, config_root_directory, parse_execution_mode,
};
use tickloop_harness::tools::{ENTER_PLAN_MODE_TOOL_NAME, EXIT_PLAN_MODE_TOOL_NAME};
use tokio::sync::{broadcast, mpsc};

mod agent_messaging;
mod approval_status;
mod artifact_ui;
mod artifacts_picker;
mod auto_setup_surface;
mod background_work;
#[cfg(test)]
mod child_config_tests;
mod clipboard;
mod clipboard_hint;
mod completed_tasks;
mod completion;
mod console;
mod context_view;
mod conversation;
#[cfg(test)]
mod conversation_layout_tests;
mod conversation_picker;
#[cfg(test)]
mod daemon_loss_tests;
mod debug_line;
mod denials_surface;
pub mod diff_catalog;
pub mod diff_dialog;
pub mod diff_view;
mod directed_summary;
mod display;
mod editor;
mod execution;
mod external_editor;
mod file_mentions;
mod footer;
mod goal_view;
mod herdr_agent;
mod hook_activity;
mod hosted_probe;
mod hyperlink;
mod image_cache;
mod image_preparation;
mod input_history;
mod input_view;
#[cfg(test)]
mod late_receipt_tests;
mod launch_rules;
mod list_row;
mod login;
mod loop_view_text;
mod mcp_panel;
mod mcp_work;
mod menus;
#[cfg(test)]
mod mode_cycle_tests;
mod monitor;
mod monitor_cards;
#[cfg(test)]
mod navigation_tests;
mod new_agent_draft;
mod opener;
mod paste_blocks;
mod pasted_image_paths;
mod permissions_surface;
mod plugin_configuration;
mod presentation;
mod process_registry;
mod prompt_suggestion;
#[cfg(test)]
mod prompt_suggestion_tests;
mod questions;
#[cfg(test)]
mod recovery_tests;
mod renderer;
mod reply_lines;
mod running_shimmer;
mod runtime_upgrade;
mod sandbox_surface;
mod scheduled_task_rows;
mod scheduled_tasks;
mod scrollback;
mod selection;
mod settings;
mod shell_input;
mod status_area;
mod style;
mod subagent_panel;
mod subscription_detail;
mod task_access;
#[cfg(test)]
mod test_runtime;
mod thinking_preview;
pub mod tick_actions;
mod tick_labels;
mod ticks_panel;
mod ticks_work;
mod transcript;
mod update_notice;
mod user_panel;
#[cfg(test)]
mod view_handoff_tests;
mod view_upgrade;
mod workflows;
mod workspace_trust;

use agent_messaging::{AgentInput, AgentMessage};
use console::{Console, InputEvent, RendererSwitch, RequestedPlanEdit};
use diff_catalog::ChangeCatalog;
use directed_summary::{DirectedSummaryTasks, SummaryTaskCompletion};
use display::{DisplayProfile, Glyph};
use execution::{
    ConsoleExecution as Agent, CreateAcknowledgement, CreationAcceptance, DraftCreationError,
    DraftPermissionsError, ExecutionObservation, InputResolution, RuleSave,
};
use login::{LoginFlow, LoginSession, LoginUpdate, LogoutFlow};
use menus::{MenuOutcome, Menus};
use new_agent_draft::ConfigTarget;
use permissions_surface::{PermissionsEffect, PermissionsReturn, PermissionsSubject};
use presentation::{MessageGroups, ToolPresentation};
use process_registry::ProcessRegistration;
use renderer::Renderer;
use reply_lines::ReplyLines;
use running_shimmer::RunningShimmer;
use scrollback::InputEcho;
use status_area::ContextGauge;
use style::Role;

const DISCONNECTED_CACHED_VIEW_NOTICE: &str = "Disconnected · cached view";
const VIEW_REGISTRATION_UNAVAILABLE_NOTICE: &str = "View registration unavailable";
const DISPATCH_DIRECTORY_MISSING: &str = "Dispatch directory must exist";
const EXECUTION_UNAVAILABLE_FOR_INPUT: &str =
    "This execution is unavailable; inspect cached history or use /resume explicitly";
const VIEW_UPDATE_AFTER_ENDED_AGENT: &str =
    "View update · waits until the shown Agent runs again; use /resume or open another Agent";
const HISTORY_ADDED_TO_OVERVIEW: &str = "Added to the Agents list · enter opens it";
const ALREADY_IN_OVERVIEW: &str = "This Agent is already in the list · press enter on its row";
const HANDOFF_AGENT_ENDED: &str = "The Agent this View showed is no longer running; its unsent draft is kept and /resume opens saved conversations";
use task_access::ShellTaskCenter;
use thinking_preview::ThinkingPreview;
use tickloop_code_access::{
    AccessError, AccessErrorCode, AfterTurnAction, AgentDetailQuery, AgentSummary, CommandResult,
    ConfigurationChange, EnvironmentValue, ExecutionAvailability, ExecutionRef, HostLocation,
    HostWorkContext, LaunchSpec, ObservationRequest, ObservationValue, UserInput, ViewRef,
};
use tickloop_harness::model::{ModelConfigExt, ReasoningEffortExt};
use tickloop_harness::protocol::{
    AgentHostOptions, BranchOutcome, BranchRequest, ChatId, ChatSnapshot, ChatStatus, Decision,
    DirectoryTrustPolicy, EventKind, ExecutionMode, InputContext, ModelConfig, ModelRequestKind,
    PermissionChange, PermissionRule, PermissionScope, PromptPolicy, QuestionRequest,
    QueuedInputKind, ReasoningEffort, Receipt, RunId, ShellTaskNotice, ShellTaskOrigin,
    ShellTaskPromptWatchdog, ShellTaskState, TaskNotification, TaskNotificationStatus, Usage,
    UserShellRecord, UserShellState,
};

use workflows::Workflow;

const SUBSCRIPTION_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);
/// How often an open transcript view is rebuilt while the Chat it shows keeps changing.
const TRANSCRIPT_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);
/// How often the host state of Monitor members without a subscription is sampled.
const MEMBER_PROBE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

macro_rules! out {
    ($console:expr, $($argument:tt)*) => { $console.write(&format!($($argument)*))? };
}
macro_rules! outln {
    ($console:expr $(,)?) => { $console.write("\n")? };
    ($console:expr, $($argument:tt)*) => { $console.write_line(&format!($($argument)*))? };
}

const COMMAND_OUTPUT_BYTES: usize = 16 * 1024;
/// How much of a finished `!` command's output its result is drawn from, as much as the Agent
/// keeps of it in the conversation.
const USER_SHELL_RESULT_BYTES: usize = 16 * 1024;
/// Latest lines of a running `!` command kept for its preview.
const USER_SHELL_PREVIEW_LINES: usize = 5;
const ANIMATION_FRAME_DURATION: std::time::Duration = std::time::Duration::from_millis(32);
/// The activity shown while a run works without an in-progress todo.
const WORKING_ACTIVITY: &str = "Working";
/// The activity shown while a run's request waits for MCP servers that are still connecting.
const AWAITED_MCP_SERVERS_ACTIVITY: &str = "Waiting for MCP servers";
const STOPPED_NOTICE: &str =
    "Stopped. Queued messages were discarded; send your next message to resume receiving Triggers.";

struct RunningActivity {
    chat_id: ChatId,
    label: String,
    started_at: Option<std::time::Instant>,
    interruptible: bool,
}

impl RunningActivity {
    fn spans(
        &self,
        console: &Console,
        preview: Option<&ThinkingPreview>,
        shimmer: &RunningShimmer,
    ) -> Result<Vec<(String, Option<Role>)>, String> {
        let display = console.display_profile()?;
        let reduced_motion = display.preferences().prefers_reduced_motion;
        let dot = display.working_dot(tickloop_harness::base::private_fs::unix_time_milliseconds());
        if let Some(preview) = preview {
            return Ok(preview.spans(console.conversation_width()?, dot, reduced_motion));
        }
        let mut row = style::RoleLine::role(dot, Role::RunningMark);
        row.push(" ", Some(Role::MutedText));
        shimmer.append(&mut row, &self.label, reduced_motion);
        let mut details = self
            .started_at
            .map(|started| format_activity_elapsed(started.elapsed()))
            .into_iter()
            .collect::<Vec<_>>();
        if self.interruptible {
            details.push("esc to interrupt".into());
        }
        if !details.is_empty() {
            row.push(format!(" ({})", details.join(" · ")), Some(Role::MutedText));
        }
        Ok(row
            .spans
            .into_iter()
            .map(|span| (span.text, span.role))
            .collect())
    }
}

/// Claude Code's past-tense verbs for a finished turn, as in `⏺ Brewed for 12s`.
const TURN_COMPLETION_VERBS: [&str; 8] = [
    "Baked",
    "Brewed",
    "Churned",
    "Cogitated",
    "Cooked",
    "Crunched",
    "Sautéed",
    "Worked",
];
/// A finished turn's line restored without its duration.
const RESTORED_TURN_SUMMARY: &str = "Done";

/// The Folder a Chat's paths show against; empty for a Chat without one, whose paths show whole.
fn shown_folder(folder: Option<&std::path::Path>) -> &std::path::Path {
    folder.unwrap_or(std::path::Path::new(""))
}

/// What a finished turn's line starts with, live with its duration or restored without it.
#[cfg(test)]
fn turn_summary_markers() -> impl Iterator<Item = String> {
    TURN_COMPLETION_VERBS
        .iter()
        .map(|verb| format!("⏺ {verb} for"))
        .chain(std::iter::once(format!("⏺ {RESTORED_TURN_SUMMARY}")))
}

#[cfg(test)]
fn shows_turn_summary(frame: &str) -> bool {
    turn_summaries(frame) > 0
}

#[cfg(test)]
fn turn_summaries(frame: &str) -> usize {
    turn_summary_markers()
        .map(|marker| frame.matches(&marker).count())
        .sum()
}

#[cfg(test)]
fn turn_summary_position(frame: &str) -> Option<usize> {
    turn_summary_markers()
        .filter_map(|marker| frame.find(&marker))
        .min()
}

fn format_activity_elapsed(elapsed: std::time::Duration) -> String {
    let seconds = elapsed.as_secs() % 60;
    let minutes = elapsed.as_secs() / 60 % 60;
    let hours = elapsed.as_secs() / 3600;
    if hours > 0 {
        format!("{hours}h {minutes}m {seconds}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

fn write_pending_reply_lines(console: &Console, reply: &mut ReplyLines) -> Result<(), String> {
    let _frame_hold = console.hold_frames()?;
    while reply.has_complete_line() {
        write_next_reply_line(console, reply)?;
    }
    Ok(())
}

/// Ends the paced display of `chat`'s runs when the user interrupts: what
/// their replies already hold is written now and later deltas are written as
/// they arrive, so the screen stops with the model instead of draining what
/// the model had produced line by line.
fn stop_visual_pacing(
    console: &Console,
    chat: ChatId,
    pending: &HashMap<uuid::Uuid, Receipt>,
    thinking_previews: &mut HashMap<uuid::Uuid, ThinkingPreview>,
    reply_lines: &mut HashMap<uuid::Uuid, ReplyLines>,
    unpaced_runs: &mut HashSet<RunId>,
) -> Result<(), String> {
    let _frame_hold = console.hold_frames()?;
    for (run_id, receipt) in pending {
        if receipt.chat_id != chat {
            continue;
        }
        unpaced_runs.insert((*run_id).into());
        thinking_previews.remove(run_id);
        if let Some(reply) = reply_lines.get_mut(run_id) {
            write_pending_reply_lines(console, reply)?;
        }
    }
    Ok(())
}

fn write_next_reply_line(console: &Console, reply: &mut ReplyLines) -> Result<(), String> {
    let Some((line, starts_message)) = reply.next_line() else {
        return Ok(());
    };
    console
        .for_message(reply.message_id.clone())
        .write_markdown(&line, starts_message)
}

fn write_completion_notice(
    console: &Console,
    details: Option<&CompletionDetails>,
    debug: Option<&CompletionDebug>,
) -> Result<(), String> {
    let display = console.display_profile()?;
    let interactive = console.is_interactive();
    let (heading, details) = match details {
        Some(details) if interactive => (
            format!("{} {}", display.glyph(Glyph::TurnSummary), details.heading),
            details.rest.as_str(),
        ),
        Some(details) => ("Reply completed".into(), details.rest.as_str()),
        None if interactive => (
            format!(
                "{} {RESTORED_TURN_SUMMARY}",
                display.glyph(Glyph::TurnSummary)
            ),
            "",
        ),
        None => ("Reply completed".into(), ""),
    };
    // Claude Code's turn summary, `Worked for 12s`, is muted throughout.
    let mut spans = vec![
        (heading, Some(Role::MutedText)),
        (details.into(), Some(Role::MutedText)),
    ];
    if let Some(debug) = debug {
        let cache = &debug.summary;
        spans.push((" · ".into(), Some(Role::MutedText)));
        spans.push((
            cache.cache.clone(),
            Some(if cache.is_warning {
                Role::Warning
            } else {
                Role::MutedText
            }),
        ));
        if let Some(average) = &cache.average {
            spans.push((format!(" · {average}"), Some(Role::MutedText)));
        }
        if interactive && console.debug_button_is_clickable()? {
            spans.push(("  ".into(), Some(Role::MutedText)));
            spans.push((debug_line::DEBUG_BUTTON.into(), Some(Role::Link)));
        }
        console.remember_debug_details(debug.details.clone())?;
    }
    spans.push(("\n".into(), None));
    console.write_completion_notice(&spans)
}

/// What a finished turn's line adds when the Console shows debug details: its prompt cache
/// summary, and the details its button copies.
struct CompletionDebug {
    summary: debug_line::CacheSummary,
    details: String,
}

/// The debug details of `run`, when the Console shows them and the run made a model request the
/// ledger, which lives only as long as the Agent's process, still holds.
fn completion_debug(
    console: &Console,
    snapshot: &ChatSnapshot,
    run: RunId,
) -> Result<Option<CompletionDebug>, String> {
    if !console.shows_debug_details()? {
        return Ok(None);
    }
    let turn = debug_line::TurnDebug::of(snapshot, run);
    Ok((!turn.has_no_requests()).then(|| CompletionDebug {
        summary: turn.cache_summary(),
        details: turn.details(),
    }))
}

/// What a finished run reports: how long it worked, in one of Claude Code's verbs that the run
/// keeps across redraws, then what still runs.
struct CompletionDetails {
    heading: String,
    rest: String,
}

fn completion_details(
    run: RunId,
    elapsed: Option<std::time::Duration>,
    shells: usize,
) -> CompletionDetails {
    let heading = elapsed.map_or_else(
        || RESTORED_TURN_SUMMARY.to_owned(),
        |elapsed| {
            let verb = TURN_COMPLETION_VERBS
                [(run.uuid().as_u128() % TURN_COMPLETION_VERBS.len() as u128) as usize];
            format!("{verb} for {}", format_activity_elapsed(elapsed))
        },
    );
    CompletionDetails {
        heading,
        rest: if shells > 0 {
            format!(
                " · {shells} {} still running",
                if shells == 1 { "shell" } else { "shells" }
            )
        } else {
            String::new()
        },
    }
}

fn ends_thinking(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Text(_)
            | EventKind::AssistantMessageCommitted { .. }
            | EventKind::ToolStarted { .. }
            | EventKind::PermissionRequested { .. }
            | EventKind::QuestionAsked { .. }
            | EventKind::ModelRetry { .. }
            | EventKind::Completed { .. }
            | EventKind::Failed(_)
            | EventKind::Interrupted
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CommandTiming {
    Immediate,
    AfterTurn,
}

fn command_timing(command: &str) -> CommandTiming {
    let name = command.split_whitespace().next().unwrap_or(command);
    if is_rewind_command(name)
        || matches!(
            name,
            "/exit"
                | "/help"
                | "/details"
                | debug_line::DEBUG_COMMAND
                | "/diff"
                | "/status"
                | "/context"
                | "/artifacts"
                | "/plugins"
                | "/ticks"
                | "/mcp"
                | "/settings"
                | "/permissions"
                | "/sandbox"
                | "/auto-mode-setup"
                | "/mode"
                | "/model"
                | "/websearch"
                | "/provider"
                | "/thinking"
                | "/operations"
                | "/rules"
                | "/plan"
                | "/interrupt"
                | "/goal"
                | "/stop"
                | "/allow"
                | "/deny"
                | "/answer"
                | "/background"
                | "/stop-command"
                | "/command-output"
                | "/clear"
                | "/branch"
                | "/tui"
                | "/resume"
                | "/acknowledge-input"
                | "/acknowledge-create"
        )
    {
        CommandTiming::Immediate
    } else {
        CommandTiming::AfterTurn
    }
}

/// How a typed input waits while a turn works: as a message, or as a command for after the turn;
/// any other command runs at once.
fn waiting_input_kind(line: &str, is_command: bool) -> Option<QueuedInputKind> {
    let input = line.trim();
    if !is_command {
        Some(QueuedInputKind::Message)
    } else if command_timing(input) == CommandTiming::AfterTurn {
        Some(QueuedInputKind::Command)
    } else {
        None
    }
}

/// Whether an input sent to `chat` now waits behind running work, as the Agent queues it: one of
/// its turns or its foreground Shell is running.
fn input_waits(
    chat: ChatId,
    pending: &HashMap<uuid::Uuid, Receipt>,
    foreground_shells: &HashMap<uuid::Uuid, uuid::Uuid>,
) -> bool {
    pending.values().any(|receipt| receipt.chat_id == chat)
        || foreground_shells.contains_key(&chat.uuid())
}

const TUI_SYNTAX: &str = "use /tui [inline|fullscreen]";
const RENDERER_NEEDS_A_TERMINAL: &str =
    "Renderer switching is available only in an interactive terminal.";

/// `Ok(None)` asks for the active renderer; `Ok(Some(_))` asks to switch to it.
fn tui_renderer_request(command: &str) -> Result<Option<Renderer>, &'static str> {
    let argument = command.strip_prefix("/tui").unwrap_or_default().trim();
    if argument.is_empty() {
        return Ok(None);
    }
    Renderer::parse(argument).map(Some).ok_or(TUI_SYNTAX)
}

fn is_rewind_command(command: &str) -> bool {
    command == "/rewind"
}

fn branch_name(command: &str) -> Result<Option<String>, String> {
    if command == "/branch" {
        return Ok(None);
    }
    let name = command
        .strip_prefix("/branch ")
        .ok_or_else(|| "use /branch [NAME]".to_owned())?
        .trim();
    Ok((!name.is_empty()).then(|| name.to_owned()))
}

fn branch_failure_message(error: &impl std::fmt::Display) -> String {
    format!(
        "Failed to branch conversation: {}",
        safe_line(&error.to_string())
    )
}

fn switch_to_saved_branch(console: &Console, child_chat_id: ChatId) -> Result<(), String> {
    match console.commit_branch_and_reset_chat(child_chat_id.into()) {
        Ok(()) => Ok(()),
        Err(reset_error) => {
            let recovery = format!(
                "Branch {} was saved, but the terminal could not switch to it: {}. Use /resume to find it or /resume {} to open it.",
                child_chat_id.public(),
                safe_line(&reset_error),
                child_chat_id.public()
            );
            match console.commit_branch_submission() {
                Ok(()) => Err(recovery),
                Err(commit_error) => {
                    let release_error = console.cancel_branch_submission(false).err();
                    Err(format!(
                        "{recovery} The submitted branch command could not be finalized: {}.{}",
                        safe_line(&commit_error),
                        release_error.map_or_else(String::new, |error| format!(
                            " Its input gate also could not be released: {}.",
                            safe_line(&error)
                        ))
                    ))
                }
            }
        }
    }
}

async fn branch_from_terminal(
    console: &Console,
    agent: &Agent,
    source_chat_id: Option<ChatId>,
    command: &str,
    preserve_until_summary_finishes: bool,
) -> Result<BranchOutcome, String> {
    let Some(source_chat_id) = source_chat_id else {
        console.cancel_branch_submission(preserve_until_summary_finishes)?;
        return Err(
            "No Chat is selected. Start or open a conversation before branching it.".into(),
        );
    };
    if preserve_until_summary_finishes {
        console.cancel_branch_submission(true)?;
        return Err(branch_failure_message(&"chat is busy"));
    }
    let name = match branch_name(command) {
        Ok(name) => name,
        Err(error) => {
            console.cancel_branch_submission(preserve_until_summary_finishes)?;
            return Err(branch_failure_message(&error));
        }
    };
    let outcome = match agent
        .branch_chat(BranchRequest {
            request_id: tickloop_ids::generate(),
            source_chat_id,
            name,
        })
        .await
    {
        Ok(outcome) => outcome,
        Err(error) => {
            console.cancel_branch_submission(preserve_until_summary_finishes)?;
            return Err(branch_failure_message(&error));
        }
    };
    ensure_snapshot_banner(console, &outcome.snapshot)?;
    switch_to_saved_branch(console, outcome.child_chat_id)?;
    Ok(outcome)
}

fn is_branch_command(command: &str) -> bool {
    command == "/branch" || command.starts_with("/branch ")
}

fn should_pause_input_for_summary(event: &InputEvent) -> bool {
    match event {
        InputEvent::Prompt { text, .. } | InputEvent::Line(text) => !is_branch_command(text.trim()),
        InputEvent::UserShellSubmitted(_) => true,
        InputEvent::CycleExecutionMode => false,
        _ => false,
    }
}

fn should_dismiss_menu_before_dispatch(command: &str) -> bool {
    command.starts_with('/')
        && command != "/diff"
        && !is_rewind_command(command)
        && !is_branch_command(command)
}

fn startup_directory_needs_trust(
    settings: Option<&UserSettings>,
    workdir: &std::path::Path,
    interactive: bool,
) -> Result<bool, String> {
    let settings = settings.ok_or_else(|| {
        "cannot record working-directory trust because the user settings directory is unavailable; set TICKLOOP_CONFIG_DIR"
            .to_owned()
    })?;
    if settings.is_working_directory_trusted(workdir) {
        return Ok(false);
    }
    if !interactive {
        return Err(format!(
            "working directory {} is not trusted; start tickloop interactively in that directory once to review and trust it",
            safe_text(&workdir.to_string_lossy())
        ));
    }
    Ok(true)
}

async fn await_startup_directory_trust(
    console: &Console,
    input: &mut mpsc::Receiver<InputEvent>,
    settings: &UserSettings,
    workdir: &std::path::Path,
    identity: (u64, u64),
) -> Result<bool, String> {
    let id = console.workspace_trust_id()?;
    loop {
        let event = tokio::select! {
            event = input.recv() => event,
            _ = tokio::signal::ctrl_c() => return Ok(false),
        };
        match event {
            Some(InputEvent::Choice {
                menu_id,
                index: workspace_trust::TRUST_ACCEPT_CHOICE_INDEX,
                ..
            }) if menu_id == id => {
                let settings = settings.clone();
                let workdir = workdir.to_path_buf();
                let result = tokio::task::spawn_blocking(move || {
                    let current = std::fs::metadata(&workdir).map_err(|error| error.to_string())?;
                    if !current.is_dir()
                        || (current.dev(), current.ino()) != identity
                        || std::fs::canonicalize(&workdir).map_err(|error| error.to_string())?
                            != workdir
                    {
                        return Err(
                            "The displayed directory changed; exit and launch again to review it."
                                .into(),
                        );
                    }
                    settings
                        .trust_working_directory(&workdir)
                        .map_err(|error| format!("Cannot save directory trust: {error}"))
                })
                .await
                .map_err(|error| error.to_string())?;
                match result {
                    Ok(()) => return Ok(true),
                    Err(error) => console.workspace_trust_failed(error)?,
                }
            }
            Some(InputEvent::Choice { menu_id, .. } | InputEvent::Cancel { menu_id })
                if menu_id == id =>
            {
                return Ok(false);
            }
            Some(InputEvent::Eof | InputEvent::Interrupt) | None => return Ok(false),
            Some(InputEvent::Error(error)) => return Err(error),
            _ => {}
        }
    }
}

/// Shows `menu` before the Agent starts and waits for the user's choice. Its last option cancels,
/// as do esc, ctrl+c and the end of input.
async fn choose_at_startup(
    menu: console::Menu,
    preferences: tickloop_code_settings::DisplayPreferences,
    startup_console: &mut Option<(Console, mpsc::Receiver<InputEvent>)>,
) -> Result<usize, String> {
    let menu_id = menu.id;
    let cancel_index = menu.options.len() - 1;
    if let Some((console, _)) = startup_console.as_ref() {
        console.set_prompt_enabled(false)?;
        console.finish_workspace_trust()?;
        console.show_focused_menu(menu)?;
    } else {
        *startup_console = Some(Console::start_with_startup_menu(preferences, menu)?);
    }
    let (console, input) = startup_console
        .as_mut()
        .expect("startup console initialized");
    let choice = loop {
        let event = tokio::select! {
            event = input.recv() => event,
            _ = tokio::signal::ctrl_c() => break cancel_index,
        };
        match event {
            Some(InputEvent::Choice {
                menu_id: id, index, ..
            }) if id == menu_id => break index,
            Some(InputEvent::Cancel { menu_id: id }) if id == menu_id => break cancel_index,
            Some(InputEvent::Eof | InputEvent::Interrupt) | None => break cancel_index,
            Some(InputEvent::Error(error)) => return Err(error),
            _ => {}
        }
    };
    console.clear_menu(menu_id)?;
    Ok(choice)
}

async fn connect_daemon_at_startup(
    interactive: bool,
    preferences: tickloop_code_settings::DisplayPreferences,
    startup_console: &mut Option<(Console, mpsc::Receiver<InputEvent>)>,
    launch_arguments: &[String],
) -> Result<Option<tickloop_code_client::LocalAgentClient>, String> {
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    let error = match tickloop_code_client::connect_or_start(executable.clone()).await {
        Ok(client) => return Ok(Some(client)),
        Err(error) => error,
    };
    if !interactive
        || launch_arguments
            .iter()
            .any(|argument| argument == "--view-handoff")
    {
        return Err(error.to_string());
    }
    let Some(mismatch) = &error.mismatch else {
        return Err(error.to_string());
    };
    let running_executable = mismatch.running_executable.clone();
    let running_version = if let Some(running) = running_executable.clone() {
        tokio::task::spawn_blocking(move || tickloop_code_updater::Installation::detect(&running))
            .await
            .ok()
            .and_then(Result::ok)
            .flatten()
            .map(|installation| {
                format!(
                    "{} ({})",
                    installation.build.version,
                    installation.source.label()
                )
            })
    } else {
        None
    };
    let mut context = vec![
        format!("Installed Tickloop: {}", env!("CARGO_PKG_VERSION")),
        format!(
            "Running daemon: {}",
            safe_text(running_version.as_deref().unwrap_or("unknown version"))
        ),
        "The running daemon is incompatible; automatic switching is unavailable.".into(),
        safe_text(&mismatch.description),
        "Stopping it stops all running Agents and their Shell tasks. Saved history is kept. Ticks keep running.".into(),
        "After stopping, this version opens automatically with your current startup options."
            .into(),
    ];
    let mut options = vec!["Stop daemon and open this version".into()];
    if let Some(running) = &running_executable {
        context.push(format!(
            "Or open the running version from {}; its Agents keep running.",
            safe_text(&running.display().to_string())
        ));
        options.push("Open the running version — keep running Agents".into());
    }
    options.push("Cancel — keep running Agents".into());
    let cancel_index = options.len() - 1;
    let menu_id = tickloop_ids::generate();
    let menu = console::Menu {
        id: menu_id,
        title: "Stop the running daemon and continue?".into(),
        context,
        markdown: None,
        options,
        selected: cancel_index,
        confirmation: true,
    };
    let choice = choose_at_startup(menu, preferences, startup_console).await?;
    if choice == cancel_index {
        return Ok(None);
    }
    if choice != 0
        && let Some(running) = running_executable
    {
        use std::os::unix::process::CommandExt;
        // The running build's own View takes over this terminal with the same arguments.
        drop(startup_console.take());
        let error = std::process::Command::new(&running)
            .args(launch_arguments)
            .exec();
        return Err(format!(
            "Cannot open the running version {}: {error}",
            running.display()
        ));
    }
    let prepared = mismatch
        .prepare_stop()
        .await
        .map_err(|reason| format!("{error}\n\n{reason}"))?
        .ok_or("The Agent daemon changed before confirmation; launch tickloop again.")?;
    let (console, _) = startup_console
        .as_ref()
        .expect("startup console initialized");
    console.set_status("Stopping the previous Agent daemon…")?;
    prepared.stop().await?;
    let client = tickloop_code_client::connect_or_start(executable)
        .await
        .map_err(|error| error.to_string())?;
    console.finish_menu_submission(menu_id)?;
    console.set_status("")?;
    Ok(Some(client))
}

pub fn print_help() {
    println!("{}", help_text());
}

fn help_text() -> String {
    let guidance = r#"Tickloop
Usage: tickloop [--cwd DIRECTORY]
       [--add-dir PATH]
       [--mode | --permission-mode acceptEdits|auto|plan] [--deny-on-prompt | --dangerously-skip-permissions]
       [--allowedTools RULES] [--disallowedTools RULES]
       [--working-directory-is-trusted]
       [-c | --continue | -r [CHAT] | --resume [CHAT] | --chat-id CHAT] [--fork-session]
       [--no-session-persistence]
       tickloop update [--check | --status]
       tickloop daemon stop
       tickloop tick install TICK[@MARKETPLACE] | install --path PATH | update [TICK] | uninstall TICK [--keep-data] | enable TICK | disable TICK | list [--available] | login TICK [--issuer LABEL] | logout TICK
       tickloop plugin install PLUGIN[@MARKETPLACE] | install --path PATH | update [PLUGIN] | uninstall PLUGIN [--keep-data] | enable PLUGIN | disable PLUGIN | list [--available] | validate DIRECTORY | marketplace add SOURCE | marketplace list | marketplace update [MARKETPLACE] | marketplace remove MARKETPLACE
       tickloop mcp list | add NAME URL [--header 'NAME: VALUE']... [--dir] | add NAME [--env NAME=VALUE]... [--dir] -- COMMAND [ARGUMENTS...] | remove NAME [--dir] | login NAME | logout NAME

daemon stop ends the local Agent daemon and all of its workers, which is every running Agent; saved history is kept and Ticks in the Tickhost keep running.
Run tickloop again to start this build. Each config root (~/.tickloop, or TICKLOOP_CONFIG_DIR when it is set) has one daemon.
tick manages Ticks, the plugins that carry a Tick program: installs one by name from a marketplace or from its directory or archive, updates, enables, disables or uninstalls one, logs one in or out of its data source, or lists those installed or available; tickloop tick --help explains them.
plugin manages the plugins without a Tick program the same way, manages marketplaces, or checks a plugin directory; tickloop plugin --help explains them.
mcp lists the MCP servers an Agent here would have without starting any, adds or removes your own servers in the global settings or, with --dir, this directory's, and logs in to or out of a remote server.

The working directory must exist; omitting --cwd uses the current directory.
--permission-mode is the same as --mode; give one, or both with the same mode. Claude Code's dontAsk and bypassPermissions are --deny-on-prompt and --dangerously-skip-permissions here. defaultMode in settings.json takes the same names.
Unknown directories require interactive trust. --working-directory-is-trusted is an explicit host assertion for externally controlled automation and does not save trust.
Starting without -c or -r opens a new Chat.
--allowedTools / --allowed-tools and --disallowedTools / --disallowed-tools add temporary allow/deny rules to the new Agent and its Subagents. Repeat flags or separate rules with spaces or commas; quote command patterns, e.g. --allowedTools "Bash(git:*) Edit". /permissions shows these read-only Launch arguments. They are not saved and expire on restart; they cannot be combined with --continue, --resume or --chat-id. Deny, Ask, sensitive paths and Auto filtering still apply. --deny-on-prompt rejects operations that need human approval.
-c, --continue resumes the most recent Chat in this directory.
-r, --resume [CHAT] resumes the given Chat, or opens the Chat picker when CHAT is omitted.
--fork-session copies the resumed conversation into a new Chat; use it with -c or -r.
--chat-id opens or creates one exact saved Chat identity.
Chats are saved under ~/.tickloop, or TICKLOOP_CONFIG_DIR when it is set.
--no-session-persistence runs without saving Chats; so does cleanupPeriodDays 0 in settings.json.
Saved Chats and their files are never removed automatically.
/provider opens Codex subscription authorization; choose browser or device code there. Judgment settings manages the separate Jev (TypeSafe) key.
/provider: press s to sign out of Codex locally; requests already sent are not revoked.
Built-in /model choices and persistent rules are saved in settings.json there; model definitions are owned by the program version.

Enter a task; follow-up inputs are accepted while the current run continues.
With piped input, tickloop exits once its input and turns are done and its Subagents have finished and reported; a longer wait is announced on stderr, and ctrl+c exits at once.
Queued messages, commands and Shell input appear above the editor; up on the first visual row restores editable queued input.
While the Agent works, enter queues a message for its next tool boundary; ctrl+enter send now interrupts the turn and sends it at once, or with empty input the oldest queued message. Idle, ctrl+enter is enter.
ctrl+enter needs a terminal that reports it (kitty keyboard protocol: Ghostty, kitty, WezTerm, iTerm2 with CSI u, or Herdr inside one); elsewhere enter then esc does the same, since esc interrupts and queued messages start right away.
The footer contains only Subagents N when there are subagents, expanded by default.
down on the last visual input row, after history ends, enters the Subagents list at the row selected last, without submitting the draft; a folded list unfolds.
Click Subagents N to hide/show the list. In the list enter opens an Agent (modified enter is ignored), space folds descendants, x dismisses a finished subagent, and esc or up on the first row returns to the editor.
Subagents wear the colour they were given at creation; the input takes the colour of the subagent it is viewing.
The key hints always end the screen; notices share that row.
shift+up/down focuses Monitor's Shells group; enter opens the selected Shell.
space or a click on a hosted Tick or a Shell in Monitor shows or hides its live screen as a card under its row, and several cards can stay open; enter enters the card, or a click in it does: keys, ctrl+c and a single esc included, go to the Tick until esc esc leaves the card back to the list, which shows other cards with space, while the card stays shown and the Tick keeps running; a Tick that hands back keys it does not use also leaves on one esc, moves to the card above or below with the arrows, to the next row with tab and to the input with left; a click outside leaves too and then acts where it landed, so a click on another row shows its card; a Shell's screen only shows it, and esc leaves it. tab and shift+tab select every row in turn and preview its card until tab moves on, unless you enter it; a click on an edge line counting updated rows above or below selects one. Rows keep the order they joined. Press x twice within 3 seconds to perform the selected row's close/remove action: it stops a running Shell, closes a running Tick this Chat opened, cancels the subscriptions of one only they show, or removes a finished row. space on a Tick this Chat opened that no longer runs reopens it in place, restoring its subscriptions at their frequencies, and shows its card, and enter reopens it and enters the card; s subscribes a Tick at high. Close/remove actions first show a red confirmation hint on the status line; pressing another key or changing the selected row cancels it. enter reopens directly. Actions report their result on the status line. Press x twice to remove a scheduled task or unlink an Artifact; f links new Artifact feedback and h includes earlier feedback.
The shared tasks summary appears only when tasks exist. ctrl+t expands or focuses it; ctrl+n/p select, enter shows details, esc returns to the editor.
Mode and model stay right-aligned on the lower divider; Context appears only with confirmed nonzero rounded usage.
Divider edge titles have a short rule beyond the text; the expanded Subagents list has no items summary row.
esc interrupts the current turn and pauses an active Goal; Tick Subscriptions and Artifact feedback keep arriving.
/stop interrupts the current turn, pauses an active Goal and pauses Triggers until Continue or your next message.
/goal CONDITION sets the Chat's Goal, confirming replacement of an unfinished Goal; conditions are limited to 4000 characters, with longer content in a file or Artifact.
/goal opens status, full condition, usage, the last completion check, and edit, pause/resume, clear and token budget controls; /goal edit supports ctrl+g in the external editor.
/goal pause, /goal resume and /goal clear act without resuming paused Trigger receiving; Continue restores receiving separately.
Monitor keeps Goal at the bottom, outside its member list; select it and press enter for details. The status bar shows Goal usage or runtime and paused/resume guidance.
/clear starts a new Chat and stops this Chat's Shell tasks that are not in the background.
/branch [NAME] copies this conversation at its current point and switches to the child Chat.
/resume opens the Chat picker; /resume CHAT switches to that Chat without running it.
Chats you switch away from keep running in this process.
/NAME [ARGUMENTS] loads a Skill; slash completion shows descriptions and sources (ctrl+n/p selects).
Use /plugin:skill, or /skill when unique; full names and built-in commands take precedence.
Each user invocation shows a Skill row with its source, also in reopened history.
/slides [TOPIC] makes new Slides in Tickloop Artifacts; without a topic the Agent asks for one.
/provider configures connections, sign-in and the separate Jev (TypeSafe) key without changing the Chat model.
/ticks manages Ticks, the plugins with a Tick program: Installed shows versions and login states; Available installs or updates. Tab or left/right switches pages.
/plugins manages the plugins without a Tick program the same way. A Tick's login runs in its panel; installed or updated Skills load on the next request.
/mcp shows the Agent's MCP servers by source with their state, tools and exposure; enter opens a server's tools, login, logout, reconnect, and turning it off or on in this directory.
/model selects built-in models from configured connections; p opens Connection details. Model choices also apply to new Chats.
/thinking opens the Thinking picker; /thinking LEVEL sets a supported level. Auto uses the model default; /settings toggles Thinking.
/sandbox opens Mode, Overrides and Config; changes apply to this Folder on the next command.
/settings opens settings for this Chat; /permissions shows Auto, Ask and Full Access and a separate recent-activity entry.
/permissions → Auto opens Auto settings, where you can select LLM or Judgment, manage review rules and Environment, and open the shared Jev (TypeSafe) key settings.
Explicit Ask rules always require a human once-only answer. Auto defaults to this Chat's model; Jev technical failure falls back to it without changing the saved choice.
/settings → Theme opens the theme picker; the choice applies to this terminal session only.
Fullscreen draws on the alternate screen; start with TICKLOOP_AGENT_TUI=inline where that is unusable, such as iTerm2 tmux -CC.
Fullscreen reads the wheel to scroll the conversation; TICKLOOP_AGENT_DISABLE_MOUSE=1 hands the wheel back to the terminal and restores click-and-drag selection.
/diff opens current working-tree changes and Agent file changes saved in this process without calling the model.
Typing / or a command prefix shows completion; tab fills the first match.
After a completed turn, a dim prompt suggestion may appear in empty input; /settings turns generation off.
tab or right accepts the suggestion for editing; empty enter accepts and submits; esc dismisses after candidates or dialogs.
↑/↓ or ctrl+n/p selects a completion; enter fills a selected match or sends the draft.
Typing @ and part of a local path offers files and directories; only an accepted candidate is attached.
Focused lists also accept ctrl+n/p; enter confirms; esc cancels or denies.
Input uses normal editing with Emacs keys: ctrl+a/e line start/end, ctrl+b/f and alt+b/f move by character and word.
ctrl+h deletes backward; ctrl+k/u/w and alt+d/alt+backspace kill; ctrl+y yanks and alt+y cycles; ctrl+_ undoes; ctrl+s stashes the draft.
ctrl+g edits the draft in $VISUAL or $EDITOR (which may be Vim); saving and quitting updates the draft.
ctrl+v attaches the clipboard image as [Image #N], moved and deleted as one block; the model must accept image input.
ctrl+r opens history search.
History search uses ctrl+s for session/project/all scopes; enter/tab fills the draft.
ctrl+j or alt/shift+enter inserts a real newline.
ctrl+c first closes candidates, then clears a draft; on an empty prompt it interrupts a running turn as esc does, or leaves this View when the Agent is idle (the Agent keeps running in the daemon).
With an empty prompt, ctrl+d must be pressed twice within 800ms to exit.
With a draft, ctrl+d deletes forward instead of exiting.
On Unix, ctrl+z suspends this Agent process; run fg in the shell to resume it.
esc twice clears a draft into history, or opens the rewind review when input is empty.
/rewind opens the Rewind checkpoint selector.
ctrl+o or /details opens the full conversation transcript; ctrl+o, esc or q returns.
In transcript: / searches, enter accepts, n/N moves between matches; esc cancels search.
ctrl+n/p or arrows scroll; g/G or home/end jumps.
Persistent rules apply to every Chat in this working directory; session rules end with this process.
/cwd [PATH] shows or sets the main directory.
/plan [open|DESCRIPTION] enters Plan mode, starting on DESCRIPTION when given; in Plan it shows the plan, and open edits it in your editor. A plan awaiting approval reopens its review: ctrl+g edits the plan, shift+tab on the feedback field approves with that feedback.
Monitor lists this Chat's Ticks, Artifacts, background Shells and scheduled tasks, with its Goal at the bottom. On extension and terminal Tick rows, s cycles subscriptions and u sends an update when unsubscribed; p pauses/resumes a periodic subscription or scheduled task; x twice confirms removal.
The first new Goal, scheduled task or Tick in each Chat during this console session opens Monitor once without moving input focus, even when hidden. Closing it prevents further automatic opens. Existing content on opening a Chat does not trigger this; narrow terminals flash the Monitor entry instead of covering the conversation.
Shell task state and per-operation output are available only in this process; they are gone after a restart.
Background Shell completions are added to their Chat as task notifications.
A completion continues the Loop once it has worked, unless its last turn was interrupted or failed; otherwise it is only recorded.
/compact [INSTRUCTIONS] summarizes context behind a compaction boundary.
/loop [INTERVAL] [INSTRUCTION] repeats work: a fixed interval runs once immediately, then on schedule; without an interval the Agent picks each next wakeup. Without instructions it reads .tickloop/loop.md, then the config root's loop.md, else maintenance guidance. Chats without a Folder only read the config root's loop.md. Monitor shows each scheduled task; p pauses/resumes it (pending tasks cannot pause), enter opens details, x twice confirms deletion. Esc cancels a self-paced pending wakeup; fixed schedules keep running. /stop pauses receiving; send a message to resume.

/help advanced lists advanced commands; /exit closes this View; Agent work continues."#;
    format!(
        "{guidance}\n\nCommands and Skills:\n{}",
        completion::command_help(false)
    )
}

pub async fn run() -> Result<(), String> {
    run_with_arguments(std::env::args().skip(1)).await
}

pub fn check_view_handoff(path: &std::path::Path) -> Result<(), String> {
    view_upgrade::check_handoff(path)
}

pub fn prepare_view_handoff(arguments: &[std::ffi::OsString]) -> Result<(), String> {
    view_upgrade::prepare_handoff(arguments)
}

pub fn recover_view_handoff(arguments: &[std::ffi::OsString]) -> Result<bool, String> {
    view_upgrade::recover_handoff(arguments)
}

pub async fn run_with_arguments(arguments: impl Iterator<Item = String>) -> Result<(), String> {
    let launch_arguments = arguments.collect::<Vec<_>>();
    let mut arguments = launch_arguments.clone().into_iter().peekable();
    let mut mode_flag = None;
    let mut permission_mode_flag = None;
    let mut prompt_policy = PromptPolicy::Interactive;
    let mut launch_rules = Vec::new();
    let mut deny_on_prompt = false;
    let mut unrestricted = false;
    let mut caller_managed_trust = false;
    let mut continue_recent = false;
    let mut resume: Option<Option<ChatId>> = None;
    let mut exact_chat = None;
    let mut view_handoff_path = None;
    let mut fork_session = false;
    let mut session_persistence = true;
    let mut additional_directories = Vec::new();
    let mut workdir = std::env::current_dir()
        .map_err(|_| "cannot read the current directory; use an accessible directory".to_owned())?;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--view-handoff" => {
                view_handoff_path = Some(PathBuf::from(
                    arguments.next().ok_or("missing View handoff path")?,
                ));
            }
            "--help" | "-h" => {
                print_help();
                return Ok(());
            }
            "--config" => {
                return Err(
                    "--config is not supported; use /model to choose from the built-in catalog"
                        .into(),
                );
            }
            "--auto-reviewer-config" | "--permission-checker-config" => {
                return Err("Independent Auto reviewer configuration has moved to /permissions → Auto mode. Remove this startup flag and choose LLM or Judgment there.".into());
            }
            "--mode" | "--permission-mode" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| format!("{argument} requires acceptEdits, auto or plan"))?;
                let (mode, warning) =
                    parse_execution_mode(&value).map_err(|error| safe_text(&error))?;
                if let Some(warning) = warning {
                    eprintln!("Warning: {warning}");
                }
                let given = Some((value, mode));
                if argument == "--mode" {
                    mode_flag = given;
                } else {
                    permission_mode_flag = given;
                }
            }
            "--allowedTools" | "--allowed-tools" | "--disallowedTools" | "--disallowed-tools" => {
                let value = arguments
                    .next_if(|value| !value.starts_with('-'))
                    .ok_or_else(|| format!("{argument} requires permission rules"))?;
                let decision = if matches!(argument.as_str(), "--allowedTools" | "--allowed-tools")
                {
                    tickloop_harness::protocol::Decision::Allow
                } else {
                    tickloop_harness::protocol::Decision::Deny
                };
                launch_rules.extend(launch_rules::parse(&value, decision)?);
                while let Some(value) = arguments.next_if(|value| !value.starts_with('-')) {
                    launch_rules.extend(launch_rules::parse(&value, decision)?);
                }
            }
            "--deny-on-prompt" => {
                deny_on_prompt = true;
                prompt_policy = PromptPolicy::DenyOnPrompt;
            }
            "--dangerously-skip-permissions" => {
                unrestricted = true;
                prompt_policy = PromptPolicy::Unrestricted;
            }
            "--working-directory-is-trusted" => caller_managed_trust = true,
            "--add-dir" => {
                let path = arguments
                    .next_if(|value| !value.starts_with("--"))
                    .filter(|value| !value.is_empty())
                    .ok_or("--add-dir requires a directory path")?;
                additional_directories.push(PathBuf::from(path));
            }
            "--cwd" => {
                workdir = PathBuf::from(arguments.next().ok_or("--cwd requires a directory path")?)
            }
            "-c" | "--continue" => continue_recent = true,
            "-r" | "--resume" => {
                let chat = arguments
                    .next_if(|value| !value.starts_with('-'))
                    .map(|value| {
                        ChatId::parse(&value).ok_or_else(|| {
                            format!(
                                "--resume expects a Chat ID, got {}; omit it to choose a Chat",
                                safe_text(&value)
                            )
                        })
                    })
                    .transpose()?;
                resume = Some(chat);
            }
            "--chat-id" => {
                let value = arguments.next().ok_or("--chat-id requires a Chat ID")?;
                exact_chat = Some(ChatId::parse(&value).ok_or_else(|| {
                    format!("--chat-id expects a Chat ID, got {}", safe_text(&value))
                })?);
            }
            "--fork-session" => fork_session = true,
            "--no-session-persistence" => session_persistence = false,
            _ => {
                return Err(format!(
                    "unknown argument {}; use --help",
                    safe_text(&argument)
                ));
            }
        }
    }
    let execution_mode_override = match (mode_flag, permission_mode_flag) {
        (Some((mode_value, mode)), Some((permission_value, permission_mode)))
            if mode != permission_mode =>
        {
            return Err(format!(
                "--mode {} and --permission-mode {} name different execution modes; give only one",
                safe_text(&mode_value),
                safe_text(&permission_value)
            ));
        }
        (mode_flag, permission_mode_flag) => {
            mode_flag.or(permission_mode_flag).map(|(_, mode)| mode)
        }
    };
    let mut view_handoff = view_handoff_path
        .as_deref()
        .map(view_upgrade::read_handoff)
        .transpose()?;
    if let Some(handoff) = &view_handoff {
        exact_chat = handoff.selection.chat.map(Into::into);
        continue_recent = false;
        resume = None;
        fork_session = false;
    }
    let resumes_saved_chat = continue_recent || resume.is_some();
    if !launch_rules.is_empty() && (resumes_saved_chat || exact_chat.is_some()) {
        if view_handoff.is_none() {
            return Err(
                "launch tool rules require a new Agent; omit --continue, --resume and --chat-id"
                    .into(),
            );
        }
        launch_rules.clear();
    }
    if fork_session && !resumes_saved_chat {
        return Err("--fork-session can only be used with --continue or --resume".into());
    }
    if usize::from(continue_recent)
        + usize::from(resume.is_some())
        + usize::from(exact_chat.is_some())
        > 1
    {
        return Err("--continue, --resume, and --chat-id cannot be combined".into());
    }
    if !session_persistence
        && (resumes_saved_chat || exact_chat.is_some())
        && view_handoff.is_none()
    {
        return Err(
            "--no-session-persistence cannot be combined with --continue, --resume, or --chat-id"
                .into(),
        );
    }
    if deny_on_prompt && unrestricted {
        return Err(
            "--deny-on-prompt and --dangerously-skip-permissions cannot be combined".into(),
        );
    }
    if unrestricted && view_handoff.is_none() {
        eprintln!(
            "Warning: unrestricted execution skips permission prompts and automatic review; use it only inside an externally isolated environment"
        );
    }
    let workdir_help =
        "use --cwd with an existing directory, or omit --cwd to use the current directory";
    let metadata = std::fs::metadata(&workdir).map_err(|error| {
        format!(
            "cannot use working directory {}: {error}; {workdir_help}",
            safe_text(&workdir.to_string_lossy())
        )
    })?;
    if !metadata.is_dir() {
        return Err(format!(
            "working directory {} is not a directory; {workdir_help}",
            safe_text(&workdir.to_string_lossy())
        ));
    }
    workdir = workdir
        .canonicalize()
        .map_err(|error| format!("cannot resolve working directory: {error}"))?;
    additional_directories = additional_directories.into_iter().filter_map(|path| {
        let path = workdir.join(path);
        match path.canonicalize().ok().filter(|path| path.is_dir()) {
            Some(path) => Some(path),
            None => {
                eprintln!("Warning: additional directory {} is unavailable or not a directory; skipped", safe_text(&path.to_string_lossy()));
                None
            }
        }
    }).collect();
    additional_directories.sort();
    additional_directories.dedup();
    let interactive_terminal = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let config_root = config_root_directory().map(normalize_state_root);
    // Use the canonicalized root like `Agent::open`, so settings warnings reported at startup match
    // those reported later when new Chats are created.
    let user_settings = config_root
        .as_deref()
        .map(|root| UserSettings::in_root(&root.canonicalize().unwrap_or_else(|_| root.into())));
    let mut startup_console = None;
    if !caller_managed_trust
        && startup_directory_needs_trust(user_settings.as_ref(), &workdir, interactive_terminal)?
    {
        if view_handoff.is_some() {
            return Err("Working-directory trust changed during the View upgrade".into());
        }
        let settings = user_settings
            .as_ref()
            .expect("trust gate requires user settings");
        let (console, mut input) =
            Console::start_with_workspace_trust(settings.display_preferences(), workdir.clone())?;
        if !await_startup_directory_trust(
            &console,
            &mut input,
            settings,
            &workdir,
            (metadata.dev(), metadata.ino()),
        )
        .await?
        {
            return Ok(());
        }
        startup_console = Some((console, input));
    }
    let mut saved_settings = user_settings.as_ref().map(UserSettings::load);
    let saved_model = saved_settings
        .as_mut()
        .map(LoadedSettings::configured_model)
        .transpose()
        .map_err(|error| format!("invalid model settings: {error}; use a readable settings directory (TICKLOOP_CONFIG_DIR). --no-session-persistence disables Chat history but settings still need an accessible directory"))?
        .flatten();
    let display_preferences = saved_settings
        .as_mut()
        .map(LoadedSettings::display_preferences)
        .unwrap_or_default();
    let status_lines = saved_settings
        .as_mut()
        .and_then(LoadedSettings::status_lines);
    let monitor_preference = saved_settings
        .as_ref()
        .map(LoadedSettings::monitor_preference)
        .unwrap_or_default();
    let monitor_width = saved_settings
        .as_mut()
        .and_then(LoadedSettings::monitor_width);
    if let Some(settings) = saved_settings.as_mut() {
        settings.prompt_suggestion_enabled();
        settings.additional_directories(&workdir);
        if let Err(error) = settings.sandbox_for(&workdir) {
            settings.warn(error);
        }
    }
    let saved_execution_mode = saved_settings
        .as_mut()
        .and_then(LoadedSettings::default_execution_mode);
    let startup_settings_warnings = saved_settings
        .map(LoadedSettings::into_warnings)
        .unwrap_or_default();
    // A built-in preview is not a model selection, even when its API key exists.
    let model_selected = saved_model.is_some();
    let config = saved_model.unwrap_or_else(|| {
        tickloop_harness::model::cards::ModelCatalog::defaults()
            .get(
                "deepseek-api",
                tickloop_harness::model::access::AccessCatalog::defaults()
                    .recommended_model("deepseek-api")
                    .expect("built-in recommendation"),
            )
            .expect("built-in default")
    });
    let host_options = AgentHostOptions {
        execution_mode_override,
        prompt_policy,
        directory_trust: if caller_managed_trust {
            DirectoryTrustPolicy::CallerManaged
        } else {
            DirectoryTrustPolicy::RequireUserTrust
        },
    };
    use std::os::unix::ffi::OsStrExt;
    let (host_work_context, view_host) = if let Some(context) = HerdrContext::from_environment()? {
        let pane = current_pane(&SocketHerdr::new(context.clone()), &context.caller_pane)
            .await
            .map_err(|error| format!("cannot inspect the calling Herdr Pane: {error}"))?;
        let metadata = std::fs::metadata(&context.socket)
            .map_err(|error| format!("cannot inspect Herdr endpoint: {error}"))?;
        let host_instance = endpoint_instance(&metadata);
        (
            Some(HostWorkContext {
                socket: context.socket.clone(),
                workspace: Some(pane.workspace_id.clone()),
            }),
            Some(HostLocation {
                host_instance,
                socket: context.socket,
                workspace: Some(pane.workspace_id),
                tab: Some(pane.tab_id),
                pane: Some(pane.pane_id),
                terminal: Some(pane.terminal_id),
                client: None,
            }),
        )
    } else {
        (None, None)
    };
    let launch = LaunchSpec {
        folder: workdir.clone(),
        additional_directories,
        config_root: config_root.clone(),
        model: Some(config.clone()),
        host_options,
        session_persistence,
        exact_chat: None,
        host_work_context,
        environment: std::env::vars_os()
            .map(|(name, value)| EnvironmentValue {
                name: name.as_os_str().as_bytes().to_vec(),
                value: value.as_os_str().as_bytes().to_vec(),
            })
            .collect(),
    };
    #[cfg(test)]
    let agent = if std::env::var_os("TICKLOOP_TEST_CODEX_OAUTH").is_some() {
        let core = tickloop_code_daemon::build_runtime(&launch, None).await?;
        Agent::from_embedded_launch(core, launch)
    } else {
        let Some(client) = connect_daemon_at_startup(
            interactive_terminal,
            display_preferences,
            &mut startup_console,
            &launch_arguments,
        )
        .await?
        else {
            return Ok(());
        };
        Agent::connected(client, launch, view_host)
    };
    #[cfg(not(test))]
    let agent = {
        let Some(client) = connect_daemon_at_startup(
            interactive_terminal,
            display_preferences,
            &mut startup_console,
            &launch_arguments,
        )
        .await?
        else {
            return Ok(());
        };
        Agent::connected(client, launch, view_host)
    };
    agent.use_launch_rules(launch_rules);
    if (resumes_saved_chat || exact_chat.is_some())
        && !agent.saves_chats()
        && view_handoff.is_none()
    {
        return Err(
            "conversations are not saved because cleanupPeriodDays is 0 in settings.json; remove it or set it to a positive number to keep and resume Chats".into(),
        );
    }
    let startup = if let Some(chat) = exact_chat {
        open_exact_chat(
            &agent,
            chat,
            &workdir,
            view_handoff.as_mut().map(|handoff| &mut handoff.console),
        )
        .await?
    } else if continue_recent {
        let recent = agent
            .saved_chats(&workdir)
            .await
            .map_err(|error| safe_text(&error.to_string()))?
            .into_iter()
            .next()
            .ok_or("No conversation found to continue")?;
        StartupChat::Resumed(Box::new(
            open_saved_chat(&agent, recent.id, fork_session).await?,
        ))
    } else if let Some(requested) = resume {
        match requested {
            Some(chat) => {
                StartupChat::Resumed(Box::new(open_saved_chat(&agent, chat, fork_session).await?))
            }
            None => StartupChat::Picker {
                as_new_chat: fork_session,
            },
        }
    } else {
        StartupChat::New
    };
    let startup = if let Some(handoff) = view_handoff {
        StartupChat::Upgrade(Box::new(startup), Box::new(handoff))
    } else {
        startup
    };
    let (console, input) = match startup_console.take() {
        Some(started) => started,
        None if view_handoff_path.is_some() => Console::start_for_upgrade(display_preferences)?,
        None => Console::start_with_display(interactive_terminal, display_preferences)?,
    };
    let console = &console;
    console.set_saves_chats(agent.saves_chats())?;
    console.set_status_lines(status_lines)?;
    console.set_monitor_preference(monitor_preference)?;
    console.set_monitor_width(monitor_width.map(usize::from))?;
    // The Agent's Ticks run in the Tickhost and are probed there.
    console.add_member_probe(std::sync::Arc::new(hosted_probe::HostedProbe::new(
        agent.clone(),
    )))?;
    console.configure_input_history(config_root.as_deref(), &workdir)?;
    console.configure_skill_completion(config_root.clone())?;
    // The image cache expires with Transcripts, so a session that saves no Chats caches nothing.
    let image_cache_root = config_root
        .as_ref()
        .filter(|_| agent.saves_chats())
        .map(|root| root.canonicalize().unwrap_or_else(|_| root.clone()));
    console.configure_image_cache(image_cache_root.as_deref())?;
    let draft = agent
        .configuration(ConfigTarget::NewDraft)
        .await
        .map_err(|error| error.to_string())?;
    console.set_input_context(None, &workdir, &draft.additional_directories)?;
    let needs_model_selection =
        interactive_terminal && matches!(startup, StartupChat::New) && !model_selected;
    initialize_startup_console(
        console,
        (!needs_model_selection).then_some(&config),
        &workdir,
        execution_mode_override
            .or(saved_execution_mode)
            .unwrap_or_default(),
        startup_settings_warnings,
    )?;
    console.finish_workspace_trust()?;
    let result = interact(
        &agent,
        workdir,
        console,
        input,
        StartupState {
            chat: startup,
            model_selected,
            user_settings,
            launch_arguments: launch_arguments.into_iter().map(Into::into).collect(),
        },
        config_root,
    )
    .await;
    let console_cleanup = console.set_prompt_enabled(false);
    let view_cleanup = agent.leave_view().await.map_err(|error| error.to_string());
    if let Some(error) = agent.transcript_write_error() {
        eprintln!(
            "Warning: conversation was not fully saved: {}",
            safe_text(&error)
        );
    }
    result.and(console_cleanup).and(view_cleanup)
}

/// The Chat entered at startup: a new one without arguments; `-c`/`-r CHAT` were already resumed
/// before startup; `-r` opens the picker.
enum StartupChat {
    New,
    Resumed(Box<tickloop_harness::protocol::ChatSnapshot>),
    Picker {
        as_new_chat: bool,
    },
    Upgrade(Box<StartupChat>, Box<view_upgrade::ViewHandoff>),
    /// A View handoff carried a Chat the daemon no longer runs; the View opens without it.
    Ended,
}

/// Opens the Chat named at startup. Without a handoff its saved history is loaded, or an empty
/// Chat with that id is created. A handoff only reconnects the Chat the daemon runs: when the
/// daemon no longer runs it, the restored View leaves that Chat, and nothing is loaded or created
/// in place of the live connection.
async fn open_exact_chat(
    agent: &Agent,
    chat: ChatId,
    workdir: &std::path::Path,
    handoff: Option<&mut console::ConsoleUpgrade>,
) -> Result<StartupChat, String> {
    let opened = if handoff.is_some() {
        agent.snapshot(chat.into()).await
    } else {
        agent.resume_chat(chat.into()).await
    };
    match opened {
        Ok(snapshot) => Ok(StartupChat::Resumed(Box::new(snapshot))),
        Err(error) if error.code == AccessErrorCode::NotFound => {
            if let Some(console) = handoff {
                console.leave_chat();
                return Ok(StartupChat::Ended);
            }
            agent
                .restore_empty_chat(chat.into(), workdir.to_owned())
                .await
                .map_err(|error| format!("Failed to create Chat {}: {error}", chat.public()))?;
            let snapshot = agent
                .snapshot(chat.into())
                .await
                .map_err(|error| format!("Failed to open Chat {}: {error}", chat.public()))?;
            Ok(StartupChat::Resumed(Box::new(snapshot)))
        }
        Err(error) => Err(format!("Failed to resume Chat {}: {error}", chat.public())),
    }
}

struct StartupState {
    chat: StartupChat,
    /// Whether settings contain a valid explicit selection, before the preview fallback.
    model_selected: bool,
    user_settings: Option<UserSettings>,
    launch_arguments: Vec<std::ffi::OsString>,
}

const MODEL_SETUP_STATUS: &str = "Choose model";
const MODEL_SETUP_PROMPT: &str = "Choose a model to get started";

fn initialize_startup_console(
    console: &Console,
    config: Option<&ModelConfig>,
    workdir: &std::path::Path,
    execution_mode: ExecutionMode,
    warnings: Vec<String>,
) -> Result<(), String> {
    console.update_status_area(
        execution_mode.to_string(),
        config.map_or_else(
            || MODEL_SETUP_STATUS.into(),
            |config| safe_text(settings::model_name(config)),
        ),
        config.and_then(initial_context_usage),
        None,
    )?;
    if let Some(config) = config {
        write_banner(console, config, workdir)?;
    } else {
        console.write_roles(&startup_banner_with_model(
            MODEL_SETUP_PROMPT.into(),
            workdir,
        ))?;
    }
    show_settings_warnings(console, warnings)?;
    console.set_prompt_enabled(true)
}

/// Resumes or copies a saved Chat per `--fork-session`, with error messages following Claude Code.
async fn open_saved_chat(
    agent: &Agent,
    chat: ChatId,
    as_new_chat: bool,
) -> Result<tickloop_harness::protocol::ChatSnapshot, String> {
    let opened = if as_new_chat {
        agent.resume_as_new_chat(chat.into()).await
    } else {
        agent.resume_chat(chat.into()).await
    };
    opened.map_err(|error| match error.code {
        AccessErrorCode::NotFound => {
            format!("No conversation found with Chat ID: {}", chat.public())
        }
        _ => format!(
            "Failed to resume Chat {}: {}",
            chat.public(),
            safe_text(&error.to_string())
        ),
    })
}

/// The brand mark beside the startup banner: the saddle net drawn in braille, sixteen columns
/// by eight rows.
const BANNER_MARK: [&str; 8] = [
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⡤⠶⣄⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⡼⠁⣰⠛⡆⠀⠀",
    "⠀⢀⣤⡀⠀⠀⠀⢀⡞⢁⡴⠃⠀⢹⡀⠀",
    "⠀⠳⢤⣹⣦⣠⣶⣯⡴⠋⠀⠀⠀⠀⣇⠀",
    "⠀⠀⠀⠀⢳⠀⠀⠀⢳⡀⠀⠀⠀⠀⡼⠀",
    "⠀⠀⠀⠀⠈⡇⠀⠀⠀⣇⠀⠀⢀⡼⠁⠀",
    "⠀⠀⠀⠀⠀⢹⡀⠀⠀⢸⡀⣠⠞⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠉⠓⠦⠖⠛⠁⠀⠀⠀⠀",
];
/// The mark rows the banner's three lines of text sit beside, centring them on the mark.
const BANNER_TEXT_FIRST_ROW: usize = 2;
const BANNER_MARK_GAP: &str = "  ";

/// Startup banner following Claude Code's condensed logo: the mark, then beside it the product
/// name in bold with its version dimmed, the model and the working directory dimmed. The
/// service endpoint lives in Session status instead, and command tips belong to the footer, as
/// neither appears in Claude Code's banner.
fn write_banner(
    console: &Console,
    config: &ModelConfig,
    workdir: &std::path::Path,
) -> Result<(), String> {
    if !console.is_interactive() {
        outln!(
            console,
            "Tickloop · {} · {} · {:?}",
            safe_text(&config.name),
            safe_text(&config.model),
            config.protocol
        );
        outln!(
            console,
            "Service: {}\n/help lists commands; /exit closes this View; Agent work continues.",
            safe_text(&config.base_url)
        );
        return Ok(());
    }
    console.write_roles(&startup_banner_spans(config, workdir))
}

/// Puts the startup banner, for the model and folder `snapshot` names, at the top of its Chat in
/// this View, unless the Chat already has one there.
fn ensure_snapshot_banner(
    console: &Console,
    snapshot: &tickloop_harness::protocol::ChatSnapshot,
) -> Result<(), String> {
    console.ensure_chat_banner(
        snapshot.id.into(),
        &startup_banner_spans(&snapshot.model, shown_folder(snapshot.folder.as_deref())),
    )
}

fn startup_banner_spans(
    config: &ModelConfig,
    workdir: &std::path::Path,
) -> Vec<(String, Option<Role>)> {
    startup_banner_with_model(
        format!("{} · {:?}", safe_text(&config.model), config.protocol),
        workdir,
    )
}

fn startup_banner_with_model(
    model: String,
    workdir: &std::path::Path,
) -> Vec<(String, Option<Role>)> {
    let directory = workdir.to_string_lossy().into_owned();
    let directory = match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && directory.starts_with(&home) => {
            format!("~{}", &directory[home.len()..])
        }
        _ => directory,
    };
    let text_lines: [Vec<(String, Role)>; 3] = [
        banner_title(None),
        vec![(model, Role::MutedText)],
        vec![(safe_text(&directory), Role::MutedText)],
    ];
    let mut spans = Vec::new();
    for (row, mark) in BANNER_MARK.iter().enumerate() {
        spans.push(((*mark).into(), Some(Role::BrandMark)));
        if let Some(text) = row
            .checked_sub(BANNER_TEXT_FIRST_ROW)
            .and_then(|line| text_lines.get(line))
        {
            spans.push((BANNER_MARK_GAP.into(), None));
            spans.extend(text.iter().cloned().map(|(text, role)| (text, Some(role))));
        }
        spans.push(("\n".into(), None));
    }
    spans
}

/// The banner's title: the version this View runs, followed by the Agent's own build when the
/// Agent runs another one.
fn banner_title(agent_runtime: Option<&str>) -> Vec<(String, Role)> {
    let mut title = vec![
        ("Tickloop".into(), Role::Title),
        (format!(" v{}", env!("CARGO_PKG_VERSION")), Role::MutedText),
    ];
    if let Some(agent_runtime) = agent_runtime {
        title.push((format!(" ({agent_runtime})"), Role::MutedText));
    }
    title
}

/// The banner line holding the title, and its row within the banner.
fn banner_title_line(agent_runtime: Option<&str>) -> (usize, Vec<(String, Option<Role>)>) {
    let mut line = vec![
        (
            BANNER_MARK[BANNER_TEXT_FIRST_ROW].into(),
            Some(Role::BrandMark),
        ),
        (BANNER_MARK_GAP.into(), None),
    ];
    line.extend(
        banner_title(agent_runtime)
            .into_iter()
            .map(|(text, role)| (text, Some(role))),
    );
    (BANNER_TEXT_FIRST_ROW, line)
}

const PLAN_OPEN_ARGUMENT: &str = "open";
const PLAN_MODE_ENABLED: &str = "Plan mode is active. Enter a task to make a plan.";
const NO_PLAN_WRITTEN: &str = "Already in plan mode. No plan written yet.";

/// `/plan [open|DESCRIPTION]`, after Claude Code: outside Plan it enters Plan and sends a
/// description as the next prompt; in Plan it shows the current plan, and `open` edits it in the
/// external editor. A plan awaiting approval reopens its review, where `open` edits that plan.
/// Returns the description to send.
async fn run_plan_command(
    agent: &Agent,
    console: &Console,
    menus: &mut Menus,
    chat: Option<ChatId>,
    argument: &str,
) -> Result<Option<String>, String> {
    let argument = argument.trim();
    let opens_plan = argument == PLAN_OPEN_ARGUMENT;
    let description = (!argument.is_empty() && !opens_plan).then(|| argument.to_owned());
    let Some(chat) = chat else {
        agent
            .change_configuration(
                ConfigTarget::NewDraft,
                ConfigurationChange::ExecutionMode {
                    mode: ExecutionMode::Plan,
                },
            )
            .await
            .map_err(|error| error.to_string())?;
        if description.is_none() {
            outln!(console, "{PLAN_MODE_ENABLED}");
        }
        return Ok(description);
    };
    let snapshot = agent
        .snapshot(chat.into())
        .await
        .map_err(|error| error.to_string())?;
    // Plain input reaches here without the editor's scope check.
    if snapshot.delegation.is_some() {
        return Err(completion::command_refusal(
            completion::CommandScope::Child,
            "/plan",
            completion::CHILD_PLAN_REPORTED,
        ));
    }
    let plan = snapshot
        .plans
        .iter()
        .find(|plan| plan.loop_id == snapshot.chat_loop.id);
    if description.is_none() && plan.is_some_and(|plan| plan.approval_pending) {
        menus.workflow(console, Workflow::plan(agent, chat.into()).await?)?;
        if opens_plan && console.is_interactive() {
            console.request_plan_edit(RequestedPlanEdit::Review)?;
        }
        return Ok(None);
    }
    let was_in_plan = snapshot.policy.mode == ExecutionMode::Plan;
    if !was_in_plan {
        agent
            .set_execution_mode(chat.into(), ExecutionMode::Plan)
            .await
            .map_err(|error| error.to_string())?;
        if description.is_some() {
            return Ok(description);
        }
        if !opens_plan {
            outln!(console, "{PLAN_MODE_ENABLED}");
            return Ok(None);
        }
    } else if !opens_plan {
        match plan {
            Some(_) if console.is_interactive() => {
                menus.workflow(console, Workflow::plan(agent, chat.into()).await?)?;
            }
            Some(plan) => outln!(
                console,
                "Plan version {} · approved mode {:?}\n{}",
                plan.version,
                plan.approved_execution_mode,
                safe_text(&plan.markdown)
            ),
            None => outln!(console, "{NO_PLAN_WRITTEN}"),
        }
        return Ok(None);
    }
    let Some(path) = agent
        .plan_file(chat.into())
        .await
        .map_err(|error| error.to_string())?
    else {
        outln!(
            console,
            "{}",
            if was_in_plan {
                NO_PLAN_WRITTEN
            } else {
                PLAN_MODE_ENABLED
            }
        );
        return Ok(None);
    };
    if console.is_interactive() {
        console.request_plan_edit(RequestedPlanEdit::File(path))?;
    } else {
        outln!(
            console,
            "Plan file: {}",
            safe_text(&path.display().to_string())
        );
    }
    Ok(None)
}

fn close_login_session(
    session: LoginSession,
    console: &Console,
    menus: &mut Menus,
    cancelled: bool,
) -> Result<(), String> {
    let chat = session.flow.chat();
    let completed = session.flow.completed();
    session.flow.close(console)?;
    if session.from_provider {
        menus.note_provider_login_result(chat, session.succeeded);
    }
    if cancelled && !completed {
        console.write("Login cancelled.\n")?;
    }
    Ok(())
}

async fn finish_provider_login(
    agent: &Agent,
    console: &Console,
    session: &mut LoginSession,
    result: &Result<String, String>,
) -> Result<(), String> {
    session.succeeded = result.is_ok();
    let mut notice = result.clone();
    if let Ok(account) = result
        && let Some(chat) = session.flow.chat()
        && let Err(error) = agent.refresh_configuration(chat).await
    {
        notice = Ok(format!(
            "{account} · Cannot refresh Agent configuration: {}",
            safe_text(&error.to_string())
        ));
    }
    login::complete_provider_login(&mut session.flow, console, &notice)
}

/// Local management commands stay available after cancelling model setup; messages and Skills
/// return to setup before an optimistic echo or a Chat can be created.
fn model_setup_blocks_input(line: &str) -> bool {
    let Some(command) = line.split_whitespace().next() else {
        return false;
    };
    if !command.starts_with('/')
        || command == "/compact"
        || (command == "/goal" && line.trim() != "/goal")
        || (command == "/plan"
            && !matches!(line.trim()["/plan".len()..].trim(), "" | PLAN_OPEN_ARGUMENT))
    {
        return true;
    }
    command != "/"
        && !completion::builtin_commands().iter().any(|spec| {
            spec.command == command && spec.menu_group != Some(completion::CommandGroup::Skills)
        })
}

async fn interact(
    agent: &Agent,
    workdir: PathBuf,
    console: &Console,
    input: mpsc::Receiver<InputEvent>,
    startup: StartupState,
    config_root: Option<PathBuf>,
) -> Result<(), String> {
    // This is a View integration, not a daemon integration: only a terminal actually hosted
    // by Herdr owns a pane identity. Ordinary terminals never start the reporter.
    let mut herdr_reporter = herdr_agent::Reporter::from_environment(console.is_interactive());
    let result = interact_with_herdr(
        agent,
        workdir,
        console,
        input,
        startup,
        config_root,
        &mut herdr_reporter,
    )
    .await;
    if let Some(reporter) = &mut herdr_reporter {
        reporter.shutdown().await;
    }
    result
}

async fn interact_with_herdr(
    agent: &Agent,
    workdir: PathBuf,
    console: &Console,
    mut input: mpsc::Receiver<InputEvent>,
    startup: StartupState,
    config_root: Option<PathBuf>,
    herdr_reporter: &mut Option<herdr_agent::Reporter>,
) -> Result<(), String> {
    let user_settings = startup.user_settings;
    console.set_prompt_suggestion_enabled(
        user_settings
            .as_ref()
            .is_none_or(|settings| settings.load().prompt_suggestion_enabled()),
    )?;
    if let Some(settings) = &user_settings {
        console.set_shift_tab_modes(settings.load().shift_tab_modes())?;
    }
    agent.use_user_settings(user_settings.clone());
    let mut events = agent.subscribe();
    let mut feed_gaps = agent.subscribe_feed_gaps();
    let mut user_shell_events = agent.subscribe_user_shells();
    let mut task_notifications = agent.subscribe_task_notifications();
    let interactive = console.is_interactive();
    let update_monitor = if interactive {
        tickloop_code_updater::UpdateMonitor::detect()
            .ok()
            .flatten()
    } else {
        None
    };
    let mut update_notice_refresh = tokio::time::interval(std::time::Duration::from_secs(1));
    update_notice_refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut activation_refresh = tokio::time::interval(runtime_upgrade::CHECK_INTERVAL);
    activation_refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut activation_checks = tokio::task::JoinSet::new();
    let mut initial_activation = !matches!(&startup.chat, StartupChat::Upgrade(_, _));
    let mut ready_runtime: Option<tickloop_code_updater::SelectedRuntime> = None;
    let mut runtime_activation: Option<runtime_upgrade::Activation> = None;
    let mut runtime_selection = None;
    let mut runtime_presentation = None;
    let mut installation_notice = None;
    let mut view_activation_notice = None;
    let mut menus = Menus::new(user_settings.clone(), config_root.clone());
    // Prompt suggestions reach the Chat's model directly with this View's credentials.
    let suggestion_models = Arc::new(tickloop_harness::model::LocalModelClients::new(
        config_root.clone(),
    ));
    let mut artifact_loads = tokio::task::JoinSet::new();
    let mut tick_package_tasks = ticks_work::TickPackageTasks::new(agent.clone());
    let mut mcp_tasks = mcp_work::McpTasks::default();
    // While `/mcp` shows servers that are still connecting, it reads them again this often.
    let mut mcp_refresh = tokio::time::interval(std::time::Duration::from_secs(1));
    mcp_refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut suggestions: tokio::task::JoinSet<(
        prompt_suggestion::SuggestionRequest,
        prompt_suggestion::SuggestionResult,
    )> = tokio::task::JoinSet::new();
    let mut suggestible_runs = HashSet::new();
    // Runs this View interrupted by esc or ctrl+c; their prompt may come back to the editor.
    let mut user_interrupted_runs: HashSet<RunId> = HashSet::new();
    let mut suggestion_diagnostics: HashMap<uuid::Uuid, (uuid::Uuid, &'static str)> =
        HashMap::new();
    let mut artifact_refresh = tokio::time::interval(std::time::Duration::from_millis(50));
    artifact_refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut logins: HashMap<uuid::Uuid, LoginSession> = HashMap::new();
    let mut provider_logout_chat = None;
    let mut reauthenticate_after_logout = false;
    let mut logout_target = login::SignInTarget::Codex;
    let mut logout: Option<LogoutFlow> = None;
    let mut tools = ToolPresentation::new(!interactive);
    let mut message_groups = MessageGroups::default();
    let mut subscription_refresh = tokio::time::interval(SUBSCRIPTION_REFRESH_INTERVAL);
    subscription_refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut member_probe = tokio::time::interval(MEMBER_PROBE_INTERVAL);
    member_probe.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut animation_refresh = tokio::time::interval(ANIMATION_FRAME_DURATION);
    animation_refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut elapsed_refresh = tokio::time::interval(std::time::Duration::from_secs(1));
    elapsed_refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut transcript_refresh = tokio::time::interval(TRANSCRIPT_REFRESH_INTERVAL);
    transcript_refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // The selected Chat changed since an open transcript view was last rebuilt.
    let mut transcript_refresh_due = false;
    let mut working_shimmer = RunningShimmer::default();
    let task_center = ShellTaskCenter::new(agent.clone());
    let mut prompt_watchdog_refresh = tokio::time::interval(std::time::Duration::from_secs(1));
    prompt_watchdog_refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut prompt_watchdog = ShellTaskPromptWatchdog::default();
    let prompt_watchdog_epoch = std::time::Instant::now();
    let mut pending: HashMap<uuid::Uuid, Receipt> = HashMap::new();
    let mut thinking_runs: HashSet<RunId> = HashSet::new();
    let mut thinking_previews: HashMap<uuid::Uuid, ThinkingPreview> = HashMap::new();
    let mut reply_lines: HashMap<uuid::Uuid, ReplyLines> = HashMap::new();
    let mut unpaced_runs: HashSet<RunId> = HashSet::new();
    let mut deferred_events = VecDeque::new();
    let mut merged_echoes = MergedEchoes::default();
    let mut merged_echoes_view = None;
    let mut permissions = HashMap::new();
    let mut questions: HashMap<uuid::Uuid, QuestionRequest> = HashMap::new();
    let mut compactions = tokio::task::JoinSet::new();
    let mut compaction_chats = HashMap::new();
    let mut directed_summaries = DirectedSummaryTasks::default();
    let mut summary_recovery_required: HashSet<ChatId> = HashSet::new();
    let mut selected_chat = None;
    let mut global = crate::agent_console::AgentConsole::new(
        agent.access(),
        agent.daemon_epoch(),
        workdir.clone(),
        agent.host_location(),
    )
    .with_store_root(config_root.clone());
    let mut overview_active = false;
    let mut history_return_overview = false;
    let mut visible_family: Vec<ChatId> = Vec::new();
    // The daemon fact behind the shared catalog; set by a failed refresh and cleared by the next success.
    let mut daemon_unreachable: Option<String> = None;
    let mut global_refresh = tokio::time::interval(std::time::Duration::from_secs(1));
    global_refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut ui_selected = selected_chat;
    let mut background_notice: Option<(ChatId, BackgroundRunNotice)> = None;
    let mut transcript_write_error = None;
    let mut loop_trigger_notices: Vec<(ChatId, String)> = Vec::new();
    let mut displayed_sequences: HashMap<uuid::Uuid, u64> = HashMap::new();
    let mut eof = false;
    let mut background_work = background_work::BackgroundWork::default();
    let mut background_poll = tokio::time::interval(background_work::POLL_INTERVAL);
    background_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut failed = false;
    let mut status_dirty = true;
    let mut completed_task_retention = completed_tasks::CompletedTaskRetention::default();
    let mut completed_task_cleanups = tokio::task::JoinSet::new();
    let mut activity_dirty = false;
    // Thinking text arrived since the activity row was last drawn; the next animation frame
    // shows it, so a fast stream redraws at the frame rate rather than once per delta.
    let mut thinking_frame_due = false;
    // Monitor's subscriptions and Shell tasks are due to be read again on its own cadence.
    let mut monitor_refresh_due = false;
    let mut turn_snapshot = TurnSnapshot::default();
    // The inputs of each Chat still waiting for their echo after the conversation was last read
    // for them; the same waits are not read for again until they change.
    let mut unsettled_echoes: HashMap<uuid::Uuid, Vec<uuid::Uuid>> = HashMap::new();
    let mut context_warnings = ContextWarnings::default();
    // The catalog's account of each family member whose snapshot this View last applied.
    let mut applied_member_work: HashMap<uuid::Uuid, MemberWork> = HashMap::new();
    let mut running_activity: Option<RunningActivity> = None;
    let mut idle_interruptible_chat = None;
    // A replayed or restored run has no known start; only a live Started event begins this clock.
    let mut observed_run_starts = HashMap::new();
    // The MCP servers each run's request waits for; a run is absent while it does not wait.
    let mut awaited_mcp_servers: HashMap<uuid::Uuid, Vec<String>> = HashMap::new();
    let mut cached_activity_status: Option<(uuid::Uuid, String)> = None;
    let mut menu_outcome: Option<MenuOutcome> = None;
    let mut menu_commands = VecDeque::new();
    let mut paused_inputs = VecDeque::new();
    let mut foreground_shells = HashMap::new();
    let mut user_shell_offsets: HashMap<uuid::Uuid, UserShellOutput> = HashMap::new();
    let mut user_shell_chats = HashMap::new();
    let mut deferred_shell_updates: HashMap<uuid::Uuid, UserShellRecord> = HashMap::new();
    let mut backgrounded_shells = HashSet::new();
    // Shell commands this View sent while a turn worked, listed in the queue until they start.
    let mut queued_shells = HashSet::new();
    let (startup_chat, view_handoff) = match startup.chat {
        StartupChat::Upgrade(chat, handoff) => (*chat, Some(*handoff)),
        chat => (chat, None),
    };
    let handoff_agent_ended = matches!(startup_chat, StartupChat::Ended);
    match startup_chat {
        StartupChat::Ended => {}
        StartupChat::New => {
            if interactive
                && let Err(error) = menus
                    .model_setup(agent, console, startup.model_selected)
                    .await
            {
                console.set_status(&safe_text(&error))?;
            }
        }
        StartupChat::Resumed(snapshot) => {
            learn_agent_colors(console, agent, &snapshot).await?;
            show_opened_chat(console, agent, &snapshot)?;
            selected_chat = Some(snapshot.id);
            displayed_sequences.insert(snapshot.id.into(), snapshot.last_sequence);
            tools.restore_snapshot(&snapshot);
            sync_snapshot(&snapshot, &mut pending, &mut permissions, &mut questions);
        }
        StartupChat::Picker { as_new_chat } => {
            menus.conversations(
                console,
                Workflow::chats(agent, &workdir, None, as_new_chat).await?,
            )?;
        }
        StartupChat::Upgrade(_, _) => return Err("nested View upgrade state".into()),
    }
    let mut restored_view_path = None;
    if let Some(handoff) = view_handoff {
        global.restore_upgrade_navigation(handoff.selection.navigation);
        agent.set_new_agent_directory(global.projection().dispatch_directory);
        overview_active = handoff.selection.overview;
        agent.restore_upgrade_configuration(handoff.selection.configuration)?;
        if overview_active {
            global
                .interpret(crate::agent_console::Intent::Overview)
                .map_err(|error| error.to_string())?;
            global.refresh().await.map_err(|error| error.to_string())?;
            console.set_global_overview(Some(agent_overview(&global.projection())))?;
        }
        console.restore_upgrade(handoff.console)?;
        if selected_chat.is_none() {
            let draft = agent
                .configuration(ConfigTarget::NewDraft)
                .await
                .map_err(|error| error.to_string())?;
            console.set_input_context(
                None,
                &global.projection().dispatch_directory,
                &draft.additional_directories,
            )?;
        }
        restored_view_path = Some(handoff.source_path);
        ui_selected = selected_chat;
    }
    if let Some(chat) = selected_chat {
        let snapshot = agent
            .snapshot(chat.into())
            .await
            .map_err(|error| error.to_string())?;
        menus.warm_model_directory(agent, ConfigTarget::Agent(chat.into()), snapshot.model);
    } else if let Some(model) = agent.launch_spec().model {
        menus.warm_model_directory(agent, ConfigTarget::NewDraft, model);
    }
    agent
        .display_chat(selected_chat.map(Into::into))
        .await
        .map_err(|error| error.to_string())?;
    let mut registration = config_root.as_deref().and_then(|root| {
        let registration = ProcessRegistration::register(
            root,
            selected_chat.map(Into::into),
            &workdir,
            interactive,
        );
        process_registry::count_concurrent_processes(root);
        registration
    });
    if let Some(path) = restored_view_path {
        view_upgrade::complete_handoff();
        let _ = std::fs::remove_file(path);
        console.finish_upgrade_restore();
    }
    if handoff_agent_ended {
        write_warning(console, HANDOFF_AGENT_ENDED)?;
    }
    loop {
        observed_run_starts.retain(|run, _| pending.contains_key(run));
        awaited_mcp_servers.retain(|run, _| pending.contains_key(run));
        suggestible_runs.retain(|run| pending.contains_key(run));
        let dispatch_directory = global.projection().dispatch_directory;
        menus.set_new_agent_directory(dispatch_directory.clone());
        agent.set_new_agent_directory(dispatch_directory);
        if !overview_active && ui_selected != selected_chat {
            agent
                .display_chat(selected_chat.map(Into::into))
                .await
                .map_err(|error| error.to_string())?;
            let visible = match selected_chat {
                Some(chat) => Some(
                    agent
                        .reference(chat.into())
                        .await
                        .map_err(|error| error.to_string())?,
                ),
                None => None,
            };
            global.visible_ready(visible);
            ui_selected = selected_chat;
        }
        let latest_write_error = agent.transcript_write_error();
        if latest_write_error != transcript_write_error {
            transcript_write_error = latest_write_error;
            status_dirty = true;
        }
        let latest_trigger_notices = agent
            .loop_trigger_notices()
            .into_iter()
            .map(|notice| (notice.chat_id, notice.message))
            .collect::<Vec<_>>();
        if latest_trigger_notices != loop_trigger_notices {
            loop_trigger_notices = latest_trigger_notices;
            status_dirty = true;
        }
        if let Some(registration) = registration.as_mut()
            && registration.chat_id() != selected_chat.map(Into::into)
        {
            registration.update_chat(selected_chat.map(Into::into));
        }
        menus.sync_dialog_chat(console)?;
        if logout.is_none()
            && let Some(chat) = provider_logout_chat.take()
        {
            menus.note_provider_login_result(chat, false);
        }
        if logout.is_none()
            && let Some(succeeded) = menus.take_provider_login_result(console)?
        {
            turn_snapshot.forget();
            if let Some(chat) = selected_chat
                && let Err(error) = agent.refresh_configuration(chat.into()).await
            {
                outln!(
                    console,
                    "Cannot refresh Agent configuration: {}",
                    safe_text(&error.to_string())
                );
            }
            if let Err(error) = menus.resume_provider_login(agent, console, succeeded).await {
                outln!(console, "Cannot return to Provider: {}", safe_text(&error));
            }
        }
        if let Some(outcome) = menu_outcome.take() {
            // A conversation chosen from the overview's history joins the list as a row there,
            // as Claude Code's agent view adds it.
            let mut added_from_history = None;
            if history_return_overview {
                if outcome.selection.is_some() {
                    history_return_overview = false;
                    added_from_history = outcome.selection;
                } else if menus.id().is_none() {
                    global
                        .interpret(crate::agent_console::Intent::Overview)
                        .map_err(|error| error.to_string())?;
                    if let Err(error) = global.refresh().await {
                        daemon_unreachable = Some(format!(
                            "{DISCONNECTED_CACHED_VIEW_NOTICE} · {}",
                            safe_text(&error.to_string())
                        ));
                        console.set_connection_notice(daemon_unreachable.clone())?;
                    }
                    console.set_global_overview(Some(agent_overview(&global.projection())))?;
                    if let Err(error) = agent.display_chat(None).await {
                        console.set_connection_notice(Some(format!(
                            "{VIEW_REGISTRATION_UNAVAILABLE_NOTICE} · {}",
                            safe_line(&error.to_string())
                        )))?;
                    }
                    overview_active = true;
                    history_return_overview = false;
                }
            }
            if outcome.provider_logout {
                menus.invalidate_model_discovery();
                provider_logout_chat = Some(selected_chat.map(Into::into));
                if let Some(root) = menus.provider_login_root() {
                    reauthenticate_after_logout = outcome.reauthenticate_after_logout;
                    logout_target = outcome.logout_target;
                    logout = Some(LogoutFlow::start(root, console, logout_target)?);
                } else {
                    outln!(
                        console,
                        "Cannot locate the Agent state directory; set TICKLOOP_CONFIG_DIR."
                    );
                }
            }
            if let Some(method) = outcome.provider_login {
                if let Some(root) = menus.provider_login_root() {
                    match LoginFlow::start(root, console, method) {
                        Ok(flow) => {
                            logins.insert(
                                flow.id(),
                                LoginSession {
                                    flow,
                                    from_provider: true,
                                    succeeded: false,
                                },
                            );
                        }
                        Err(error) => outln!(console, "Cannot log in: {}", safe_text(&error)),
                    }
                } else {
                    outln!(
                        console,
                        "Cannot locate the Agent state directory; set TICKLOOP_CONFIG_DIR."
                    );
                }
            }
            if let Some(command) = outcome.command {
                menu_commands.push_back(command);
            }
            if let Some((chat, request)) = outcome.summary {
                let direction = directed_summaries.start(agent, chat, request);
                selected_chat = Some(chat.into());
                console.set_prompt_enabled(false)?;
                outln!(
                    console,
                    "\nSummarizing conversation {}",
                    directed_summary::direction_phrase(direction)
                );
            }
            if let Some(id) = outcome.permission {
                permissions.remove(&id);
            }
            if let Some(id) = outcome.question {
                questions.remove(&id);
            }
            if let Some(snapshot) = outcome.restored {
                displayed_sequences.insert(snapshot.id.into(), snapshot.last_sequence);
                tools.restore_snapshot(&snapshot);
                sync_snapshot(&snapshot, &mut pending, &mut permissions, &mut questions);
            }
            if let Some(chat) = outcome.selection {
                if selected_chat != Some(chat.into()) {
                    message_groups = MessageGroups::default();
                }
                selected_chat = Some(chat.into());
            }
            if let Some(receipt) = outcome.receipt {
                selected_chat = Some(receipt.chat_id);
                pending.insert(receipt.run_id.into(), receipt);
            }
            if let Some(status) = outcome.status {
                console.set_status(&status)?;
                status_dirty = false;
            } else {
                status_dirty = true;
            }
            if let Some(chat) = added_from_history {
                global.visible_ready(agent.reference(chat).await.ok());
                if let Err(error) = global.refresh().await {
                    daemon_unreachable = Some(format!(
                        "{DISCONNECTED_CACHED_VIEW_NOTICE} · {}",
                        safe_text(&error.to_string())
                    ));
                    console.set_connection_notice(daemon_unreachable.clone())?;
                }
                global
                    .interpret(crate::agent_console::Intent::Overview)
                    .map_err(|error| error.to_string())?;
                console.set_global_overview(Some(agent_overview(&global.projection())))?;
                if let Err(error) = agent.display_chat(None).await {
                    console.set_connection_notice(Some(format!(
                        "{VIEW_REGISTRATION_UNAVAILABLE_NOTICE} · {}",
                        safe_line(&error.to_string())
                    )))?;
                }
                overview_active = true;
                console.set_status(HISTORY_ADDED_TO_OVERVIEW)?;
                status_dirty = false;
            }
        }
        if let Some(chat) = selected_chat {
            let updates = deferred_shell_updates
                .extract_if(|_, record| record.chat_id == chat)
                .map(|(_, record)| record)
                .collect::<Vec<_>>();
            for record in updates {
                let observation = observe_user_shell_record(
                    &mut foreground_shells,
                    &mut backgrounded_shells,
                    &record,
                );
                show_started_shell(console, &mut queued_shells, &record)?;
                if observation.render_in_chat {
                    render_user_shell_update(agent, console, &record, &mut user_shell_offsets)
                        .await?;
                }
                if user_shell_is_terminal(&record) {
                    user_shell_chats.remove(&record.id);
                    user_shell_offsets.remove(&record.id);
                }
            }
        }
        if interactive && status_dirty {
            running_activity = None;
            idle_interruptible_chat = None;
            cached_activity_status = None;
            console.set_activity_interruptible(false)?;
            console.set_idle_loop_interruptible(false)?;
            if overview_active {
                completed_task_retention.observe(None, std::time::Instant::now());
                // The overview heading and dispatch line carry counts and directory; the status
                // row keeps the latest action feedback until another action replaces it.
                console.set_activity(None)?;
            } else if let Some(chat) = selected_chat {
                if background_notice
                    .as_ref()
                    .is_some_and(|(notice_chat, _)| *notice_chat == chat)
                {
                    background_notice = None;
                }
                // An input waits for its echo after a turn that read the conversation for it, and
                // that turn's conversation settles it. Only waits that read could not settle, as
                // when the conversation was unreachable at their event, are read for here.
                let mut awaiting = console.awaiting_input_echoes(chat.into())?;
                awaiting.sort();
                let reconciling = match turn_snapshot.conversation_of(chat) {
                    Some(conversation) => Some(conversation),
                    None if !awaiting.is_empty()
                        && unsettled_echoes.get(&chat.uuid()) != Some(&awaiting) =>
                    {
                        turn_snapshot.read(agent, chat).await.ok()
                    }
                    None => None,
                };
                let status = turn_snapshot
                    .status(agent, chat)
                    .await
                    .map_err(|error| error.to_string())?;
                let task_snapshot = task_center
                    .snapshot(chat.into())
                    .await
                    .map_err(|error| error.to_string())?;
                let subscriptions = agent.tick_subscriptions(chat.into()).await.ok();
                // Everything is read before the first change shows, and the changes reach the
                // terminal as one frame; keys typed during the reads still draw at once.
                let _frame_hold = console.hold_frames()?;
                console.set_input_context(
                    Some(chat.into()),
                    shown_folder(status.folder.as_deref()),
                    &status.additional_directories,
                )?;
                console.set_visible_delegation(status.delegation.is_some())?;
                console.set_foreground_shell(foreground_shells.contains_key(&chat.uuid()))?;
                console.set_queued_inputs(chat.into(), status.queued_inputs())?;
                console.set_chat_images(
                    chat.into(),
                    status.accepts_images(),
                    status.highest_pasted_image_id,
                )?;
                if let Some(conversation) = &reconciling {
                    reconcile_consumed_input_echoes(console, conversation)?;
                    // A cached conversation, read while the daemon is unreachable, settles
                    // nothing for good; the waits are read for again once it answers.
                    if agent.disconnected().is_none() {
                        let mut unsettled = console.awaiting_input_echoes(chat.into())?;
                        unsettled.sort();
                        unsettled_echoes.insert(chat.into(), unsettled);
                    }
                }
                console.set_foreground_commands(
                    &task_snapshot
                        .tasks
                        .iter()
                        .filter(|task| {
                            task.chat_id == chat
                                && task.origin == ShellTaskOrigin::Agent
                                && task.capabilities.can_background
                        })
                        .map(|task| (task.id, task.call_id.clone()))
                        .collect::<Vec<_>>(),
                )?;
                console.refresh_monitor(monitor::MonitorSources {
                    chat: status.clone(),
                    shells: task_snapshot,
                    subscriptions,
                })?;
                console.set_provider_failures(chat.into(), &status.provider_failures)?;
                menus.note_provider_failures(chat.into(), &status.provider_failures);
                menus.offer_continue(console, &status)?;
                console.set_provider_failure(
                    status
                        .provider_failures
                        .get(&status.model.provider_id)
                        .map(|failure| failure.summary()),
                )?;
                let visible_task_board = if !status.task_tools_enabled {
                    None
                } else if status.delegation.is_some() {
                    status.task_board.clone()
                } else {
                    status.task_boards.get(&status.chat_loop.id).cloned()
                };
                if let Some(cleanup) = completed_task_retention.observe(
                    if eof {
                        None
                    } else {
                        visible_task_board.as_ref()
                    },
                    std::time::Instant::now(),
                ) {
                    let agent = agent.clone();
                    completed_task_cleanups.spawn(async move {
                        agent.clear_completed_tasks(chat.into(), cleanup).await
                    });
                }
                console.update_status_area(
                    status.policy.mode.to_string(),
                    if menus.model_setup_applies_to(Some(chat.into())) {
                        MODEL_SETUP_STATUS.into()
                    } else {
                        safe_text(settings::model_name(&status.model))
                    },
                    context_usage(&status),
                    visible_task_board,
                )?;
                if let Some(notice) = context_warnings.crossing_notice(&status) {
                    write_warning(console, &notice)?;
                }
                let running_receipt = pending
                    .values()
                    .find(|receipt| {
                        receipt.chat_id == chat
                            && status.chat_loop.run_id == Some(receipt.run_id)
                            && status.chat_loop.state
                                == tickloop_harness::protocol::LoopState::Running
                    })
                    .or_else(|| pending.values().find(|receipt| receipt.chat_id == chat));
                let running_loop = running_receipt.map(|receipt| receipt.loop_id);
                let run_is_working = running_loop.is_some();
                let mut activity: Option<Vec<(String, Option<Role>)>> = None;
                let connection = execution_connection_notice(
                    &global.projection().rows,
                    chat,
                    agent.disconnected().is_some(),
                    status.chat_loop.run_id.is_some()
                        && status.chat_loop.state == tickloop_harness::protocol::LoopState::Idle
                        && !pending.values().any(|receipt| receipt.chat_id == chat)
                        && agent.unknown_input(chat.into()).is_none()
                        && status.queued_inputs().is_empty()
                        && status.user_shells.iter().all(|record| {
                            matches!(
                                record.state,
                                UserShellState::Completed | UserShellState::Failed
                            )
                        }),
                    daemon_unreachable.is_some(),
                );
                if connection.is_some() {
                    console.set_execution_connection_notice(
                        connection,
                        agent.disconnected().or_else(|| daemon_unreachable.clone()),
                    )?;
                } else if let Some(notice) = daemon_unreachable.clone() {
                    console.set_connection_notice(Some(notice))?;
                } else {
                    console.set_execution_connection_notice(connection, agent.disconnected())?;
                }
                let loop_interruptible = daemon_unreachable.is_none()
                    && agent.disconnected().is_none()
                    && global.projection().rows.iter().any(|row| {
                        row.chat_id == chat
                            && row.availability == ExecutionAvailability::Live
                            && row
                                .capabilities
                                .iter()
                                .any(|capability| capability == "loop")
                    });
                idle_interruptible_chat = (loop_interruptible
                    && !run_is_working
                    && !directed_summaries.contains(chat)
                    && (status.goal.as_ref().is_some_and(|goal| {
                        matches!(goal.status, tickloop_harness::protocol::GoalStatus::Active)
                    }) || status.scheduled_tasks.iter().any(|task| {
                        matches!(
                            task.kind,
                            tickloop_harness::protocol::ScheduledTaskKind::SelfPaced { .. }
                        )
                    })))
                .then_some(chat);
                console.set_idle_loop_interruptible(idle_interruptible_chat.is_some())?;
                let state = if connection
                    .is_some_and(|notice| notice != console::ExecutionConnectionNotice::CachedView)
                {
                    String::new()
                } else if logout.is_some() {
                    "Removing Codex login · esc cancels".to_owned()
                } else if !status.permissions.is_empty() {
                    "Waiting for approval".to_owned()
                } else if !status.questions.is_empty() {
                    "Waiting for your answer".to_owned()
                } else if compaction_chats
                    .values()
                    .any(|(candidate, _)| *candidate == chat)
                {
                    "Compacting".to_owned()
                } else if summary_recovery_required.contains(&chat) {
                    "Restart required".to_owned()
                } else if directed_summaries.contains(chat) || run_is_working {
                    let working = RunningActivity {
                        chat_id: chat,
                        label: running_receipt
                            .and_then(|receipt| awaited_mcp_servers.get(&receipt.run_id.uuid()))
                            .map_or_else(
                                || WORKING_ACTIVITY.to_owned(),
                                |servers| {
                                    format!(
                                        "{AWAITED_MCP_SERVERS_ACTIVITY}: {}",
                                        safe_text(&servers.join(", "))
                                    )
                                },
                            ),
                        started_at: running_receipt.and_then(|receipt| {
                            observed_run_starts.get(&receipt.run_id.uuid()).copied()
                        }),
                        interruptible: loop_interruptible,
                    };
                    let preview = pending.iter().find_map(|(run_id, receipt)| {
                        (receipt.chat_id == chat
                            && thinking_runs.contains(&tickloop_ids::Id::from(*run_id)))
                        .then(|| thinking_previews.get(run_id))
                        .flatten()
                    });
                    activity = Some(working.spans(console, preview, &working_shimmer)?);
                    console.set_activity_interruptible(working.interruptible)?;
                    running_activity = Some(working);
                    "Working".to_owned()
                } else if foreground_shells.contains_key(&chat.uuid()) {
                    "Shell running".to_owned()
                } else {
                    "Ready".to_owned()
                };
                let status = format!(
                    "{}{}{}{}{}",
                    if state == "Ready" || state.contains("Working") || state == "Shell running" {
                        ""
                    } else {
                        state.as_str()
                    },
                    if status.pending_model.is_some() {
                        " · model change pending"
                    } else {
                        ""
                    },
                    if tools.is_detailed() {
                        " · details"
                    } else {
                        ""
                    },
                    transcript_write_error
                        .as_ref()
                        .map_or_else(String::new, |error| format!(" · Not saved: {error}")),
                    loop_trigger_notices
                        .iter()
                        .filter(|(notice_chat, _)| *notice_chat == chat)
                        .map(|(_, message)| format!(" · Trigger: {message}"))
                        .collect::<String>(),
                );
                let displayed_background = background_wait(selected_chat, &permissions, &questions)
                    .map(|wait| {
                        let message = match wait.kind {
                            BackgroundWaitKind::Approval => "waiting for approval",
                            BackgroundWaitKind::Answer => "waiting for your answer",
                        };
                        (
                            wait.chat_id,
                            BackgroundRunNotice {
                                message: message.into(),
                                compact_message: None,
                                necessary: true,
                            },
                            Role::Permission,
                        )
                    })
                    .or_else(|| {
                        background_notice
                            .clone()
                            .filter(|(chat, notice)| {
                                notice.message != REPLY_COMPLETED_NOTICE
                                    || !agent.cached_is_delegated((*chat).into())
                            })
                            .map(|(chat, notice)| {
                                let role = if notice.necessary {
                                    Role::Warning
                                } else {
                                    Role::MutedText
                                };
                                (chat, notice, role)
                            })
                    });
                console.set_background_status_notice(
                    displayed_background
                        .map(|(chat, notice, role)| {
                            console.background_status_notice(
                                agent.cached_title(chat.into()),
                                notice.message,
                                notice.compact_message,
                                notice.necessary,
                                role,
                            )
                        })
                        .transpose()?,
                )?;
                cached_activity_status =
                    Some((chat.into(), safe_text(status.trim_start_matches(" · "))));
                console.set_activity_and_status(
                    activity.as_deref(),
                    &safe_text(status.trim_start_matches(" · ")),
                )?;
            } else {
                completed_task_retention.observe(None, std::time::Instant::now());
                let draft = agent.configuration(ConfigTarget::NewDraft).await;
                let _frame_hold = console.hold_frames()?;
                console.set_activity(None)?;
                console.set_background_status_notice(None)?;
                console.set_footer_items(Vec::new())?;
                console.set_provider_failure(None)?;
                if let Ok(draft) = draft {
                    console.set_input_context(
                        None,
                        &global.projection().dispatch_directory,
                        &draft.additional_directories,
                    )?;
                    console.update_status_area(
                        draft.execution_mode.to_string(),
                        if menus.model_setup_pending() {
                            MODEL_SETUP_STATUS.into()
                        } else {
                            safe_text(settings::model_name(&draft.model))
                        },
                        (!menus.model_setup_pending())
                            .then(|| initial_context_usage(&draft.model))
                            .flatten(),
                        None,
                    )?;
                }
            }
            status_dirty = false;
            activity_dirty = false;
            thinking_frame_due = false;
            monitor_refresh_due = false;
        }
        turn_snapshot.forget();
        if interactive && monitor_refresh_due {
            monitor_refresh_due = false;
            if !overview_active && let Some(chat) = selected_chat {
                // Subscriptions and Shell tasks move on while the Chat itself stays as it was,
                // whose own changes come with its events; only they are read again.
                let shells = task_center
                    .snapshot(chat.into())
                    .await
                    .map_err(|error| error.to_string())?;
                console.refresh_monitor_ticks(
                    shells,
                    agent.tick_subscriptions(chat.into()).await.ok(),
                )?;
            }
        }
        if interactive && activity_dirty {
            if !overview_active
                && let Some(chat) = selected_chat
                && let Some((cached_chat, status)) = &cached_activity_status
                && *cached_chat == chat
            {
                #[cfg(test)]
                agent.record_recovery_fact(recovery_tests::RecoveryFact::ActivityFrameBegin {
                    chat: chat.into(),
                });
                let activity = running_activity
                    .as_ref()
                    .filter(|activity| activity.chat_id == chat)
                    .map(|activity| {
                        let preview = pending.iter().find_map(|(run_id, receipt)| {
                            (receipt.chat_id == chat)
                                .then(|| thinking_previews.get(run_id))
                                .flatten()
                        });
                        activity.spans(console, preview, &working_shimmer)
                    })
                    .transpose()?;
                console.set_activity_and_status(activity.as_deref(), status)?;
                #[cfg(test)]
                agent.record_recovery_fact(recovery_tests::RecoveryFact::ActivityFrameEnd {
                    chat: chat.into(),
                    painted: activity.is_some(),
                });
            }
            activity_dirty = false;
        }
        menus.sync_dialog_chat(console)?;
        if !overview_active {
            menus
                .refresh_family_permissions(
                    agent,
                    console,
                    &visible_family,
                    &permissions,
                    &questions,
                )
                .await?;
        }
        if eof
            && let Some(message) =
                input_closed_while_waiting(selected_chat, &permissions, &questions)
        {
            return Err(message);
        }
        // Runs of other family members the View follows are background work, as are the
        // completions they leave; a piped View waits for those as Claude Code's print mode does.
        let is_own_work_done = pending
            .values()
            .all(|receipt| !interactive && Some(receipt.chat_id) != selected_chat)
            && compactions.is_empty()
            && directed_summaries.is_empty()
            && deferred_events.is_empty()
            && !reply_lines.values().any(ReplyLines::has_complete_line)
            && logins.is_empty()
            && logout.is_none()
            && paused_inputs.is_empty()
            && menu_commands.is_empty();
        if !is_own_work_done {
            background_work.note_busy();
        } else if eof
            && (interactive || !background_work.keeps_open(agent, selected_chat).await)
            && pending.is_empty()
        {
            return view_exit(interactive, failed);
        }
        let summary_input_locked =
            !directed_summaries.is_empty() || !summary_recovery_required.is_empty();
        let visual_pacing_pending = interactive
            && !overview_active
            && reply_lines.values().any(|reply| {
                Some(reply.chat_id) == selected_chat.map(Into::into) && reply.has_complete_line()
            });
        let prefers_reduced_motion =
            interactive && console.display_preferences()?.prefers_reduced_motion;
        if interactive
            && let Some(request) = console.take_artifact_load()?
            && let Some(root) = config_root.clone()
        {
            artifact_loads.spawn(async move {
                let result = artifact_ui::load_page(&root, &request).await;
                (request, result)
            });
        }
        if let Some(target) = ready_runtime.as_ref()
            && !agent.is_handing_off()
            && pending.is_empty()
            && compactions.is_empty()
            && directed_summaries.is_empty()
            && deferred_events.is_empty()
            && menu_commands.is_empty()
            && paused_inputs.is_empty()
            && foreground_shells.is_empty()
            && logins.is_empty()
            && logout.is_none()
            && !reply_lines.values().any(ReplyLines::has_complete_line)
            && let Some(navigation) = global.prepare_upgrade_navigation()
            && let Some(configuration) = agent.upgrade_configuration()?
        {
            // The new View reconnects the shown Chat live and never loads its history instead;
            // after the daemon stopped (`make dev` stops it) this View keeps the Chat it caches.
            let blocked_by_shown_chat = match selected_chat {
                Some(chat) => match agent.runs_chat(chat.into()).await {
                    Ok(true) => None,
                    Ok(false) => Some(VIEW_UPDATE_AFTER_ENDED_AGENT.to_owned()),
                    Err(error) => Some(format!("View update · {error}")),
                },
                None => None,
            };
            if let Some(reason) = blocked_by_shown_chat {
                view_activation_notice =
                    Some(tickloop_code_updater::UpdateNotice::Deferred { reason });
                ready_runtime = None;
            } else {
                let selection = view_upgrade::ViewSelection {
                    navigation,
                    chat: selected_chat.map(Into::into),
                    overview: overview_active,
                    configuration,
                };
                // exec replaces this process without running destructors. Release the old
                // pane report first; the replacement View will publish its current state.
                if let Some(reporter) = herdr_reporter.as_mut() {
                    reporter.shutdown().await;
                }
                if let Err(error) = view_upgrade::try_reexec(
                    console,
                    &input,
                    &target.executable,
                    startup.launch_arguments.clone(),
                    selection,
                )
                .await
                {
                    view_activation_notice = Some(tickloop_code_updater::UpdateNotice::Deferred {
                        reason: format!("View update · {error}"),
                    });
                    ready_runtime = None;
                }
                // A postponed upgrade also returns without replacing the process.
                *herdr_reporter = herdr_agent::Reporter::from_environment(interactive);
            }
        }
        if interactive {
            let execution = selected_chat
                .filter(|_| !overview_active)
                .and_then(|chat| agent.cached_reference(chat.into()));
            if runtime_selection != execution {
                runtime_selection = execution;
                if execution.is_some() {
                    activation_refresh.reset_immediately();
                }
            }
            let worker =
                execution.and_then(|execution| runtime_activation.as_ref()?.worker(execution));
            if let Some(chat) = selected_chat.filter(|_| !overview_active) {
                let pending = worker.is_some_and(|worker| worker.upgrading);
                let presentation = (
                    chat,
                    worker
                        .and_then(|worker| worker.build_id.clone())
                        .filter(|build| {
                            pending
                                || tickloop_code_access::build_identity()
                                    .is_none_or(|view| view != build.as_str())
                        }),
                    pending,
                );
                if runtime_presentation.as_ref() != Some(&presentation) {
                    console.set_agent_runtime(
                        chat.into(),
                        presentation.1.clone(),
                        presentation.2,
                    )?;
                    runtime_presentation = Some(presentation);
                }
            }
            let notice = view_activation_notice
                .clone()
                .or_else(|| {
                    runtime_activation
                        .as_ref()
                        .and_then(|activation| activation.notice_for(execution))
                })
                .or_else(|| installation_notice.clone());
            console.set_update_notice(notice)?;
        }
        if let Some(reporter) = herdr_reporter.as_ref() {
            reporter.update(herdr_view_state(
                selected_chat,
                overview_active,
                &visible_family,
                pending
                    .values()
                    .map(|receipt| receipt.chat_id)
                    .chain(compaction_chats.values().map(|(chat, _)| *chat))
                    .chain(directed_summaries.active_chats()),
                permissions
                    .values()
                    .map(|request| request.chat_id)
                    .chain(questions.values().map(|request| request.chat_id)),
            ));
        }
        let discovery_active = menus.discovery_active();
        let search_test_active = menus.search_test_active();
        let active_login = login::active_login_id(&logins, console)?;
        let login_active = active_login.is_some();
        let login_updates_pending = logins
            .values()
            .any(|session| session.flow.updates_pending());
        let logout_active = logout.is_some();
        #[cfg(test)]
        let global_refresh_enabled = agent.recovery_refresh_enabled();
        #[cfg(not(test))]
        let global_refresh_enabled = true;
        tokio::select! {
            _ = background_poll.tick(), if background_work.is_waiting() => {},
            _ = activation_refresh.tick(), if interactive && activation_checks.is_empty() => {
                let initial = std::mem::take(&mut initial_activation);
                activation_checks.spawn(runtime_upgrade::check(agent.clone(), initial));
            },
            checked = activation_checks.join_next(), if !activation_checks.is_empty() => {
                ready_runtime = None;
                match checked {
                Some(Ok(Ok(mut activation))) => {
                    if let Some(target) = activation.target.take() {
                        if runtime_upgrade::is_current_executable(&target.executable) {
                            view_activation_notice = None;
                        } else {
                            ready_runtime = Some(target);
                        }
                    }
                    runtime_activation = Some(activation);
                },
                Some(Ok(Err(error))) => {
                    if let Some(activation) = runtime_activation.as_mut() {
                        activation.error = Some(error);
                    }
                },
                Some(Err(error)) => {
                    if let Some(activation) = runtime_activation.as_mut() {
                        activation.error = Some(error.to_string());
                    }
                },
                None => {},
                }
            },
            result = completed_task_cleanups.join_next(), if !completed_task_cleanups.is_empty() => {
                match result {
                    Some(Ok(Ok(()))) => {},
                    Some(Ok(Err(error))) => {
                        console.set_status(&format!("Completed task cleanup unavailable · {}", safe_line(&error.to_string())))?;
                    },
                    Some(Err(_)) => {
                        console.set_status("Completed task cleanup result unavailable; task records will be refreshed")?;
                    },
                    None => {},
                }
                status_dirty = true;
            },
            _ = artifact_refresh.tick(), if console.artifacts_picker_is_open()? => {},
            result = artifact_loads.join_next(), if !artifact_loads.is_empty() => {
                if let Some(Ok((request, result))) = result { console.artifact_page_loaded(&request, result)?; }
            },
            result = tick_package_tasks.join_next(), if !tick_package_tasks.is_empty() => {
                if let Some((panel, result)) = result && let Some(root) = config_root.as_deref() {
                    ticks_work::finish(console, root, panel, result, &mut tick_package_tasks)?;
                }
            },
            result = mcp_tasks.join_next(), if !mcp_tasks.is_empty() => {
                if let Some((panel, done)) = result && let Some(root) = config_root.as_deref() {
                    mcp_work::finish(console, root, agent, panel, done, &mut mcp_tasks)?;
                }
            },
            _ = mcp_refresh.tick(), if console.mcp_panel_awaiting_connections()?.is_some() => {
                if let Some(panel) = console.mcp_panel_awaiting_connections()? && let Some(root) = config_root.as_deref() {
                    mcp_work::refresh(root, agent, panel, &mut mcp_tasks);
                }
            },
            result = suggestions.join_next(), if !suggestions.is_empty() => {
                if let Some(Ok((request, mut result))) = result {
                    let suppression = if selected_chat != Some(request.chat.into()) || overview_active {
                        Some("discarded: Agent View changed")
                    } else {
                        match agent.status(request.chat).await {
                            Ok(status) => prompt_suggestion::suppression(&status)
                                .or(console.prompt_suggestion_suppression()?),
                            Err(_) => Some("discarded: Agent unavailable"),
                        }
                    };
                    if result.text.is_some() && let Some(reason) = suppression {
                        result.text = None;
                        result.diagnostic = reason;
                    } else if request.cancel.is_cancelled() && result.text.is_some() {
                        result.text = None;
                        result.diagnostic = "cancelled";
                    }
                    if let Some((id, diagnostic)) = suggestion_diagnostics.get_mut(&request.chat)
                        && *id == request.id {
                        *diagnostic = result.diagnostic;
                    }
                    console.finish_prompt_suggestion(&request, result.text)?;
                }
            },
            result = menus.next_provider_update(), if discovery_active || search_test_active => {
                match result {
                    menus::ProviderUpdate::Search(result) => menus.finish_search_test(console, result)?,
                    menus::ProviderUpdate::Models(result) => menus.finish_discovery(agent, console, result).await?,
                }
                status_dirty = true;
            },
            (login_id, update) = login::next_login_update(&mut logins), if login_updates_pending => {
                match update {
                    Some(LoginUpdate::Device { address, code }) => logins[&login_id].flow.show_device(console, &address, &code)?,
                    Some(LoginUpdate::Browser { address, callback }) => logins.get_mut(&login_id).unwrap().flow.show_browser(console, address, callback)?,
                    Some(LoginUpdate::Finished(result)) => {
                        let session = logins.get_mut(&login_id).unwrap();
                        let provider_label = session.flow.label();
                        let is_artifact_login = session.flow.is_artifact();
                        let login_chat = session.flow.chat();
                        let from_provider = session.from_provider;
                        session.succeeded = result.is_ok();
                        if interactive && from_provider {
                            finish_provider_login(agent, console, session, &result).await?;
                        } else {
                            close_login_session(logins.remove(&login_id).unwrap(), console, &mut menus, false)?;
                        }
                        match result {
                            Ok(account) => {
                                if is_artifact_login
                                    && let Some(root) = config_root.as_deref()
                                    && let Err(error) = artifact_ui::open(agent, console, root, login_chat, interactive).await {
                                    outln!(console, "Cannot open artifacts: {} · use /artifacts to retry", safe_text(&error));
                                }
                                if !interactive || !from_provider { outln!(console, "{} login saved · {}", provider_label, safe_text(&account)); }
                            }
                            Err(error) if !interactive || !from_provider => outln!(console, "Cannot log in: {}", safe_text(&error)),
                            Err(_) => {},
                        }
                    }
                    None => {
                        let session = logins.get_mut(&login_id).unwrap();
                        if interactive && session.from_provider {
                            login::complete_provider_login(&mut session.flow, console, &Err("Sign-in ended before completion; return to Provider to retry.".into()))?;
                        } else {
                            close_login_session(logins.remove(&login_id).unwrap(), console, &mut menus, false)?;
                            outln!(console, "Sign-in ended before completion; use /provider to retry.");
                        }
                    }
                }
                status_dirty = true;
            },
            result = async { logout.as_mut().unwrap().next().await }, if logout_active => {
                logout.take();
                match result {
                    Ok(()) => {
                        outln!(console, "{} login removed locally.", logout_target.label());
                        if reauthenticate_after_logout && let Some(root) = config_root.clone() {
                            match LoginFlow::start(root, console, logout_target.login_method()) {
                                Ok(flow) => {
                                    logins.insert(flow.id(), LoginSession { flow, from_provider: true, succeeded: false });
                                    provider_logout_chat = None;
                                },
                                Err(error) => outln!(console, "Cannot log in: {}", safe_text(&error)),
                            }
                        }
                    }
                    Err(tickloop_harness::model::auth::AuthError::Cancelled) => outln!(console, "Logout cancelled; saved login remains."),
                    Err(error) => outln!(console, "Cannot log out: {}", safe_text(&error.to_string())),
                }
                reauthenticate_after_logout = false;
                status_dirty = true;
            },
            line = async {
                if !login_active && !logout_active && !summary_input_locked && let Some(event)=paused_inputs.pop_front() {Some(event)}
                else if !login_active && !logout_active && let Some(command)=menu_commands.pop_front() {Some(InputEvent::Line(command))}
                else {input.recv().await}
            }, if !eof || (!logout_active && (!paused_inputs.is_empty() || !menu_commands.is_empty())) => {
                // Opening the overview, navigating, restoring or any other View action may show
                // the conversation again, so a merged-input block only spans uninterrupted events.
                merged_echoes.close_all();
                if !matches!(&line,Some(InputEvent::Line(input)|InputEvent::Prompt {family_target: None, text:input,..}) if !console.is_command_input(input)?) {
                    merged_echoes.next_submission_block();
                }
                let line = match line {
                    Some(InputEvent::TickAction { chat, action }) if selected_chat == Some(chat.into()) && !overview_active && !login_active => {
                        let context = tick_actions::TickActionContext { agent, console, root:config_root.as_deref(), chat };
                        if let Err(error) = tick_actions::execute(action,context).await { console.report_monitor_error(chat, &error)?; }
                        status_dirty=true;
                        continue;
                    }
                    Some(InputEvent::TickAction { .. }) => continue,
                    other => other,
                };
                let (_delayed_guard, line) = match line {
                    Some(InputEvent::DelayedPrompt { id }) => {
                        if overview_active || login_active || logout_active || summary_input_locked {
                            console.repark_delayed_submission(id)?;
                            continue;
                        }
                        match console.begin_delayed_submission(id, selected_chat.map(Into::into))? {
                            Some((event, guard)) => (Some(guard), Some(event)),
                            None => continue,
                        }
                    },
                    other => (None, other),
                };
                match line {
                None | Some(InputEvent::Eof) => {
                    if interactive { return Ok(()); }
                    eof = true;
                },
                Some(InputEvent::DelayedPrompt { .. }) => unreachable!("delayed event normalized above"),
                Some(InputEvent::Error(error)) => return Err(safe_text(&error)),
                Some(InputEvent::OpenOverview) => {
                    menus.dismiss(console)?;
                    // Only a registered conversation is something to return to; a new draft has none.
                    let source=match selected_chat { Some(chat)=>agent.reference(chat.into()).await.ok(), None=>None };
                    global.visible_ready(source);
                    // An unreachable daemon leaves the cached catalog on screen with a stale label;
                    // the View, its drafts and its reading positions stay alive.
                    if let Err(error)=global.refresh().await {
                        daemon_unreachable=Some(format!("{DISCONNECTED_CACHED_VIEW_NOTICE} · {}",safe_text(&error.to_string())));
                        console.set_connection_notice(daemon_unreachable.clone())?;
                    }
                    global.interpret(crate::agent_console::Intent::Overview).map_err(|error|error.to_string())?;
                    let projection=global.projection();
                    console.set_status("")?;
                    console.set_global_overview(Some(agent_overview(&projection)))?;
                    if let Err(error)=agent.display_chat(None).await {
                        console.set_connection_notice(Some(format!("{VIEW_REGISTRATION_UNAVAILABLE_NOTICE} · {}",safe_line(&error.to_string()))))?;
                    }
                    overview_active = true;
                    #[cfg(test)]
                    agent.record_recovery_fact(recovery_tests::RecoveryFact::OverviewOpened);
                    status_dirty = true;
                },
                Some(InputEvent::CloseOverview) => {
                    global.interpret(crate::agent_console::Intent::Back).map_err(|error|error.to_string())?;
                    console.set_global_overview(None)?;
                    overview_active = false;
                    if let Err(error)=agent.display_chat(selected_chat.map(Into::into)).await {
                        console.set_connection_notice(Some(format!("{VIEW_REGISTRATION_UNAVAILABLE_NOTICE} · {}",safe_line(&error.to_string()))))?;
                    }
                    if let Some(chat)=selected_chat {
                        match restore_view(console,agent,&mut tools,chat,displayed_sequences.get(&chat.uuid()).copied().unwrap_or(0),&mut reply_lines,&mut message_groups).await {
                            Ok(observation)=>{
                                displayed_sequences.insert(chat.into(),observation.snapshot.last_sequence);
                                sync_snapshot(&observation.snapshot,&mut pending,&mut permissions,&mut questions);
                            },
                            Err(error)=>console.set_status(&format!("Showing cached conversation · {}",safe_line(&error)))?,
                        }
                    }
                    status_dirty = true;
                },
                Some(InputEvent::DismissAgent(agent_id)) => {
                    let target=global.projection().rows.iter().find(|row|row.execution.agent_id==agent_id).map(|row|row.execution);
                    if let Some(target)=target && global.dismiss(target) && let Some(visible)=global.projection().visible {
                        let family=global.family(visible);
                        console.set_agent_family(if family.len()>1 {agent_rows(&family,false)}else{Vec::new()},Some(visible.agent_id))?;
                    }
                },
                Some(InputEvent::ResumeEndedAgent(chat)) => {
                    match agent.resume_chat(chat).await {
                        Ok(snapshot)=>{
                            console.set_global_overview(None)?;global.interpret(crate::agent_console::Intent::Back).map_err(|error|error.to_string())?;
                            overview_active=false;learn_agent_colors(console,agent,&snapshot).await?;show_opened_chat(console,agent,&snapshot)?;
                            selected_chat=Some(snapshot.id);message_groups=MessageGroups::default();
                            displayed_sequences.insert(snapshot.id.into(),snapshot.last_sequence);
                            tools.restore_snapshot(&snapshot);
                            sync_snapshot(&snapshot,&mut pending,&mut permissions,&mut questions);
                        },
                        Err(error)=>console.set_status(&format!("Cannot resume the Agent · {}",safe_line(&error.to_string())))?,
                    }
                    status_dirty = true;
                },
                Some(InputEvent::StopAgent(agent_id)) => {
                    let projection=global.projection();
                    let Some(row)=projection.rows.iter().find(|row|row.execution.agent_id==agent_id).cloned() else {
                        console.set_status("This Agent is no longer running")?;status_dirty=true;continue;
                    };
                    match global.stop_agent(row.execution).await {
                        Ok(())=>{
                            // The stopped Agent is no longer one this View can return to.
                            if selected_chat.map(ChatId::uuid)==Some(row.chat_id) {
                                selected_chat=None;global.visible_ready(None);console.clear_visible_agent()?;
                            }
                            if let Err(error)=global.refresh().await {
                                daemon_unreachable=Some(format!("{DISCONNECTED_CACHED_VIEW_NOTICE} · {}",safe_text(&error.to_string())));
                                console.set_connection_notice(daemon_unreachable.clone())?;
                            }
                            console.set_global_overview(Some(agent_overview(&global.projection())))?;
                            console.set_status(&format!("Stopped {} · enter resumes it",safe_line(&row.title)))?;
                        },
                        Err(error)=>console.set_status(&format!("Cannot stop the Agent · {}",safe_line(&error.to_string())))?,
                    }
                    status_dirty = true;
                },
                Some(InputEvent::PeekOpened(agent_id)) => {
                    let row=global.projection().rows.into_iter().find(|row|row.execution.agent_id==agent_id);
                    let request=match row {
                        Some(row) if crate::agent_console::AgentState::of(&row)==crate::agent_console::AgentState::NeedsInput => {
                            agent.snapshot(row.chat_id).await.ok().and_then(|snapshot|peek_request(&snapshot))
                        },
                        _ => None,
                    };
                    console.set_peek_request(agent_id,request)?;
                },
                Some(InputEvent::PeekReply { agent_id, text }) => {
                    let sent=send_peek_reply(agent,&global.projection(),agent_id,text).await;
                    if let Ok((_,Some(loaded)))=&sent {
                        console.follow_peek(agent_id,*loaded)?;
                    }
                    if let Err(error)=global.refresh().await {
                        daemon_unreachable=Some(format!("{DISCONNECTED_CACHED_VIEW_NOTICE} · {}",safe_text(&error.to_string())));
                        console.set_connection_notice(daemon_unreachable.clone())?;
                    }
                    console.set_global_overview(Some(agent_overview(&global.projection())))?;
                    if let Some(peeked)=console.peeked_agent()? {
                        console.select_overview_agent(peeked)?;
                    }
                    match sent {
                        Ok((status,_))=>console.set_status(&status)?,
                        Err(error)=>console.set_status(&format!("Cannot send the reply · {}",safe_line(&error)))?,
                    }
                    status_dirty = true;
                },
                Some(InputEvent::PeekAnswer { agent_id, request_id, option }) => {
                    match answer_peek(agent,&global.projection(),agent_id,request_id,option).await {
                        Ok(())=>console.set_status("Answered")?,
                        Err(error)=>console.set_status(&format!("Cannot answer · {}",safe_line(&error.to_string())))?,
                    }
                    if let Err(error)=global.refresh().await {
                        daemon_unreachable=Some(format!("{DISCONNECTED_CACHED_VIEW_NOTICE} · {}",safe_text(&error.to_string())));
                        console.set_connection_notice(daemon_unreachable.clone())?;
                    }
                    console.set_global_overview(Some(agent_overview(&global.projection())))?;
                    paused_inputs.push_back(InputEvent::PeekOpened(agent_id));
                    status_dirty = true;
                },
                Some(InputEvent::ForgetEndedAgent(chat)) => {
                    let title=global.projection().ended.iter().find(|ended|ended.record.chat_id==chat).map(|ended|safe_line(&ended.title)).unwrap_or_else(||"the Agent".into());
                    match global.forget_ended(chat).await {
                        Ok(())=>{
                            console.set_global_overview(Some(agent_overview(&global.projection())))?;
                            console.set_status(&format!("Removed {title} from the list · its saved history stays"))?;
                        },
                        Err(error)=>console.set_status(&format!("Cannot remove the Agent · {}",safe_line(&error.to_string())))?,
                    }
                    status_dirty = true;
                },
                Some(InputEvent::OpenAgent(agent_id)) => {
                    if let Err(error)=global.refresh().await {
                        daemon_unreachable=Some(format!("{DISCONNECTED_CACHED_VIEW_NOTICE} · {}",safe_text(&error.to_string())));
                        console.set_connection_notice(daemon_unreachable.clone())?;
                        console.set_status("Cannot open the Agent while its daemon is unreachable; the overview shows cached facts")?;
                        status_dirty = true;
                        continue;
                    }
                    let target=global.projection().rows.iter().find(|row|row.execution.agent_id==agent_id).map(|row|row.execution);
                    if let Some(target)=target {
                        let effect=global.interpret(crate::agent_console::Intent::Open(target)).map_err(|error|error.to_string())?.ok_or("missing navigation effect")?;
                        let opened=match navigate_agent(&mut global, agent, console, &mut menus, effect, &displayed_sequences, &mut reply_lines).await {
                            Ok(opened)=>opened,
                            Err(error)=>{
                                #[cfg(test)]
                                agent.record_recovery_fact(recovery_tests::RecoveryFact::NavigationRejected {
                                    selected_chat: selected_chat.map(Into::into),
                                    visible: global.projection().visible,
                                });
                                console.set_status(&format!("Cannot open the Agent · {}",safe_line(&error)))?;
                                status_dirty = true;
                                continue;
                            },
                        };
                        if let Some(snapshot)=opened {
                            overview_active=false;selected_chat=Some(snapshot.id);
                            displayed_sequences.insert(snapshot.id.into(),snapshot.last_sequence);
                            tools.restore_snapshot(&snapshot);
                            sync_snapshot(&snapshot,&mut pending,&mut permissions,&mut questions);
                            message_groups=MessageGroups::default();
                            #[cfg(test)]
                            agent.record_recovery_fact(recovery_tests::RecoveryFact::AgentOpened {chat:snapshot.id.into()});
                        }
                    } else {console.set_status("This Agent is no longer live; reopen its saved history to recover")?;}
                    status_dirty = true;
                },
                Some(InputEvent::Suspend | InputEvent::OpenRewind | InputEvent::TakeBackQueued { .. }
                    | InputEvent::OpenTranscript { .. } | InputEvent::ExpandChoice { .. } | InputEvent::PlanEdited { .. }
                    | InputEvent::BackgroundForegroundShell | InputEvent::Permissions(_)
                    | InputEvent::UserShellSubmitted(_) | InputEvent::CycleExecutionMode) if login_active => {},
                Some(InputEvent::Escape) => {
                    if let Some(id) = active_login {
                        close_login_session(logins.remove(&id).unwrap(), console, &mut menus, true)?;
                        status_dirty = true;
                        continue;
                    }
                    if let Some(flow) = logout.as_ref() {
                        flow.cancel();
                        console.set_status("Cancelling logout…")?;
                        status_dirty = true;
                        continue;
                    }
                    if console.close_transcript()? { continue; }
                },
                Some(InputEvent::Interrupt) if logout_active => {
                    logout.as_ref().unwrap().cancel();
                    console.set_status("Cancelling logout…")?;
                    status_dirty = true;
                },
                Some(event @ InputEvent::Cancel { menu_id }) if logout_active && menus.provider_logout_menu_is_active(menu_id) => {
                    logout.as_ref().unwrap().cancel();
                    paused_inputs.push_back(event);
                    console.set_status("Cancelling logout…")?;
                    status_dirty = true;
                },
                Some(event) if logout_active => {
                    paused_inputs.push_back(event);
                    console.set_status("Logging out · input will run afterward")?;
                    status_dirty = true;
                },
                Some(InputEvent::Suspend) => {
                    console.suspend()?;
                    status_dirty=true;
                },
                Some(InputEvent::SaveMonitorWidth { columns }) => {
                    if let Err(error) = save_monitor_width(user_settings.as_ref(), columns) { console.set_status(&safe_text(&error))?; }
                    status_dirty = true;
                },
                Some(InputEvent::ToggleMonitor { show }) => {
                    if let Err(error) = set_monitor_visible(console, user_settings.as_ref(), show) { console.set_status(&safe_text(&error))?; }
                    else if show && let Some(chat) = selected_chat { artifact_ui::refresh(agent, console, config_root.as_deref(), chat.into()).await?; }
                    status_dirty = true;
                },
                Some(InputEvent::ArtifactChoice(choice)) => {
                    let Some(root) = config_root.as_deref() else { continue; };
                    if !console.artifact_picker_matches(choice.picker)? { continue; }
                    if matches!(choice.action, artifacts_picker::ArtifactPickerAction::Add(_)) && selected_chat.is_none() {
                        let _ = ensure_chat_with_model_setup(console, agent, &mut menus, &mut selected_chat, &workdir).await?;
                    }
                    match artifact_ui::apply_choice(agent, console, root, selected_chat.map(Into::into), *choice).await {
                        Ok(()) => {}, Err(error) => console.artifact_picker_error(safe_text(&error))?,
                    }
                    status_dirty = true;
                },
                Some(InputEvent::ArtifactSignOut) => {
                    menus.confirm_artifact_sign_out(console, ConfigTarget::visible(selected_chat.map(Into::into)), false)?;
                },
                Some(InputEvent::TickPackages(work)) => {
                    if let Some(root) = config_root.as_deref() { ticks_work::start(root, work, &mut tick_package_tasks); }
                },
                Some(InputEvent::Mcp(work)) => {
                    if let Some(root) = config_root.as_deref() { mcp_work::start(console, root, agent, work, &mut mcp_tasks)?; }
                },
                Some(InputEvent::TickAction { .. }) => {},
                Some(InputEvent::OpenRewind) => {
                    let is_busy=selected_chat.is_some_and(|chat| pending.values().any(|receipt|receipt.chat_id==chat)
                        || compaction_chats.values().any(|(candidate,_)|*candidate==chat)
                        || directed_summaries.contains(chat)
                        || summary_recovery_required.contains(&chat)
                        || foreground_shells.contains_key(&chat.uuid()));
                    open_rewind(agent,console,&mut menus,selected_chat,is_busy).await?;
                },
                Some(InputEvent::Interrupt) => {
                    if let Some(id) = active_login {
                        close_login_session(logins.remove(&id).unwrap(), console, &mut menus, true)?;
                        status_dirty = true;
                        continue;
                    }
                    if console.close_transcript()? { continue; }
                    if overview_active {
                        if console.confirm_control_c_exit()? {return Ok(());}
                        continue;
                    }
                    if let Some(chat)=selected_chat
                        && let Some(command_id)=foreground_shells.get(&chat.uuid()).copied()
                    {
                        match agent.stop_user_shell(chat.into(),command_id).await {
                            Ok(_) => outln!(console,"\nStopped user shell {}.",shown_id(command_id)),
                            Err(error) => outln!(console,"\nCannot stop user shell: {}",safe_text(&error.to_string())),
                        }
                        foreground_shells.remove(&chat.uuid());
                        console.set_foreground_shell(false)?;
                        status_dirty=true;
                        continue;
                    }
                    if let Some(chat)=selected_chat
                        && directed_summaries.contains(chat)
                    {
                        agent.interrupt(chat.into()).await.map_err(|error|error.to_string())?;
                        outln!(console,"\nCancelling conversation summary…");
                        status_dirty=true;
                        continue;
                    }
                    // As in Codex: on an empty input a running turn is interrupted as Esc does, and
                    // an idle Agent's View is left.
                    if let Some(chat) = selected_chat
                        && pending.values().any(|receipt| receipt.chat_id == chat)
                    {
                        agent.interrupt(chat.into()).await.map_err(|error|error.to_string())?;
                        user_interrupted_runs.extend(pending.values().filter(|receipt| receipt.chat_id == chat).map(|receipt| receipt.run_id));
                        stop_visual_pacing(console,chat,&pending,&mut thinking_previews,&mut reply_lines,&mut unpaced_runs)?;
                        status_dirty=true;
                    } else if console.leave_on_control_c()? {
                        return Ok(());
                    }
                },
                Some(InputEvent::InterruptActivity) => {
                    let Some(chat) = selected_chat.filter(|chat| idle_interruptible_chat == Some(*chat) || running_activity.as_ref().is_some_and(|activity| activity.chat_id == *chat && activity.interruptible)) else { continue; };
                    if idle_interruptible_chat == Some(chat) || pending.values().any(|receipt| receipt.chat_id == chat) || directed_summaries.contains(chat) {
                        match agent.interrupt(chat.into()).await {
                            Ok(_) => {
                                user_interrupted_runs.extend(pending.values().filter(|receipt| receipt.chat_id == chat).map(|receipt| receipt.run_id));
                                stop_visual_pacing(console,chat,&pending,&mut thinking_previews,&mut reply_lines,&mut unpaced_runs)?;
                                if directed_summaries.contains(chat) { outln!(console,"\nCancelling conversation summary…"); }
                            },
                            Err(error) => console.set_status(&format!("Cannot interrupt: {}",safe_text(&error.to_string())))?,
                        }
                        status_dirty=true;
                    }
                },
                Some(InputEvent::SendQueuedNow { run_id }) => {
                    let Some(chat) = selected_chat else { continue; };
                    if let Err(error) = agent.send_now(chat.into(), run_id).await {
                        console.set_status(&format!("Cannot send now: {}", safe_line(&error.to_string())))?;
                    }
                    status_dirty = true;
                },
                Some(InputEvent::TakeBackQueued { draft_empty }) => {
                    let Some(chat) = selected_chat else {
                        console.set_status("No Chat is selected")?;
                        continue;
                    };
                    match agent.take_queued_inputs(chat.into(), draft_empty).await {
                        Ok(entries) if entries.is_empty() => {
                            let status=agent.status(chat.into()).await.map_err(|error|error.to_string())?;
                            console.set_queued_inputs(chat.into(),status.queued_inputs())?;
                        },
                        Ok(entries) => {
                            for entry in &entries {
                                pending.remove(&entry.id);
                                user_shell_offsets.remove(&entry.id);
                                user_shell_chats.remove(&entry.id);
                                queued_shells.remove(&entry.id);
                                foreground_shells.remove(&entry.chat_id.uuid());
                            }
                            console.restore_queued_inputs(&entries)?;
                            console.set_status("Queued input restored for editing")?;
                        },
                        Err(error) => console.set_status(&format!("Cannot restore queued input: {}",safe_text(&error.to_string())))?,
                    }
                    status_dirty=true;
                },
                Some(InputEvent::OpenTranscript { id }) => {
                    if let Some(chat)=selected_chat {
                        let snapshot=agent.snapshot(chat.into()).await.map_err(|error|error.to_string())?;
                        console.update_transcript(id,presentation::transcript_document(&snapshot, console.display_profile()?, &console.image_links()?, &console.agent_colors()?))?;
                    } else {console.update_transcript(id,vec!["No conversation yet. Return to enter a task.".into()].into())?;}
                },
                Some(InputEvent::PickerAction { menu_id, index, action }) => {
                    if !login_active && logout.is_none() {
                        match menus.picker_action(agent, console, menu_id, index, action).await {
                            Ok(outcome) => menu_outcome = Some(outcome),
                            Err(error) => outln!(console, "Cannot apply selection: {}", safe_text(&error)),
                        }
                        status_dirty = true;
                    }
                },
                Some(InputEvent::Choice {menu_id,index,comment}) => {
                    if let Some(session) = logins.get(&menu_id) {
                        if index == 0 && session.flow.is_browser() && !session.flow.completed() {
                            session.flow.submit_callback(console, comment.as_deref())?;
                            console.continue_menu_submission(menu_id)?;
                            continue;
                        }
                        close_login_session(logins.remove(&menu_id).unwrap(), console, &mut menus, true)?;
                        status_dirty = true;
                        continue;
                    }
                    status_dirty=true;
                    match menus.choose(agent,console,menu_id,Some(index),comment).await {
                        Ok(outcome)=>menu_outcome=Some(outcome),
                        Err(error)=>outln!(console,"Cannot apply selection: {}",safe_text(&error)),
                    }
                },
                Some(InputEvent::PlanEdited {menu_id,markdown}) => {
                    status_dirty=true;
                    match menus.edit_plan(agent,console,menu_id,markdown).await {
                        Ok(())=>console.set_status("Plan edited · approve or keep planning on the new version")?,
                        Err(error)=>console.set_status(&format!("Cannot save the edited plan: {}",safe_line(&error)))?,
                    }
                },
                Some(InputEvent::ExpandChoice {menu_id,index}) => {
                    status_dirty=true;
                    if let Err(error)=menus.expand(console,menu_id,index) {
                        console.set_status(&format!("Cannot expand selection: {}",safe_text(&error)))?;
                    }
                },
                Some(InputEvent::SwitchQuestion {menu_id,next}) => {
                    status_dirty=true;
                    if let Err(error)=menus.switch_question(console,menu_id,next) {
                        console.set_status(&format!("Cannot switch question: {}",safe_text(&error)))?;
                    }
                },
                Some(InputEvent::Cancel {menu_id}) => {
                    if let Some(session) = logins.remove(&menu_id) {
                        close_login_session(session, console, &mut menus, true)?;
                        status_dirty = true;
                        continue;
                    }
                    status_dirty=true;
                    match menus.choose(agent,console,menu_id,None,None).await {
                        Ok(outcome)=>menu_outcome=Some(outcome),
                        Err(error)=>outln!(console,"Cannot cancel selection: {}",safe_text(&error)),
                    }
                },
                Some(InputEvent::BackgroundForegroundShell) => {
                    let Some(chat)=selected_chat else { continue; };
                    // Like Claude Code's `backgroundAll`, one press moves every foreground Shell.
                    if let Some(command_id)=foreground_shells.get(&chat.uuid()).copied() {
                        match agent.background_user_shell(chat.into(),command_id).await {
                            Ok(_) => {
                                foreground_shells.remove(&chat.uuid());
                                backgrounded_shells.insert(command_id);
                                console.set_foreground_shell(false)?;
                                console.set_shell_preview(&[])?;
                                console.write_response(&presentation::backgrounded_shell_runs(console.display_profile()?))?;
                            },
                            Err(error)=>outln!(console,"Cannot background user shell: {}",safe_text(&error.to_string())),
                        }
                    }
                    // The Bash call returns to the model on its own once its command leaves the foreground.
                    for command_id in console.backgroundable_commands()? {
                        if let Err(error)=agent.background_command(chat.into(),command_id).await {
                            outln!(console,"Cannot background command: {}",safe_text(&error.to_string()));
                        }
                    }
                    status_dirty=true;
                },
                Some(InputEvent::Permissions(effect)) => {
                    match effect {
                        PermissionsEffect::Close { destination: PermissionsReturn::ModePicker { settings } } => {
                            menus.permission_modes(agent, console, ConfigTarget::visible(selected_chat.map(Into::into)), settings).await?;
                        },
                        PermissionsEffect::GenerateEnvironment { generation, options } => {
                            let Some(chat) = console.permissions_view_chat()? else { continue; };
                            let agent = agent.clone(); let console = console.clone();
                            tokio::spawn(async move {
                                let result = agent.generate_auto_environment(chat, options).await.map_err(|error| safe_text(&error.to_string()));
                                let _ = console.environment_generated(chat, generation, result);
                            });
                        },
                        PermissionsEffect::None => {},
                        PermissionsEffect::Apply(change) => match console.permissions_subject()? {
                            Some(PermissionsSubject::Agent { chat, revision }) => {
                                match agent.apply_permission_change(chat,change,revision).await {
                                    Ok(view)=>console.permissions_saved(view)?,
                                    Err(AccessError { latest_permissions: Some(latest), .. }) => console.permissions_conflict(*latest)?,
                                    Err(error)=>console.permissions_error(Some(chat), safe_text(&error.to_string()))?,
                                }
                            },
                            // Saved rules and Auto policy are written now; the mode, session rules
                            // and directories wait for the first input. No Agent is created.
                            Some(PermissionsSubject::NewDraft { project, revision }) => {
                                match agent.change_draft_permissions(&project,&revision,change).await {
                                    Ok(change)=>{
                                        let kept=change.kept;
                                        console.permissions_saved(change.page)?;
                                        if kept {console.permissions_notice(None, "Kept for the New Agent; applies when you send the first input".into())?;}
                                    },
                                    Err(DraftPermissionsError::Conflict(latest))=>console.permissions_conflict(*latest)?,
                                    Err(DraftPermissionsError::Rejected(error))=>console.permissions_error(None, safe_text(&error.to_string()))?,
                                }
                            },
                            None => {},
                        },
                        PermissionsEffect::RefreshDenials => {
                            let Some(chat)=console.permissions_view_chat()? else {continue;};
                            match agent.approval_history_view(chat).await {
                                Ok(view)=>console.permissions_denials(chat, Ok(view))?,
                                Err(error)=>console.permissions_denials(chat, Err(safe_text(&error.to_string())))?,
                            }
                        },
                        PermissionsEffect::RetryDenial(record_id) => {
                            let Some(chat)=console.permissions_view_chat()? else {continue;};
                            match agent.request_denial_retry(chat,record_id).await {
                                Ok(receipt)=>{
                                    console.close_permissions()?;
                                    pending.insert(receipt.run_id.into(),receipt);
                                    status_dirty=true;
                                    outln!(console,"New retry requested; the Agent will prepare the operation and check current permissions.");
                                },
                                Err(error)=>console.permissions_denials(chat,Err(safe_text(&error.to_string())))?,
                            }
                        },
                        PermissionsEffect::OpenJudgmentSettings { select_reviewer } => {
                            let Some(subject)=console.permissions_subject()? else {continue;};
                            let chat = subject.chat();
                            if let Err(error) = menus.judgment_settings_from_permissions(agent,console,subject,select_reviewer).await {
                                console.permissions_error(chat, format!("Cannot open Judgment settings: {}",safe_text(&error)))?;
                            }
                        },
                    }
                },
                Some(event)
                    if should_pause_input_for_summary(&event)
                        && selected_chat.is_some_and(|chat| {
                        directed_summaries.contains(chat)
                            || summary_recovery_required.contains(&chat)
                    }) =>
                {
                    paused_inputs.push_back(event);
                    console.set_status("Working · input will be available after the summary")?;
                },
                Some(InputEvent::UserShellSubmitted(submission)) => {
                    let retry_of = console.take_submitted_retry()?;
                    if overview_active {
                        if let Err(error)=agent.refresh_interaction_source().await {
                            console.restore_rejected_shell(submission.command(),&safe_line(&error.to_string()))?;
                            continue;
                        }
                        let receipt=global.dispatch(agent.launch_spec(),UserInput::Shell { command:submission.command().into(),background:true },agent.origin()).await;
                        match receipt {
                            Ok(receipt)=>match agent.accept_created(receipt).await {
                                // A background Shell started from Overview leaves the list open for the next one.
                                CreationAcceptance::Attached(..)=>{},
                                CreationAcceptance::AttachUnavailable(error)=>console.set_status(&creation_attach_unavailable(&error))?,
                                CreationAcceptance::Rejected(error)=>console.restore_rejected_shell(submission.command(),&format!("New Agent was not accepted: {}",safe_line(&error.to_string())))?,
                            },
                            Err(error)=>{console.restore_rejected_shell(submission.command(),&format!("New Agent was not confirmed: {error}"))?;}
                        }
                        show_creation_feedback(console,&global.projection())?;
                        global_refresh.reset_immediately();
                        status_dirty=true;continue;
                    }
                    if selected_chat.is_some() && agent.disconnected().is_some() {
                        console.restore_rejected_shell(submission.command(),"This execution is unavailable; use /resume explicitly")?;
                        continue;
                    }
                    let directory=global.projection().dispatch_directory;
                    let chat = match ensure_chat_with_model_setup(console,agent,&mut menus,&mut selected_chat,&directory).await {
                        Ok(chat)=>chat,
                        Err(error)=>{console.restore_rejected_shell(submission.command(),&error)?;continue;},
                    };
                    let history_line=format!("!{}",submission.command());
                    // Only this attempt's own echo or queue entry may be withdrawn if it is held back
                    // unsent. While a turn works, the command waits in the queue until it starts.
                    let queue_entry=if retry_of.is_none() && input_waits(chat.into(),&pending,&foreground_shells) {console.queue_submitted_input(QueuedInputKind::Shell,submission.command())?} else {None};
                    let echo=if retry_of.is_none() && queue_entry.is_none() {console.record_input(&history_line)?} else {None};
                    console.record_prompt_history(&history_line)?;
                    let starts_in_background = submission.execution()==shell_input::ShellExecution::Background;
                    let sent=send_input(agent,chat.into(),retry_of,agent.submit_user_shell(chat,submission.command().to_owned(),starts_in_background),|result|match result {
                        CommandResult::ShellSubmitted(receipt)=>Some(receipt),
                        _=>None,
                    }).await;
                    match sent {
                        InputSend::Accepted { value: receipt, adopted }=>{
                            if let Some(id)=adopted {console.set_status(&input_adopted(id))?;}
                            user_shell_offsets.insert(receipt.command_id,UserShellOutput::default());
                            user_shell_chats.insert(receipt.command_id,chat);
                            // The terminal UI shows the command as its echo and its output under it.
                            if !interactive {
                                outln!(
                                    console,
                                    "\n{} Shell {}\n  $ {}",
                                    console.display_profile()?.glyph(Glyph::ToolCall),
                                    receipt.command_id,
                                    safe_text(submission.command())
                                );
                            }
                            let status = agent
                                .status(chat)
                                .await
                                .map_err(|error| error.to_string())?;
                            let shell_state = status
                                .user_shells
                                .iter()
                                .find(|record| record.id == receipt.command_id)
                                .map(|record| record.state.clone());
                            {
                                // The command is listed where the Agent queued it, or shown as
                                // sent once it has started, in one frame.
                                let _frame_hold = console.hold_frames()?;
                                console.set_queued_inputs(chat, status.queued_inputs())?;
                                if shell_state == Some(UserShellState::Queued) {
                                    if let Some(echo) = echo {
                                        console.withdraw_echo(Some(chat), echo)?;
                                    }
                                    if retry_of.is_none() {
                                        queued_shells.insert(receipt.command_id);
                                    }
                                } else if queue_entry.is_some() {
                                    console.record_consumed_input(chat, receipt.command_id, &history_line)?;
                                }
                            }
                            if !starts_in_background && shell_state == Some(UserShellState::Running) {
                                foreground_shells.insert(chat,receipt.command_id);
                                console.set_foreground_shell(true)?;
                            }
                        },
                        InputSend::Unknown(id, error)=>{
                            if let Some(entry)=queue_entry {console.withdraw_queued_input(entry)?;}
                            outln!(console,"Cannot run user shell: {}",safe_text(&error.to_string()));
                            console.restore_unknown_shell(submission.command(),&safe_text(&error.to_string()),Some(id))?;
                        },
                        InputSend::Held(error)=>{
                            if let Some(echo)=echo {console.withdraw_echo(Some(chat),echo)?;}
                            if let Some(entry)=queue_entry {console.withdraw_queued_input(entry)?;}
                            outln!(console,"Cannot run user shell: {}",safe_text(&error.to_string()));
                            console.restore_rejected_shell(submission.command(),&safe_text(&error.to_string()))?;
                        },
                        InputSend::Failed(error)=>{
                            if let Some(entry)=queue_entry {console.withdraw_queued_input(entry)?;}
                            outln!(console,"Cannot run user shell: {}",safe_text(&error.to_string()));
                            console.restore_rejected_shell(submission.command(),&safe_text(&error.to_string()))?;
                        },
                    }
                    status_dirty=true;
                },
                Some(input @ (InputEvent::Line(_) | InputEvent::Prompt { .. })) => {
                    console.clear_prompt_suggestion()?;
                    let retry_of = console.take_submitted_retry()?;
                    let (line, file_references, pasted_images, restored_contexts, is_user_prompt, family_target, send_now) = match input {
                        InputEvent::Line(line) => (line, Vec::new(), Vec::new(), Vec::new(), false, None, false),
                        InputEvent::Prompt { text, file_references, images, family_target, send_now } => {
                            let restored_contexts = if is_branch_command(text.trim()) {
                                Vec::new()
                            } else {
                                console.take_restored_contexts(&text)?
                            };
                            (text, file_references, images, restored_contexts, true, family_target, send_now)
                        }
                        _ => unreachable!(),
                    };
                    let is_command = console.is_command_input(&line)?;
                    if let Some(id) = active_login {
                        if matches!(line.trim(), "cancel" | "/cancel" | "/exit") {
                            close_login_session(logins.remove(&id).unwrap(), console, &mut menus, true)?;
                            if line.trim() == "/exit" { return view_exit(interactive, failed); }
                        } else if logins[&id].flow.is_browser() && !logins[&id].flow.completed() {
                            logins[&id].flow.submit_callback(console, Some(&line))?;
                        } else {
                            outln!(console, "Login is waiting. Complete the official authorization or enter cancel.");
                        }
                        continue;
                    }
                    if let Some(arguments)=line.strip_prefix("/agent-location ") {
                        let fields=arguments.split_whitespace().collect::<Vec<_>>();
                        if fields.len()!=3 {console.set_status("Invalid Agent location choice")?;continue;}
                        let parsed=fields[0].parse::<u64>().ok().zip(fields[1].parse().ok()).zip(fields[2].parse().ok());
                        if let Some(((generation,session_id),view_id))=parsed {
                            match global.interpret(crate::agent_console::Intent::ChooseLocation {generation,view:ViewRef{session_id,view_id}}) {
                                Ok(Some(effect))=>{if let Some(snapshot)=navigate_agent(&mut global,agent,console,&mut menus,effect,&displayed_sequences,&mut reply_lines).await? {
                                    selected_chat=Some(snapshot.id);overview_active=false;
                                    displayed_sequences.insert(snapshot.id.into(),snapshot.last_sequence);
                                    tools.restore_snapshot(&snapshot);
                                    sync_snapshot(&snapshot,&mut pending,&mut permissions,&mut questions);
                                    message_groups=MessageGroups::default();
                                }},
                                Ok(None)=>{},Err(error)=>console.set_status(&safe_text(&error.to_string()))?,
                            }
                        }
                        continue;
                    }
                    if (overview_active && menus.model_setup_pending()
                        || menus.model_setup_applies_to(selected_chat.map(Into::into)))
                        && model_setup_blocks_input(&line)
                    {
                        console.restore_rejected_prompt(restored_contexts, "Choose a model before sending")?;
                        if let Err(error) = menus.model_setup(agent, console, false).await {
                            console.set_status(&safe_text(&error))?;
                        }
                        status_dirty = true;
                        continue;
                    }
                    if overview_active {
                        if line.trim()=="/exit" {return view_exit(interactive,failed);}
                        // The reference is shown in place; only a command the overview cannot name becomes a Skill dispatch.
                        if line.trim()=="/help advanced" {menus.help_advanced(console)?;continue;}
                        if let Some(command_id)=line.trim().strip_prefix("/acknowledge-create ") {
                            match tickloop_ids::base62::decode_either(command_id).ok_or_else(||format!("{command_id} is not a command ID"))
                                .and_then(|id|global.interpret(crate::agent_console::Intent::AcknowledgeUnconfirmedDispatch(id)).map_err(|error|error.to_string())) {
                                Ok(_)=>{console.set_creation_notice(None)?;console.set_status("Unknown creation acknowledged; no command was replayed")?;},
                                Err(error)=>{console.restore_rejected_prompt(restored_contexts,&error)?;report_refused_input(console,&error,&mut failed);},
                            }
                            continue;
                        }
                        if line.trim()=="/clear" {
                            global.interpret(crate::agent_console::Intent::NewDraft).map_err(|error|error.to_string())?;
                            console.set_global_overview(None)?;console.clear_visible_agent()?;
                            overview_active=false;selected_chat=None;
                            agent.display_chat(None).await.map_err(|error|error.to_string())?;
                            status_dirty=true;continue;
                        }
                        if line.trim()=="/resume" {
                            let projection=global.projection();
                            let listed=projection.rows.iter().map(|row|row.chat_id).chain(projection.ended.iter().map(|ended|ended.record.chat_id)).collect::<Vec<_>>();
                            match Workflow::chats_except(agent,&projection.dispatch_directory,&listed,false).await {
                                Ok(workflow)=>{
                                    console.set_global_overview(None)?;global.interpret(crate::agent_console::Intent::Back).map_err(|error|error.to_string())?;
                                    overview_active=false;history_return_overview=true;menus.conversations(console,workflow)?;
                                },
                                Err(error)=>{console.restore_rejected_prompt(restored_contexts,&error)?;report_refused_input(console,&error,&mut failed);},
                            }
                            continue;
                        }
                        if let Some(listed)=line.trim().strip_prefix("/resume ").and_then(ChatId::parse).and_then(|chat|{
                            let projection=global.projection();
                            projection.rows.iter().find(|row|row.chat_id==chat.uuid()&&row.subagent.is_none()&&row.parent.is_none()).map(|row|row.execution.agent_id)
                                .or_else(||projection.ended.iter().find(|ended|ended.record.chat_id==chat.uuid()).map(|ended|ended.record.chat_id))
                        }) {
                            console.select_overview_agent(listed)?;
                            console.set_status(ALREADY_IN_OVERVIEW)?;
                            continue;
                        }
                        if line.trim().starts_with("/resume ") {
                            match open_chat(agent,line.trim(),None).await {
                                Ok(snapshot)=>{
                                    console.set_global_overview(None)?;global.interpret(crate::agent_console::Intent::Back).map_err(|error|error.to_string())?;
                                    overview_active=false;learn_agent_colors(console,agent,&snapshot).await?;show_opened_chat(console,agent,&snapshot)?;selected_chat=Some(snapshot.id);
                                    sync_snapshot(&snapshot,&mut pending,&mut permissions,&mut questions);
                                },
                                Err(error)=>{console.restore_rejected_prompt(restored_contexts,&error)?;report_refused_input(console,&error,&mut failed);},
                            }
                            continue;
                        }
                        if let Some(path)=line.trim().strip_prefix("/cwd ") {
                            let directory=PathBuf::from(path);
                            let directory=if directory.is_absolute(){directory}else{global.projection().dispatch_directory.join(directory)};
                            match directory.canonicalize() {
                                Ok(directory) if directory.is_dir()=>{global.interpret(crate::agent_console::Intent::DispatchDirectory(directory)).map_err(|error|error.to_string())?;},
                                _=>{console.restore_rejected_prompt(restored_contexts,DISPATCH_DIRECTORY_MISSING)?;report_refused_input(console,DISPATCH_DIRECTORY_MISSING,&mut failed);continue;}
                            }
                            global_refresh.reset_immediately();status_dirty=true;continue;
                        }
                        if line.trim()=="/cwd" {
                            console.set_status(&format!("New Agents start in {}",safe_line(&global.projection().dispatch_directory.display().to_string())))?;
                            continue;
                        }
                        if let Some((command,reason))=completion::unavailable_command(&line,completion::CommandScope::Overview) {
                            let refusal=completion::command_refusal(console.command_scope(),command,reason);
                            console.restore_rejected_prompt(restored_contexts,&refusal)?;
                            report_refused_input(console,&refusal,&mut failed);
                            continue;
                        }
                        if let Some(usage) = completion::unhandled_builtin_usage(&line) {
                            console.restore_rejected_prompt(restored_contexts,&usage)?;
                            console.hold_status(&usage)?;
                            report_refused_input(console,&usage,&mut failed);
                            continue;
                        }
                        let input=if is_command && let Some(skill)=line.trim().strip_prefix('/') {
                            let (name,arguments)=skill.split_once(' ').unwrap_or((skill,""));
                            UserInput::Skill {name:name.into(),arguments:arguments.into()}
                        } else {
                            UserInput::Text {text:line.clone(),contexts:pasted_images.into_iter().map(InputContext::Image).chain(restored_contexts.clone()).collect(),paths:file_references}
                        };
                        console.record_input(&line)?;console.record_prompt_history(&line)?;
                        if let Err(error)=agent.refresh_interaction_source().await {
                            let reason=safe_line(&error.to_string());
                            console.restore_rejected_prompt(restored_contexts,&reason)?;
                            report_refused_input(console,&reason,&mut failed);
                            continue;
                        }
                        match global.dispatch(agent.launch_spec(),input,agent.origin()).await {
                            Ok(receipt)=>match agent.accept_created(receipt).await {
                                CreationAcceptance::Attached(created,submitted)=>{
                                    if let Some(receipt)=submitted {suggestible_runs.insert(receipt.run_id.into());pending.insert(receipt.run_id.into(),receipt);}
                                    // The View goes on to the Agent it just started.
                                    paused_inputs.push_back(InputEvent::OpenAgent(created));
                                },
                                CreationAcceptance::AttachUnavailable(error)=>{
                                    let notice=creation_attach_unavailable(&error);
                                    console.set_status(&notice)?;
                                    report_refused_input(console,&notice,&mut failed);
                                },
                                CreationAcceptance::Rejected(error)=>{
                                    let reason=format!("New Agent was not accepted: {error}");
                                    console.restore_rejected_prompt(restored_contexts,&reason)?;
                                    report_refused_input(console,&reason,&mut failed);
                                },
                            },
                            Err(error)=>{
                                let reason=format!("New Agent acceptance is unconfirmed: {error}; check the receipt before another submission");
                                console.restore_rejected_prompt(restored_contexts,&reason)?;
                                report_refused_input(console,&reason,&mut failed);
                            },
                        }
                        show_creation_feedback(console,&global.projection())?;
                        global_refresh.reset_immediately();
                        status_dirty=true;continue;
                    }
                    if selected_chat.is_some() && agent.disconnected().is_some()
                        && !matches!(line.trim(),"/exit"|"/clear"|"/status"|"/help"|"/help advanced"|"/resume"|"/tui"|"/details")
                        && !line.trim().starts_with("/resume ") {
                        console.restore_rejected_prompt(restored_contexts,EXECUTION_UNAVAILABLE_FOR_INPUT)?;
                        report_refused_input(console,EXECUTION_UNAVAILABLE_FOR_INPUT,&mut failed);
                        continue;
                    }
                    if let (Some(source_chat), Some(target)) = (selected_chat, family_target) {
                        let contexts = pasted_images.into_iter().map(InputContext::Image).chain(restored_contexts.clone()).collect();
                        match send_input(agent, source_chat, retry_of,
                            agent.send_family_input(source_chat.into(), target, line.clone(), contexts, file_references, None),
                            |result| match result {
                                CommandResult::FamilyInputSent { receipt, target_handle, .. } => Some((receipt, target_handle)),
                                _ => None,
                            }).await {
                            InputSend::Accepted { value: (_, handle), adopted } => {
                                if let Some(guard) = &_delayed_guard { guard.mark_accepted(); }
                                console.record_prompt_history(&line)?;
                                let _ = handle;
                                if let Some(id) = adopted { console.set_status(&input_adopted(id))?; }
                                global_refresh.reset_immediately();
                            }
                            InputSend::Unknown(id, error) => {
                                console.restore_unknown_prompt(restored_contexts, &error.to_string(), Some(id))?;
                                report_refused_input(console, &error.to_string(), &mut failed);
                            }
                            InputSend::Held(error) => {
                                console.restore_unknown_prompt(restored_contexts, &error.to_string(), None)?;
                                report_refused_input(console, &error.to_string(), &mut failed);
                            }
                            InputSend::Failed(error) => {
                                let reason = format!("Directed message failed · {}", safe_line(&error.to_string()));
                                console.restore_rejected_prompt(restored_contexts, &reason)?;
                                report_refused_input(console, &reason, &mut failed);
                            }
                        }
                        status_dirty = true; continue;
                    }
                    // Retrying an unchanged draft checks its receipt without another optimistic echo
                    // or queue entry. While a turn works, an input for a later turn waits in the queue,
                    // as Claude Code lists it, and shows as sent only once a turn takes it.
                    let waiting_kind=(retry_of.is_none() && is_user_prompt && selected_chat.is_some_and(|chat|input_waits(chat,&pending,&foreground_shells))).then(||waiting_input_kind(&line,is_command)).flatten();
                    let mut queue_entry=match waiting_kind {Some(kind)=>console.queue_submitted_input(kind,&line)?,None=>None};
                    let mut echo=if retry_of.is_none() && queue_entry.is_none() {console.record_input(&line)?} else {None};
                    let record_after_chat = is_user_prompt
                        && selected_chat.is_none()
                        && !is_command;
                    if is_user_prompt
                        && !record_after_chat
                        && !is_branch_command(line.trim())
                    {
                        console.record_prompt_history(&line)?;
                    }
                    status_dirty=true;
                    let original_input = line;
                    let line = original_input.trim().to_owned();
                    if is_user_prompt
                        && is_command
                        && command_timing(&line) == CommandTiming::AfterTurn
                        && let Some(chat) = selected_chat
                    {
                        let status=agent.status(chat.into()).await.map_err(|error|error.to_string())?;
                        let is_busy=matches!(status.chat_loop.state,tickloop_harness::protocol::LoopState::Running|tickloop_harness::protocol::LoopState::WaitingPermission|tickloop_harness::protocol::LoopState::WaitingQuestion);
                        if is_busy {
                            let queued=agent.queue_command(chat.into(),original_input.clone());
                            match send_input(agent,chat,retry_of,queued,|result|match result {
                                CommandResult::Queued { command_id }=>Some(command_id),
                                _=>None,
                            }).await {
                                InputSend::Accepted { adopted, .. }=>{
                                    if let Some(echo)=echo {console.withdraw_echo(Some(chat.into()),echo)?;}
                                    console.set_status(&adopted.map(input_adopted).unwrap_or_else(||"Command queued until the current turn ends".into()))?;
                                },
                                InputSend::Unknown(id, error)=>{
                                    if let Some(echo)=echo {console.withdraw_echo(Some(chat.into()),echo)?;}
                                    if let Some(entry)=queue_entry {console.withdraw_queued_input(entry)?;}
                                    console.restore_unknown_prompt(restored_contexts,&safe_line(&error.to_string()),Some(id))?;
                                    report_refused_input(console,&error.to_string(),&mut failed);
                                },
                                InputSend::Held(error)=>{
                                    if retry_of.is_none() && let Some(echo)=echo {console.withdraw_echo(Some(chat.into()),echo)?;}
                                    if let Some(entry)=queue_entry {console.withdraw_queued_input(entry)?;}
                                    console.restore_unknown_prompt(restored_contexts,&safe_line(&error.to_string()),None)?;
                                    report_refused_input(console,&error.to_string(),&mut failed);
                                },
                                InputSend::Failed(error)=>{
                                    if let Some(entry)=queue_entry {console.withdraw_queued_input(entry)?;}
                                    console.restore_unknown_prompt(restored_contexts,&safe_line(&error.to_string()),None)?;
                                    report_refused_input(console,&error.to_string(),&mut failed);
                                },
                            }
                            status_dirty=true;
                            continue;
                        }
                        // The turn ended before the command was sent, so it runs now and shows as sent.
                        if let Some(entry)=queue_entry.take() {
                            let _frame_hold=console.hold_frames()?;
                            console.withdraw_queued_input(entry)?;
                            echo=console.record_input(&original_input)?;
                        }
                    }
                    if !interactive && let Some(menu_id)=menus.id() && !is_command {
                        let selection=if line.eq_ignore_ascii_case("cancel") {Some(None)} else {line.parse::<usize>().ok().and_then(|number|number.checked_sub(1)).map(Some)};
                        if let Some(index)=selection {
                            match menus.choose(agent,console,menu_id,index,None).await {
                                Ok(outcome)=>menu_outcome=Some(outcome),
                                Err(error)=>outln!(console,"Cannot apply selection: {}",safe_text(&error)),
                            }
                        } else {outln!(console,"Enter an option number or cancel; no selection was applied.");}
                        continue;
                    }
                    if is_command && should_dismiss_menu_before_dispatch(&line) {menus.dismiss(console)?;}
                    match line.as_str() {
                        "/exit" => return view_exit(interactive, failed),
                        "/help" | "/" if interactive => menus.help(console,None)?,
                        "/help" => outln!(console, "{}",help_text()),
                        "/help advanced" if interactive => menus.help_advanced(console)?,
                        "/help advanced" => outln!(console, "{}",completion::command_help(true)),
                        command if command == "/artifacts" || command.starts_with("/artifacts ") => {
                            if command.trim() != "/artifacts" { outln!(console, "Use /artifacts"); continue; }
                            let Some(root) = config_root.as_ref() else { outln!(console, "Tickloop Artifacts needs a state directory; set TICKLOOP_CONFIG_DIR."); continue; };
                            match tickloop_harness::artifacts::ArtifactClient::new(root).map_err(|error| error.to_string()) {
                                Ok(client) => match client.status().await {
                                    Ok(Some(_)) => { if let Err(error) = artifact_ui::open(agent, console, root, selected_chat.map(Into::into), interactive).await { outln!(console, "Cannot open Tickloop Artifacts: {}", safe_text(&error)); } }
                                    Ok(None) => match LoginFlow::start(root.clone(), console, login::LoginMethod::Artifact) { Ok(flow) => { logins.insert(flow.id(), LoginSession { flow, from_provider: false, succeeded: false }); }, Err(error) => outln!(console, "Cannot start the Tickloop Artifacts login: {}", safe_text(&error)) },
                                    Err(error) => outln!(console, "Cannot check the Tickloop Artifacts account: {}", safe_text(&error.to_string())),
                                },
                                Err(error) => outln!(console, "Cannot configure Tickloop Artifacts: {}", safe_text(&error)),
                            }
                        },
                        command if matches!(command.split_whitespace().next(), Some("/plugins" | "/ticks")) => {
                            let kind = if command.split_whitespace().next() == Some("/ticks") { ticks_panel::PluginKind::Tick } else { ticks_panel::PluginKind::Plugin };
                            let (name, cli) = (ticks_work::panel_command(kind), kind.command());
                            if command.split_whitespace().count() != 1 { outln!(console, "Use {name}"); continue; }
                            let Some(root) = config_root.as_deref() else { outln!(console, "{name} needs a state directory; set TICKLOOP_CONFIG_DIR."); continue; };
                            if !interactive { outln!(console, "{name} needs an interactive terminal; run {cli} there instead."); continue; }
                            if let Err(error) = ticks_work::open(console, root, selected_chat.map(Into::into), kind, &mut tick_package_tasks) { outln!(console, "Cannot open {name}: {}", safe_text(&error)); }
                        },
                        command if command.split_whitespace().next() == Some("/mcp") => {
                            if command.split_whitespace().count() != 1 { outln!(console, "Use /mcp"); continue; }
                            let Some(root) = config_root.as_deref() else { outln!(console, "/mcp needs a state directory; set TICKLOOP_CONFIG_DIR."); continue; };
                            if !interactive { outln!(console, "/mcp needs an interactive terminal; run tickloop mcp list there instead."); continue; }
                            if let Err(error) = mcp_work::open(console, root, agent, selected_chat.map(Into::into), workdir.clone(), &mut mcp_tasks) { outln!(console, "Cannot open /mcp: {}", safe_text(&error)); }
                        },
                        command if is_rewind_command(command) => {
                            let is_busy=selected_chat.is_some_and(|chat| input_waits(chat,&pending,&foreground_shells)
                                || compaction_chats.values().any(|(candidate,_)|*candidate==chat)
                                || directed_summaries.contains(chat)
                                || summary_recovery_required.contains(&chat));
                            if open_rewind(agent,console,&mut menus,selected_chat,is_busy).await?.is_some() {
                                status_dirty=false;
                            }
                        },
                        "/clear" => {
                            if let Some(chat)=selected_chat {
                                let status=agent.status(chat.into()).await.map_err(|error|error.to_string())?;
                                if status.delegation.is_some() {
                                    console.hold_status(&completion::command_refusal(completion::CommandScope::Child,"/clear",completion::CHILD_HISTORY_FIXED))?;
                                    continue;
                                }
                                let mut shell_stop_failed=false;
                                let tasks=task_center.snapshot(chat.into()).await.map_err(|error|error.to_string())?;
                                for shell in tasks.tasks.iter().filter(|task|task.chat_id==chat && agent.disconnected().is_none() && !task.was_background && task.capabilities.can_stop && matches!(task.state,ShellTaskState::Running|ShellTaskState::Queued|ShellTaskState::WaitingPermission)) {
                                    if let Err(error)=task_center.dispatch(chat.into(),shell.id,tickloop_harness::protocol::ShellTaskAction::Stop).await {
                                        console.set_status(&format!("Cannot clear the foreground Shell: {}",safe_line(&error.to_string())))?;
                                        shell_stop_failed=true;
                                        break;
                                    }
                                }
                                if shell_stop_failed {continue;}
                            }
                            global.interpret(crate::agent_console::Intent::NewDraft).map_err(|error|error.to_string())?;
                            console.clear_visible_agent()?;
                            selected_chat=None;
                            agent.display_chat(None).await.map_err(|error|error.to_string())?;
                            message_groups=MessageGroups::default();
                            tools.restore(&[]);
                        },
                        "/status" => {
                            if let Some(chat)=selected_chat {
                                let status=agent.status(chat.into()).await.map_err(|error|error.to_string())?;
                                outln!(console,"Chat {} · {} · {:?} · {} · {} queued",chat.public(),safe_text(&status.model.model),status.model.protocol,status.policy.mode,status.queued_inputs().len());
                                if console.prompt_suggestion_enabled()? {
                                    let diagnostic = prompt_suggestion::suppression(&status)
                                        .map(str::to_owned)
                                        .unwrap_or_else(|| format!("last: {}", suggestion_diagnostics.get(&chat.uuid()).map_or("not generated in this View", |(_, reason)| *reason)));
                                    outln!(console, "Prompt suggestions: On · {diagnostic}");
                                } else { outln!(console, "Prompt suggestions: Off"); }
                            } else {outln!(console,"No Chat is selected.");}
                        },
                        command if command.split_whitespace().next() == Some("/context") => {
                            let mut arguments = command.split_whitespace().skip(1);
                            let expanded = match (arguments.next(), arguments.next()) {
                                (None, _) => false,
                                (Some(context_view::EXPAND_ARGUMENT), None) => true,
                                _ => { outln!(console, "Usage: /context [{}]", context_view::EXPAND_ARGUMENT); continue; }
                            };
                            let counted = match selected_chat {
                                Some(chat) => match (agent.context_breakdown(chat.into()).await, agent.status(chat.into()).await) {
                                    (Ok(breakdown), Ok(status)) => Ok(Some((breakdown, status.model))),
                                    (Err(error), _) | (_, Err(error)) => Err(error),
                                },
                                // Before the first message the New Agent does not exist yet: count what its first request would hold.
                                None => agent.draft_context_breakdown(global.projection().dispatch_directory).await,
                            };
                            match counted {
                                Ok(Some((breakdown, model))) => {
                                    let home = std::env::home_dir();
                                    let view = context_view::ContextView {
                                        breakdown: &breakdown,
                                        model_name: settings::model_name(&model),
                                        model_id: &model.model,
                                        home: home.as_deref(),
                                        expanded,
                                    };
                                    console.write_roles(&view.runs(console.conversation_width()?, console.display_profile()?))?;
                                },
                                Ok(None) => outln!(console,"No model is set up for the New Agent yet; choose one with /model to see its context."),
                                Err(error) => outln!(console,"Cannot read context usage: {}",safe_text(&error.to_string())),
                            }
                        },
                        "/resume" => match Workflow::chats(agent,&workdir,selected_chat.map(Into::into),false).await {
                            Ok(picker) => menus.conversations(console,picker)?,
                            Err(error) => outln!(console,"{error}"),
                        },
                        "/monitor" => {
                            let show = !console.monitor_is_visible()?;
                            if let Err(error) = set_monitor_visible(console, user_settings.as_ref(), show) { outln!(console, "{}", safe_text(&error)); }
                            else if show && let Some(chat) = selected_chat { artifact_ui::refresh(agent, console, config_root.as_deref(), chat.into()).await?; }
                        },
                        _ if line == "/plan" || line.starts_with("/plan ") => {
                            match run_plan_command(agent,console,&mut menus,selected_chat,&line["/plan".len()..]).await {
                                Ok(Some(description)) => paused_inputs.push_front(InputEvent::Line(description)),
                                Ok(None) => {},
                                Err(error) => outln!(console,"Cannot open plan: {}",safe_text(&error)),
                            }
                            status_dirty = true;
                        },
                        "/details" if interactive => {
                            let id = console.start_transcript()?;
                            if let Some(chat)=selected_chat {
                                let snapshot=agent.snapshot(chat.into()).await.map_err(|error|error.to_string())?;
                                console.update_transcript(id,presentation::transcript_document(&snapshot, console.display_profile()?, &console.image_links()?, &console.agent_colors()?))?;
                            } else {console.update_transcript(id,vec!["No conversation yet. Return to enter a task.".into()].into())?;}
                        },
                        "/details" => out!(console, "{}",tools.toggle()),
                        debug_line::DEBUG_COMMAND if console.shows_debug_details()? => console.copy_debug_details()?,
                        "/diff" if interactive => {
                            if let Some(chat) = selected_chat {
                                open_diff(agent,console,chat).await?;
                            } else {
                                outln!(console, "No Chat is selected. Enter a task first.");
                            }
                        },
                        "/diff" => {
                            if let Some(chat) = selected_chat {
                                let snapshot = agent.snapshot(chat.into()).await.map_err(|error|error.to_string())?;
                                let catalog = ChangeCatalog::load(&snapshot).await;
                                    outln!(console, "Diff sources: {} (Agent changes saved in this process)", catalog.sources.len());
                                    for source in catalog.sources {
                                        outln!(console, "Source: {:?} · {:?}", source.id, source.state);
                                        for file in source.files {
                                            let stats = file.summary.map_or_else(|| "unavailable".into(), |summary| format!("+{} -{}",summary.added_lines,summary.removed_lines));
                                            outln!(console, "  {:?} · {} · {stats}",file.action,safe_text(&file.path.to_string_lossy()));
                                        }
                                    }
                            } else {
                                outln!(console, "No Chat is selected. Enter a task first.");
                            }
                        },
                        "/settings" => {
                            menus.settings(agent,console,ConfigTarget::visible(selected_chat.map(Into::into))).await?;
                        },
                        "/auto-mode-setup" => {
                            if let Some(chat) = selected_chat {
                                match agent.permissions_view(chat.into()).await {
                                    Ok(view) => { console.show_permissions(view, agent.approval_history_view(chat.into()).await.map_err(|error| error.to_string()), false, true)?; console.open_environment_setup()?; }
                                    Err(error) => outln!(console, "Cannot open Environment setup: {}", safe_text(&error.to_string())),
                                }
                            } else { outln!(console, "Enter a task before generating an Environment draft."); }
                        },
                        command if command == "/sandbox" || command.starts_with("/sandbox ") => {
                            if let Err(error) = menus.sandbox_command(agent, console, ConfigTarget::visible(selected_chat.map(Into::into)), command).await {
                                outln!(console, "Cannot open Sandbox: {}", safe_text(&error));
                            }
                        },
                        "/permissions" => {
                            if console.is_interactive() {
                                if let Err(error)=menus.permission_modes(agent,console,ConfigTarget::visible(selected_chat.map(Into::into)),false).await {
                                    outln!(console,"Cannot open Permissions: {}.",safe_text(&error));
                                }
                            } else {
                                outln!(console, "Permissions view requires a terminal; use /rules to list rules.");
                            }
                        },
                        "/rules" => {
                            let Some(chat)=selected_chat else {
                                match agent.saved_project_rules(&global.projection().dispatch_directory) {
                                    Ok(mut rules) => {
                                        rules.extend(agent.staged_rules());
                                        outln!(console,"Permission rules for the New Agent (deny takes precedence over ask and allow):\n{}",safe_text(&serde_json::to_string_pretty(&rules).map_err(|_|"cannot render permission rules")?));
                                    },
                                    Err(error) => outln!(console,"Cannot list permission rules: {}",safe_text(&error.to_string())),
                                }
                                continue;
                            };
                            match agent.permissions_view(chat.into()).await {
                                Ok(view) => {
                                    let rules=view.rules.into_iter().filter(|entry| matches!(entry.source,tickloop_harness::protocol::PermissionRuleSource::ChatSession | tickloop_harness::protocol::PermissionRuleSource::ProjectUser)).map(|entry|entry.rule).collect::<Vec<_>>();
                                    outln!(console,"Permission rules (deny takes precedence over ask and allow):\n{}",safe_text(&serde_json::to_string_pretty(&rules).map_err(|_|"cannot render permission rules")?));
                                },
                                Err(error) => outln!(console,"Cannot list permission rules: {}",safe_text(&error.to_string())),
                            }
                        },
                        "" => {},
                        command if is_branch_command(command) => {
                            let preserve_until_summary_finishes=selected_chat
                                .is_some_and(|chat| directed_summaries.contains(chat));
                            match branch_from_terminal(console,agent,selected_chat,command,preserve_until_summary_finishes).await {
                                Ok(outcome) => {
                                    if menus.model_setup_applies_to(Some(outcome.source_chat_id.into())) {
                                        menus.model_setup_created_chat(outcome.child_chat_id.into());
                                    }
                                    menus.dismiss(console)?;
                                    outln!(console,"Branched conversation \"{}\". You are now in the new Chat {}. Use /resume {} to return to the original.",safe_line(&outcome.name),outcome.child_chat_id.public(),shown_id(outcome.source_chat_id));
                                    workflows::write_public_history(console, &outcome.snapshot)?;
                                    selected_chat=Some(outcome.child_chat_id);
                                    message_groups=MessageGroups::default();
                                    displayed_sequences.insert(outcome.child_chat_id.into(),outcome.snapshot.last_sequence);
                                    tools.restore_snapshot(&outcome.snapshot);
                                    sync_snapshot(&outcome.snapshot,&mut pending,&mut permissions,&mut questions);
                                    console.scroll_conversation_to_end()?;
                                    status_dirty=true;
                                },
                                Err(message) => {
                                    if console.write(&format!("{message}\n")).is_err() {
                                        return Err(message);
                                    }
                                },
                            }
                        },
                        _ if line=="/mode" || line.starts_with("/mode ") => {
                            let target=ConfigTarget::visible(selected_chat.map(Into::into));
                            if let Some(mode) = line.strip_prefix("/mode ") {
                                match parse_execution_mode(mode) {
                                    Ok((mode,warning)) => {
                                        let change=agent.change_configuration(target,ConfigurationChange::ExecutionMode {mode}).await.map_err(|error|error.to_string())?;
                                        if let Some(warning)=warning {outln!(console,"Warning: {warning}");}
                                        outln!(console, "Execution mode: {mode}. Explicit deny rules remain effective.{}",change.staged_note());
                                    },
                                    Err(error) => outln!(console, "Cannot set execution mode: {}.",safe_text(&error)),
                                }
                            } else { outln!(console, "Execution mode: {}",agent.configuration(target).await.map_err(|error|error.to_string())?.execution_mode); }
                            status_dirty=true;
                        },
                        _ if line=="/provider" => {
                            if let Err(error) = menus.providers(agent, console, ConfigTarget::visible(selected_chat.map(Into::into))).await {
                                outln!(console,"Cannot open Provider directory: {}",safe_text(&error));
                            }
                        },
                        "/model" => {
                            if let Err(error) = menus.models(agent,console,ConfigTarget::visible(selected_chat.map(Into::into))).await { outln!(console,"Cannot open Models: {}",safe_text(&error)); }
                        },
                        _ if line.starts_with("/model ") => { outln!(console,"Model files are not supported; use /model to choose from the built-in catalog."); },
                        _ if line=="/thinking" || line.starts_with("/thinking ") => {
                            let target=ConfigTarget::visible(selected_chat.map(Into::into));
                            if let Some(level) = line.strip_prefix("/thinking ") {
                                let level = level.trim();
                                let configuration = agent.configuration(target).await.map_err(|error| error.to_string())?;
                                let supported = ReasoningEffort::choices(&configuration.model);
                                let effort = if level == "auto" {
                                    Some(None)
                                } else {
                                    supported
                                        .iter()
                                        .copied()
                                        .find(|effort| effort.label() == level)
                                        .map(Some)
                                };
                                if let Some(effort) = effort {
                                    match agent.change_configuration(target, ConfigurationChange::ReasoningEffort {effort}).await {
                                        Ok(change) => outln!(console, "Effort: {}{}", effort.map_or("auto", ReasoningEffort::label), change.staged_note()),
                                        Err(error) => outln!(console, "Cannot set Effort: {}", safe_text(&error.to_string())),
                                    }
                                } else {
                                    let levels = supported.iter().map(|effort| effort.label()).collect::<Vec<_>>().join(", ");
                                    if levels.is_empty() {
                                        outln!(console, "{} does not support a configurable effort level.", safe_text(&configuration.model.model));
                                    } else {
                                        outln!(console, "Effort levels for {}: {}. Use auto for the model default; use /settings to toggle Thinking.", safe_text(&configuration.model.model), levels);
                                    }
                                }
                            } else {
                                if let Err(error) = menus.efforts(agent, console, target).await {
                                    outln!(console, "Cannot open Thinking: {}", safe_text(&error));
                                }
                            }
                        },
                        _ if line == "/websearch" => {
                            let target = ConfigTarget::visible(selected_chat.map(Into::into));
                            if let Err(error) = Box::pin(menus.search_settings(agent, console, target, false)).await {
                                outln!(console, "Cannot open Web search: {}", safe_text(&error));
                            }
                        },
                        _ if line.starts_with("/websearch ") => outln!(console, "Use /websearch to choose and configure a search source with the keyboard."),
                        _ if line=="/compact" || line.starts_with("/compact ") => {
                            if let Some(chat)=selected_chat {
                                let agent=agent.clone();
                                let instructions=line.strip_prefix("/compact").map(str::trim).filter(|instructions|!instructions.is_empty()).map(str::to_owned);
                                let task=compactions.spawn(async move {(chat,agent.compact_with_instructions(chat.into(),instructions).await)});
                                compaction_chats.insert(task.id(),(chat,None::<uuid::Uuid>));
                                outln!(console, "Compaction requested for Chat {}.", chat.public());
                            } else {outln!(console, "No Chat is selected.");}
                        },
                        _ if line=="/tui" || line.starts_with("/tui ") => {
                            match tui_renderer_request(&line) {
                                Ok(None) => match console.renderer() {
                                    Ok(Some(renderer)) => outln!(console, "Current renderer: {}",renderer.name()),
                                    Ok(None) => outln!(console, "{RENDERER_NEEDS_A_TERMINAL}"),
                                    Err(error) => outln!(console, "Cannot read the current renderer: {}",safe_text(&error)),
                                },
                                Ok(Some(target)) => match console.switch_renderer(target) {
                                    Ok(RendererSwitch::Switched) => outln!(console, "Switched to the {} renderer.",target.name()),
                                    Ok(RendererSwitch::AlreadyActive) => outln!(console, "Already using the {} renderer.",target.name()),
                                    Ok(RendererSwitch::NotInteractive) => outln!(console, "{RENDERER_NEEDS_A_TERMINAL}"),
                                    Err(error) => outln!(console, "Cannot switch renderer: {}",safe_text(&error)),
                                },
                                Err(error) => outln!(console, "{error}"),
                            }
                        },
                        _ if line=="/cwd" || line.starts_with("/cwd ") || line.starts_with("/add-dir ") => {
                            let Some(chat)=selected_chat else {
                                match stage_draft_directories(agent,&mut global,&line).await {
                                    Ok((directory,additional)) => {
                                        console.set_input_context(None,&directory,&additional)?;
                                        show_directory_listing(console, agent, ConfigTarget::NewDraft, &directory, &additional).await?;
                                    },
                                    Err(error) => outln!(console, "Cannot update directories: {}",safe_text(&error)),
                                }
                                continue;
                            };
                            let mut snapshot = agent.snapshot(chat.into()).await.map_err(|error|error.to_string())?;
                            let Some(mut folder) = snapshot.folder.clone() else {
                                outln!(console, "This Agent has no working directory");
                                continue;
                            };
                            if line=="/cwd" {
                                show_directory_listing(console, agent, ConfigTarget::Agent(chat.into()), &folder, &snapshot.additional_directories).await?;
                                continue;
                            }
                            if let Some(path) = line.strip_prefix("/cwd ") { folder = folder.join(path.trim()); }
                            if let Some(path) = line.strip_prefix("/add-dir ") { snapshot.additional_directories.push(folder.join(path.trim())); }
                            match agent.set_directories(chat.into(),folder,snapshot.additional_directories).await {
                                Ok(()) => {
                                    let snapshot = agent.snapshot(chat.into()).await.map_err(|error|error.to_string())?;
                                    show_directory_listing(console, agent, ConfigTarget::Agent(chat.into()), shown_folder(snapshot.folder.as_deref()), &snapshot.additional_directories).await?;
                                    show_settings_warnings(console, agent.user_settings_warnings(chat.into()))?;
                                },
                                Err(error) => outln!(console, "Cannot update directories: {}",safe_text(&error.to_string())),
                            }
                        },
                        _ if line.starts_with("/acknowledge-create ") || line.starts_with("/acknowledge-input ") => {
                            let (command,argument)=line.split_once(' ').unwrap_or((line.as_str(),""));
                            let Some(id)=tickloop_ids::base62::decode_either(argument.trim()) else {
                                outln!(console,"Usage: {command} COMMAND_ID");
                                continue;
                            };
                            let message=if command=="/acknowledge-input" {
                                if agent.acknowledge_input(id) {format!("Input {} is no longer tracked; nothing was resent",shown_id(id))} else {format!("No input {} with an unknown acceptance result",shown_id(id))}
                            } else {
                                match agent.acknowledge_first_create(id) {
                                    Some(CreateAcknowledgement::DraftRestored)=>format!("Creation {} is no longer tracked; nothing was resent. If it created an Agent, that Agent appears in Agents; the next input creates a new Agent",shown_id(id)),
                                    Some(CreateAcknowledgement::ChoicesCancelled)=>format!("Choices not yet applied after {} are cancelled; nothing was resent. The Agent keeps the choices already applied and now takes input",shown_id(id)),
                                    None=>format!("No unknown or failed New Agent command {}",shown_id(id)),
                                }
                            };
                            outln!(console,"{message}");
                        },
                        _ if line.starts_with("/rule ") => {
                            let result=match selected_chat {
                                Some(chat)=>update_rule(agent,chat,&line).await,
                                None=>stage_draft_rule(agent,&global.projection().dispatch_directory,&line),
                            };
                            match result {Ok(message)=>outln!(console, "{message}"),Err(error)=>outln!(console, "Cannot update rule: {}",safe_text(&error)),}
                        },
                        _ if line.starts_with("/background ") || line.starts_with("/stop-command ") || line.starts_with("/command-output ") => {
                            if let Some(chat)=selected_chat {
                                if let Err(error)=command_control(console, agent,chat,&line).await {outln!(console, "Command control failed: {}",safe_text(&error));}
                            } else {outln!(console, "No Chat is selected.");}
                        },
                        "/operations" => {
                            if let Some(chat)=selected_chat {
                                let snapshot=agent.snapshot(chat.into()).await.map_err(|error|error.to_string())?;
                                outln!(console, "Operations recorded in this process: {}",snapshot.operations.len());
                                for operation in snapshot.operations {
                                    outln!(console, "Operation: {} · {:?}\n{} · {}",shown_id(operation.id),operation.state,safe_text(&operation.name),safe_text(&operation.input.to_string()));
                                    if let Some(result)=operation.result {outln!(console, "Result: {}",safe_text(&result.content));}
                                }
                            } else {outln!(console, "No Chat is selected.");}
                        },
                        "/interrupt" => {
                            if let Some(chat) = selected_chat {
                                let operations = agent.interrupt(chat.into()).await.map_err(|error|error.to_string())?;
                                outln!(console, "Interrupt requested for the current turn. Tick Subscriptions and Artifact feedback keep arriving.");
                                for operation in operations { outln!(console, "Operation {} remains {:?}: {}",shown_id(operation.id),operation.state,safe_text(&operation.name)); }
                            } else { outln!(console, "No Chat is selected."); }
                        },
                        _ if line == "/goal" || line.starts_with("/goal ") => {
                            let chat = ensure_chat_with_model_setup(console, agent, &mut menus, &mut selected_chat, &workdir).await?;
                            if let Err(error) = menus.goal_command(agent, console, chat, &line).await {
                                outln!(console, "{}", safe_text(&error));
                            }
                            status_dirty = true;
                        },
                        "/stop" => {
                            if let Some(chat) = selected_chat {
                                match agent.stop(chat.into()).await {
                                    Ok(operations) => {
                                        menus.reoffer_continue(chat.into());
                                        outln!(console, "{STOPPED_NOTICE}");
                                        for operation in operations { outln!(console, "Operation {} remains {:?}: {}",shown_id(operation.id),operation.state,safe_text(&operation.name)); }
                                    },
                                    Err(error) => outln!(console, "Cannot stop: {}",safe_text(&error.to_string())),
                                }
                            } else { outln!(console, "No Chat is selected."); }
                        },
                        _ if line.starts_with("/allow ") || line.starts_with("/deny ") => {
                            match answer_permission(agent,selected_chat,&line).await {
                                Ok(request) => {
                                    permissions.remove(&request);
                                    status_dirty=true;
                                    outln!(console, "Permission decision accepted.");
                                },
                                Err(error) => outln!(console, "Permission decision failed: {}", safe_text(&error)),
                            }
                        },
                        // A terminal answers questions only in their dialog; typed answers are for a View without one.
                        _ if line.starts_with("/answer ") && interactive => outln!(console, "Answer the question in its dialog."),
                        _ if line.starts_with("/answer ") => {
                            let request=line.trim_start_matches("/answer ").split_once(' ').ok_or("use /answer QUESTION ANSWER").and_then(|(id,answer)|{
                                let id=tickloop_ids::base62::decode_either(id).ok_or("not a question ID")?;
                                let request=questions.get(&id).cloned().ok_or("question expired")?;
                                Ok((request,answer.to_owned()))
                            });
                            match request {
                                Ok((request,answer)) => {
                                    answer_typed_question(agent,console,&mut menus,&mut questions,&request,&answer).await?;
                                },
                                Err(error) => outln!(console, "Cannot answer question: {}",safe_text(error)),
                            }
                        },
                        _ if line.starts_with("/resume ") => {
                            match open_chat(agent, &line, selected_chat).await {
                                Ok(snapshot) => {
                                    learn_agent_colors(console,agent,&snapshot).await?;
                                    show_opened_chat(console,agent,&snapshot)?;
                                    selected_chat=Some(snapshot.id);
                                    message_groups=MessageGroups::default();
                                    displayed_sequences.insert(snapshot.id.into(),snapshot.last_sequence);
                                    tools.restore_snapshot(&snapshot);
                                    sync_snapshot(&snapshot,&mut pending,&mut permissions,&mut questions);
                                },
                                Err(error) => outln!(console, "{}", safe_text(&error)),
                            }
                        },
                        _ if is_command => {
                            if let Some(usage) = completion::unhandled_builtin_usage(&line) {
                                console.restore_rejected_prompt(restored_contexts,&usage)?;
                                console.hold_status(&usage)?;
                                report_refused_input(console,&usage,&mut failed);
                                continue;
                            }
                            let command = line.trim_start_matches('/');
                            let (name, arguments) = command
                                .split_once(char::is_whitespace)
                                .map_or((command, ""), |(name, arguments)| (name, arguments.trim()));
                            let directory=global.projection().dispatch_directory;
                            if selected_chat.is_none() && let Err(error) = agent.check_new_agent_skill(&directory,name,arguments) {
                                if matches!(error, tickloop_code_settings::skills::SkillError::Unknown(_)) {
                                    outln!(console, "Unknown command; use /help.");
                                    console.restore_rejected_prompt(restored_contexts,"Unknown command; use /help")?;
                                } else {
                                    let error = safe_line(&error.to_string());
                                    console.restore_rejected_prompt(restored_contexts,&error)?;
                                    console.hold_status(&error)?;
                                    report_refused_input(console,&error,&mut failed);
                                }
                                continue;
                            }
                            let chat_id = match ensure_chat_for_input(console,agent,&mut selected_chat,&directory,retry_of,&original_input,&mut echo).await? {
                                Ok(chat)=>chat,
                                Err(error)=>{console.restore_rejected_prompt(restored_contexts,&error)?;report_refused_input(console,&error,&mut failed);status_dirty=true;continue;},
                            };
                            let sent=send_input(agent,chat_id.into(),retry_of,agent.invoke_skill(chat_id, name, arguments),submitted_receipt).await;
                            match sent {
                                InputSend::Accepted { value: receipt, adopted } => {
                                    echo=track_accepted_echo(console,agent,&receipt,echo).await?;
                                    merged_echoes.accepted(receipt.run_id,echo);
                                    suggestible_runs.insert(receipt.run_id.into());
                                    if !interactive {outln!(console, "Accepted: Run {}", receipt.run_id.public());}
                                    track_accepted_input(console,agent,receipt,adopted,&mut pending,&mut permissions,&mut questions).await?;
                                },
                                InputSend::Unknown(id, error) => {
                                    if let Some(echo)=echo {console.withdraw_echo(Some(chat_id),echo)?;}
                                    outln!(console, "Failed: {}", safe_text(&error.to_string()));
                                    console.restore_unknown_prompt(restored_contexts,&safe_line(&error.to_string()),Some(id))?;
                                    failed=true;
                                },
                                InputSend::Held(error) => {
                                    if retry_of.is_none() && let Some(echo)=echo {console.withdraw_echo(Some(chat_id),echo)?;}
                                    outln!(console, "Failed: {}", safe_text(&error.to_string()));
                                    console.restore_unknown_prompt(restored_contexts,&safe_line(&error.to_string()),None)?;
                                    failed=true;
                                },
                                InputSend::Failed(error) => outln!(console, "Unknown or unavailable Skill: {}", safe_text(&error.to_string())),
                            }
                        },
                        _ => {
                            let visible=questions.values().filter(|request|!interactive && Some(request.chat_id)==selected_chat).cloned().collect::<Vec<_>>();
                            if !visible.is_empty() {
                                if let [request]=visible.as_slice() {
                                    answer_typed_question(agent,console,&mut menus,&mut questions,request,&original_input).await?;
                                } else {outln!(console, "More than one question is pending; use /answer QUESTION ANSWER.");}
                                continue;
                            }
                            let directory=global.projection().dispatch_directory;
                            let chat_id = match ensure_chat_for_input(console,agent,&mut selected_chat,&directory,retry_of,&original_input,&mut echo).await? {
                                Ok(chat)=>chat,
                                Err(error)=>{
                                    if let Some(entry)=queue_entry {console.withdraw_queued_input(entry)?;}
                                    console.restore_rejected_prompt(restored_contexts,&error)?;report_refused_input(console,&error,&mut failed);status_dirty=true;continue;
                                },
                            };
                            if record_after_chat {
                                console.record_prompt_history(&original_input)?;
                            }
                            // Pasted images stay with the editor's paste blocks, so a refused
                            // prompt gets back only the other saved attachments.
                            let submitted_contexts = restored_contexts.clone();
                            let contexts = pasted_images
                                .into_iter()
                                .map(InputContext::Image)
                                .chain(restored_contexts)
                                .collect::<Vec<_>>();
                            let submission = agent.submit_with_input_contexts(
                                chat_id,
                                original_input,
                                contexts,
                                file_references,
                            );
                            match send_input(agent,chat_id.into(),retry_of,submission,submitted_receipt).await {
                                InputSend::Accepted { value: receipt, adopted } => {
                                    if let Some(guard) = &_delayed_guard { guard.mark_accepted(); }
                                    suggestible_runs.insert(receipt.run_id.into());
                                    echo=track_accepted_echo(console,agent,&receipt,echo).await?;
                                    merged_echoes.accepted(receipt.run_id,echo);
                                    if !interactive {outln!(console, "Accepted: Run {}", receipt.run_id.public());}
                                    let run_id=receipt.run_id;
                                    track_accepted_input(console,agent,receipt,adopted,&mut pending,&mut permissions,&mut questions).await?;
                                    if send_now && let Err(error)=agent.send_now(chat_id,run_id.into()).await {
                                        console.set_status(&format!("Cannot send now: {}",safe_line(&error.to_string())))?;
                                    }
                                },
                                InputSend::Unknown(id, error) => {
                                    console.restore_unknown_prompt(submitted_contexts,&safe_line(&error.to_string()),Some(id))?;
                                    if let Some(echo)=echo {console.withdraw_echo(Some(chat_id),echo)?;}
                                    if let Some(entry)=queue_entry {console.withdraw_queued_input(entry)?;}
                                    outln!(console, "Failed: {}", safe_text(&error.to_string()));
                                    failed=true;
                                },
                                InputSend::Held(error) => {
                                    if retry_of.is_none() && let Some(echo)=echo {console.withdraw_echo(Some(chat_id),echo)?;}
                                    if let Some(entry)=queue_entry {console.withdraw_queued_input(entry)?;}
                                    outln!(console, "Failed: {}", safe_text(&error.to_string()));
                                    console.restore_unknown_prompt(submitted_contexts,&safe_line(&error.to_string()),None)?;
                                    failed=true;
                                },
                                InputSend::Failed(error) => {
                                    if let Some(entry)=queue_entry {console.withdraw_queued_input(entry)?;}
                                    outln!(console, "Failed: {}", safe_text(&error.to_string())); failed=true;
                                },
                            }
                        },
                    }
                }
                Some(InputEvent::CycleExecutionMode) => {
                    let target=ConfigTarget::visible(selected_chat.map(Into::into));
                    // The overview shows no Agent; `selected_chat` there is the hidden one.
                    let hidden=overview_active.then(||completion::unavailable_command("/mode",completion::CommandScope::Overview)).flatten();
                    let result=if let Some((_,reason))=hidden {Err(completion::command_refusal(completion::CommandScope::Overview,"shift+tab mode",reason))} else {match agent.configuration(target).await {
                        Ok(configuration)=>{
                            match configuration.execution_mode.next_in(&console.shift_tab_modes()?) {
                                None=>Err(format!("shift+tab only cycles {}; choose more modes in /settings",configuration.execution_mode)),
                                Some(mode)=>{
                                    let change=ConfigurationChange::ExecutionMode {mode};
                                    match configuration.unavailable_change(&change) {
                                        Some(reason)=>Err(completion::command_refusal(if configuration.delegated {completion::CommandScope::Child} else {completion::CommandScope::Root},"shift+tab mode",reason)),
                                        None=>agent.change_configuration(target,change).await.map(|_|mode).map_err(|error|format!("Cannot change execution mode: {}",safe_text(&error.to_string()))),
                                    }
                                }
                            }
                        },
                        Err(error)=>Err(format!("Cannot read execution mode: {}",safe_text(&error.to_string()))),
                    }};
                    match result {
                        // The confirmed mode is the only status that changed; a status refresh
                        // would read the whole Chat again before the mode could show.
                        Ok(mode)=>console.show_execution_mode(mode)?,
                        Err(message)=>{console.hold_status(&message)?;status_dirty=true;},
                    }
                }
            }},
            event = next_event(&mut events, &mut feed_gaps, &mut deferred_events, visual_pacing_pending) => {
                if matches!(&event, Ok(event) if Some(event.chat_id) == selected_chat && matches!(event.kind, EventKind::Started | EventKind::InputMerged { .. } | EventKind::ConversationRewound { .. })) {
                    console.clear_prompt_suggestion()?;
                }
                let event = if visual_pacing_pending {
                    match event {
                        Ok(queued_event) if !overview_active && Some(queued_event.chat_id) == selected_chat => {
                            deferred_events.push_back(queued_event);
                            continue;
                        }
                        other => other,
                    }
                } else {
                    event
                };
                if merged_echoes_view!=selected_chat {
                    merged_echoes.close_all();
                    merged_echoes_view=selected_chat;
                }
                match &event {
                    Ok(event) => {
                        if matches!(event.kind,EventKind::Started) && event.sequence>*displayed_sequences.get(&event.chat_id.uuid()).unwrap_or(&0) {
                            pending.entry(event.run_id.into()).or_insert(Receipt {chat_id:event.chat_id,loop_id:event.loop_id,run_id:event.run_id});
                            observed_run_starts.entry(event.run_id.into()).or_insert_with(std::time::Instant::now);
                            #[cfg(test)]
                            agent.record_recovery_fact(recovery_tests::RecoveryFact::LiveRunStarted {chat:event.chat_id.into(),run:event.run_id.into(),overview:overview_active});
                        }
                        if matches!(event.kind,EventKind::Started|EventKind::InputMerged {..}) {
                            let echo=show_consumed_input(console,agent,&mut turn_snapshot,event).await?;
                            merged_echoes.consumed_echo(event.run_id,echo);
                        }
                        if let Some((previous,current))=merged_echoes.observe(event,displayed_sequences.get(&event.chat_id.uuid()).copied().unwrap_or(0)) {console.join_inputs(event.chat_id.into(),previous,current)?;}
                        #[cfg(test)]
                        if let EventKind::InputMerged { into_run_id } = &event.kind {
                            agent.record_recovery_fact(recovery_tests::RecoveryFact::MergeObserved {chat:event.chat_id.into(),run:event.run_id.into(),sequence:event.sequence,into_run:(*into_run_id).into()});
                        }
                    },
                    Err(_) => merged_echoes.close_all(),
                }
                if let Ok(event) = &event {
                    match &event.kind {
                        EventKind::PermissionRequested { request } => menus.note_request(event.chat_id.into(), request.id),
                        EventKind::QuestionAsked { request } => menus.note_request(event.chat_id.into(), request.id),
                        _ => {}
                    }
                    match &event.kind {
                        EventKind::WaitingForMcpServers { servers } if !servers.is_empty() => {
                            awaited_mcp_servers.insert(event.run_id.uuid(), servers.clone());
                        }
                        EventKind::WaitingForMcpServers { .. }
                        | EventKind::ResponseStarted { .. }
                        | EventKind::Completed { .. }
                        | EventKind::Failed(_)
                        | EventKind::Interrupted => {
                            awaited_mcp_servers.remove(&event.run_id.uuid());
                        }
                        _ => {}
                    }
                    transcript_refresh_due |= Some(event.chat_id) == selected_chat;
                }
                match event {
                Ok(event) if !overview_active && (pending.contains_key(&event.run_id.uuid()) || Some(event.chat_id)==selected_chat) => {
                    // Only a run that took in a family message has one to show from the conversation.
                    if Some(event.chat_id)==selected_chat && matches!(event.kind,EventKind::Started|EventKind::InputMerged {..})
                        && turn_snapshot.status(agent,event.chat_id).await.is_ok_and(|status|status.family_input_runs.contains(&event.run_id))
                        && let Ok(snapshot)=turn_snapshot.read(agent,event.chat_id).await {
                        learn_agent_colors(console,agent,&snapshot).await?;
                        show_consumed_family_input(console,&snapshot,event.run_id)?;
                    }
                    if event.sequence<=*displayed_sequences.get(&event.chat_id.uuid()).unwrap_or(&0) {
                        #[cfg(test)]
                        agent.record_recovery_fact(recovery_tests::RecoveryFact::Skipped {
                            chat: event.chat_id.into(),
                            run: event.run_id.into(),
                            sequence: event.sequence,
                            completed: matches!(event.kind, EventKind::Completed { .. }),
                        });
                        continue;
                    }
                    let was_thinking = thinking_runs.contains(&event.run_id);
                    match &event.kind {
                        EventKind::Thinking(thinking) => {
                            thinking_runs.insert(event.run_id);
                            if interactive && Some(event.chat_id) == selected_chat && !unpaced_runs.contains(&tickloop_ids::Id::from(event.run_id.uuid())) {
                                let preview = thinking_previews.entry(event.run_id.into()).or_default();
                                preview.append(thinking);
                                thinking_frame_due = true;
                            }
                        },
                        EventKind::Text(_)
                        | EventKind::AssistantMessageCommitted { .. }
                        | EventKind::ToolStarted { .. }
                        | EventKind::PermissionRequested { .. }
                        | EventKind::QuestionAsked { .. }
                        | EventKind::ModelRetry { .. }
                        | EventKind::Completed { .. }
                        | EventKind::Failed(_)
                        | EventKind::Interrupted => { thinking_runs.remove(&event.run_id); },
                        _ => {},
                    }
                    let stopped_thinking = was_thinking && !thinking_runs.contains(&event.run_id);
                    if stopped_thinking {
                        thinking_previews.remove(&event.run_id.uuid());
                    }
                    status_dirty |= !matches!(&event.kind, EventKind::Text(_) | EventKind::Thinking(_) | EventKind::ToolUseStarted { .. }) || stopped_thinking;
                    track_run_requests(&event,&mut pending,&mut permissions,&mut questions);
                    if matches!(&event.kind,EventKind::Failed(_)) {
                        failed=true;
                        if let Ok(snapshot) = turn_snapshot.read(agent, event.chat_id).await {
                            menus.invalidate_failed_directory(&snapshot, event.run_id.into());
                        }
                    }
                    if matches!(&event.kind,EventKind::Started) {
                        working_shimmer.reset();
                    }
                    if matches!(&event.kind, EventKind::ConversationRewound { .. }) {
                        if Some(event.chat_id) != selected_chat {
                            continue;
                        }
                        let snapshot = turn_snapshot
                            .read(agent, event.chat_id)
                            .await
                            .map_err(|error| error.to_string())?;
                        displayed_sequences.insert(event.chat_id.into(), snapshot.last_sequence);
                        tools.restore_snapshot(&snapshot);
                        message_groups = MessageGroups::default();
                        thinking_runs.clear();
                        thinking_previews.clear();
                        reply_lines.clear();
                        deferred_events.clear();
                        sync_snapshot(
                            &snapshot,
                            &mut pending,
                            &mut permissions,
                            &mut questions,
                        );
                        console.reset_chat(event.chat_id.into())?;
                        learn_agent_colors(console, agent, &snapshot).await?;
                        workflows::write_public_history(console, &snapshot)?;
                        outln!(
                            console,
                            "Conversation restored. ctrl+o opens the detailed transcript."
                        );
                        continue;
                    }
                    if Some(event.chat_id)!=selected_chat {
                        announce_background_event(console,&event,&background_label(agent,console,event.chat_id),&mut background_notice)?;
                        continue;
                    }
                    let message_id = message_groups.message_id(&event);
                    let starts_assistant_message = matches!(&event.kind, EventKind::Text(_))
                        && message_id
                            .as_ref()
                            .is_some_and(|id| message_groups.displayed.insert(id.clone()));
                    let starts_thinking = matches!(&event.kind, EventKind::Thinking(_))
                        && message_id
                            .as_ref()
                            .is_some_and(|id| message_groups.displayed.insert(id.clone()));
                    let follows_thinking = starts_assistant_message
                        && message_id.as_ref().is_some_and(|id| {
                            message_groups.displayed
                                .contains(&id.replace(":assistant:", ":thinking:"))
                        });
                    displayed_sequences.insert(event.chat_id.into(),event.sequence);
                    let console = &console.for_message(message_id.clone());
                    let result_source = tools.result_source(&event);
                    let conversation = if result_source == presentation::ResultSource::Conversation || matches!(&event.kind, EventKind::FamilyRunCompleted { .. }) {
                        turn_snapshot.read(agent, event.chat_id).await.ok()
                    } else {
                        None
                    };
                    if let Some(call_id) = tools.nested_call_to_learn(&event)
                        && let Ok(Some(nested)) = agent.operation(event.chat_id.into(), event.run_id.into(), call_id).await {
                        tools.learn_nested_call(&event, &nested);
                    }
                    let operation = match &event.kind {
                        EventKind::ToolCompleted { call_id, .. } if result_source == presentation::ResultSource::Operation => {
                            agent.operation(event.chat_id.into(), event.run_id.into(), call_id).await.ok().flatten()
                        },
                        _ => None,
                    };
                    if let Some(snapshot) = &conversation {
                        learn_agent_colors(console, agent, snapshot).await?;
                    }
                    // The event's writes reach the terminal as one frame. The hold ends before
                    // anything is awaited, so keys typed meanwhile still draw at once.
                    let mut event_frame = Some(console.hold_frames()?);
                    if interactive && matches!(&event.kind,
                        EventKind::Thinking(_)
                        | EventKind::AssistantMessageCommitted { .. }
                        | EventKind::ToolStarted { .. }
                        | EventKind::PermissionRequested { .. }
                        | EventKind::QuestionAsked { .. }
                        | EventKind::ModelRetry { .. }
                        | EventKind::Completed { .. }
                        | EventKind::Failed(_)
                        | EventKind::Interrupted
                    )
                        && let Some(mut reply) = reply_lines.remove(&event.run_id.uuid()) {
                            reply.finish();
                            write_pending_reply_lines(console, &mut reply)?;
                    }
                    if interactive && stopped_thinking {
                        console.write_pieces(&[presentation::thinking_label()])?;
                    }
                    let interrupted_by_user = matches!(&event.kind,EventKind::Completed {..} | EventKind::Failed(_) | EventKind::Interrupted)
                        && user_interrupted_runs.remove(&tickloop_ids::Id::from(event.run_id.uuid()))
                        && matches!(&event.kind,EventKind::Interrupted);
                    if matches!(&event.kind,EventKind::Completed {..} | EventKind::Failed(_) | EventKind::Interrupted) {
                        unpaced_runs.remove(&tickloop_ids::Id::from(event.run_id.uuid()));
                    }
                    let facts = match &operation {
                        Some(operation) => presentation::CallFacts { review: presentation::operation_review(operation), report: None },
                        None => conversation.as_ref().map(|snapshot| tools.call_facts(&event, snapshot)).unwrap_or_default(),
                    };
                    let interrupted_by_rejection = tools.ended_with_rejection(event.run_id.into());
                    if tools.present_event(console,&event,&facts)? { continue; }
                    match event.kind {
                        EventKind::FamilyInputUpdated { record } => console.write(&presentation::family_input_text(&record, presentation::OutputDetail::Preview))?,
                        EventKind::FamilyRunCompleted { completion } => {
                            if let Some(snapshot) = &conversation {
                                show_family_completion(console, &completion, snapshot)?;
                            }
                        },
                        EventKind::Thinking(thinking) => {
                            if !interactive {
                                if starts_thinking {
                                    console.write("\nThinking: ")?;
                                }
                                console.write(&safe_text(&thinking))?;
                            }
                        },
                        EventKind::Text(text) => {
                            if interactive {
                                let reply = reply_lines.entry(event.run_id.into()).or_insert_with(|| ReplyLines::new(event.chat_id.into(), message_id));
                                reply.push(&safe_text(&text));
                                #[cfg(test)]
                                agent.record_recovery_fact(recovery_tests::RecoveryFact::Buffered {
                                    chat: event.chat_id.into(),
                                    run: event.run_id.into(),
                                    sequence: event.sequence,
                                    complete_line: reply.has_complete_line(),
                                });
                                if unpaced_runs.contains(&tickloop_ids::Id::from(event.run_id.uuid())) {
                                    write_pending_reply_lines(console, reply)?;
                                } else {
                                    write_next_reply_line(console, reply)?;
                                }
                                #[cfg(test)]
                                {
                                    event_frame.take();
                                    agent.recovery_checkpoint().await;
                                }
                            } else {
                                if follows_thinking {console.write("\nAssistant: ")?;}
                                console.write_markdown(&safe_text(&text), starts_assistant_message)?;
                            }
                        },
                        EventKind::Completed { .. } => {
                            #[cfg(test)]
                            agent.record_recovery_fact(recovery_tests::RecoveryFact::Completed {chat:event.chat_id.into(),run:event.run_id.into(),sequence:event.sequence});
                            event_frame.take();
                            let details = if interactive {
                                let shells = task_center.snapshot(event.chat_id.into()).await.ok().map(|snapshot| snapshot.tasks.into_iter().filter(|task| matches!(task.state, ShellTaskState::Running | ShellTaskState::Background | ShellTaskState::Cancelling)).count()).unwrap_or_default();
                                Some(completion_details(event.run_id, observed_run_starts.get(&event.run_id.uuid()).map(std::time::Instant::elapsed), shells))
                            } else { None };
                            let debug = if console.shows_debug_details()?
                                && let Ok(snapshot) = turn_snapshot.read(agent, event.chat_id).await
                            {
                                completion_debug(console, &snapshot, event.run_id)?
                            } else { None };
                            write_completion_notice(console, details.as_ref(), debug.as_ref())?;
                            if suggestible_runs.remove(&event.run_id.uuid()) && interactive {
                                if let Ok(status) = turn_snapshot.status(agent, event.chat_id).await {
                                    if let Some(reason) = prompt_suggestion::suppression(&status) {
                                        suggestion_diagnostics.insert(event.chat_id.into(), (event.run_id.into(), reason));
                                    } else if let Some(request) = console.begin_prompt_suggestion(event.chat_id.into())? {
                                        suggestion_diagnostics.insert(event.chat_id.into(), (request.id, "generating"));
                                        let models = suggestion_models.clone();
                                        // A suggestion is drawn from the conversation it follows.
                                        let conversation = turn_snapshot.read(agent, event.chat_id).await;
                                        suggestions.spawn(async move {
                                            let result = match conversation {
                                                Ok(snapshot) => prompt_suggestion::generate(&request, &snapshot, models.as_ref()).await,
                                                Err(_) => prompt_suggestion::SuggestionResult::empty("suppressed: Agent unavailable"),
                                            };
                                            (request, result)
                                        });
                                    } else {
                                        let reason = console.prompt_suggestion_suppression()?.unwrap_or("suppressed: Agent View changed");
                                        suggestion_diagnostics.insert(event.chat_id.into(), (event.run_id.into(), reason));
                                    }
                                } else {
                                    suggestion_diagnostics.insert(event.chat_id.into(), (event.run_id.into(), "suppressed: Agent unavailable"));
                                }
                            }
                        },
                        EventKind::Failed(error) => {
                            if suggestible_runs.remove(&event.run_id.uuid()) {
                                suggestion_diagnostics.insert(event.chat_id.into(), (event.run_id.into(), "suppressed: failed turn"));
                            }
                            let message = if interactive {
                                format!(
                                    "{} Failed: {}",
                                    console.display_profile()?.glyph(Glyph::Failure),
                                    safe_text(&error)
                                )
                            } else {
                                format!("Failed: {}", safe_text(&error))
                            };
                            console.write_roles(&[
                                ("\n".into(), None),
                                (message, Some(Role::Error)),
                                ("\n".into(), None),
                            ])?;
                        },
                        EventKind::ModelRetry { kind, error, attempt, max_retries, delay_ms } => {
                            let (failure, retry) =
                                model_retry_parts(kind, &error, attempt, max_retries, delay_ms);
                            console.write_model_retry(event.run_id.into(), kind, attempt, &[
                                (failure, Some(Role::Error)),
                                (retry, Some(Role::MutedText)),
                            ])?;
                        },
                        EventKind::Interrupted => {
                            if suggestible_runs.remove(&event.run_id.uuid()) {
                                suggestion_diagnostics.insert(event.chat_id.into(), (event.run_id.into(), "suppressed: interrupted turn"));
                            }
                            event_frame.take();
                            if interactive {
                                // A declined call's result already says the run stopped there.
                                if !interrupted_by_rejection {
                                    console.write_response(&[(format!("{}\n", presentation::interrupted_notice(console.display_profile()?)), Some(Role::MutedText))])?;
                                }
                                // Taking the prompt back changes the Chat this turn has read.
                                if interrupted_by_user {
                                    turn_snapshot.forget();
                                }
                                if interrupted_by_user
                                    && let Some(restored)=take_back_interrupted_prompt(agent,console,event.chat_id,event.run_id).await?
                                {
                                    displayed_sequences.insert(restored.id.into(),restored.last_sequence);
                                    tools.restore_snapshot(&restored);
                                    sync_snapshot(&restored,&mut pending,&mut permissions,&mut questions);
                                }
                            } else {
                                outln!(console, "\nInterrupted run {}",event.run_id.public());
                            }
                        },
                        EventKind::InputMerged { into_run_id } => {
                            if !interactive {
                                outln!(console, "Queued input joined run {}.", into_run_id.public());
                            }
                        },
                        EventKind::InputTakenBack | EventKind::ResponseStarted { .. } | EventKind::ToolUseStarted { .. } | EventKind::WaitingForMcpServers { .. } => {},
                        EventKind::ConversationRewound { .. } => unreachable!(),
                        EventKind::Started => {
                            pending.entry(event.run_id.into()).or_insert(Receipt {chat_id:event.chat_id,loop_id:event.loop_id,run_id:event.run_id});
                            if interactive {
                                console.hide_completion_notice()?;
                            } else {
                                event_frame.take();
                                let status = turn_snapshot.status(agent, event.chat_id).await.map_err(|error|error.to_string())?;
                                outln!(console, "Run: {} · Running\nModel: {} · {:?}", event.run_id.public(),safe_text(&status.model.model),status.model.protocol);
                            }
                        },
                        // The terminal UI asks in its request dialog, outside the conversation.
                        EventKind::PermissionRequested {request} if !interactive => {
                            event_frame.take();
                            if tickloop_harness::tools::exits_plan_mode_without_plan(&request.name, &request.input) {outln!(console, "\nExit plan mode? Request: {}\nThe Agent wants to exit plan mode.\nUse /allow {0} to exit plan mode or /deny {0} to stay in plan mode.", shown_id(request.id));}
                            else if request.name == EXIT_PLAN_MODE_TOOL_NAME {menus.workflow(console,Workflow::plan(agent,event.chat_id.into()).await?)?;}
                            else if request.name==ENTER_PLAN_MODE_TOOL_NAME {outln!(console, "\nEnter plan mode? Request: {}\nThe Agent wants to enter plan mode to explore and design an implementation approach.\nUse /allow {0} to enter plan mode or /deny {0} to stay in the current mode.", shown_id(request.id));}
                            else {outln!(console, "\nPermission request: {0} · {1}\nOperation: {2}\nUse /allow {0} [once|session|persistent] or /deny {0}.", shown_id(request.id), safe_text(&request.name), safe_text(&request.input.to_string()));}
                        },
                        EventKind::PermissionRequested {..} => {},
                        EventKind::ApprovalAssessed { .. } | EventKind::ApprovalReviewStarted { .. } | EventKind::ApprovalRecorded { .. } => {},
                        EventKind::PermissionChecked {decision,reason} => {
                            if decision != Decision::Allow && (!interactive || decision != Decision::Deny) {
                                outln!(console, "\nAuto review: {decision:?} · {}",safe_text(&reason));
                            }
                        },
                        EventKind::QuestionAsked {request} => print_question(console, &request)?,
                        EventKind::PlanUpdated {plan} => {
                            console.write_plan(&format!("Plan: {} · version {}",plan.loop_id.public(),plan.version),&safe_text(&plan.markdown))?;

                        },
                        EventKind::ProgressUpdated {progress} => {
                            if !interactive {
                                outln!(console, "\nProgress:");
                                for todo in progress.todos {outln!(console, "  {:?}: {}",todo.status,safe_text(&todo.content));}
                            }
                        },
                        EventKind::ScheduledTasksChanged { notice: Some(tickloop_harness::protocol::ScheduledTaskNotice::WakeupCancelled { .. }) } => {
                            outln!(console, "\nPending wakeup cancelled");
                        },
                        EventKind::GoalUpdated {goal, notice} => {
                            if let Some(notice) = notice { console.write_pieces(&goal_view::notice_pieces(&notice, goal.as_ref().map_or("", |goal| goal.objective.as_str())))?; }
                        },
                        EventKind::HookRan {record} => {
                            for (notice, role) in hook_activity::run_notices(&record) {
                                console.write_roles(&[("\n".into(), None), (safe_text(&notice), Some(role)), ("\n".into(), None)])?;
                            }
                        },
                        EventKind::HooksSkipped {notice} => {
                            console.write_roles(&[("\n".into(), None), (safe_text(&hook_activity::skip_notice(&notice)), Some(Role::MutedText)), ("\n".into(), None)])?;
                        },
                        EventKind::CommandUpdated {..} | EventKind::TaskBoardChanged {..} | EventKind::ScheduledTasksChanged {..} => {},
                        EventKind::ContextCompaction {status,error} => {
                            outln!(console, "\nContext compaction: {status:?}");
                            if let Some(error)=error {outln!(console, "{}",safe_text(&error));}
                        },
                        EventKind::AssistantMessageCommitted {..} | EventKind::ToolStarted {..} | EventKind::ToolCompleted {..} | EventKind::AgentMessageQueued {..} => {},
                    }

                },
                Ok(event) => {
                    track_run_requests(&event,&mut pending,&mut permissions,&mut questions);
                    status_dirty |= announce_background_event(console,&event,&background_label(agent,console,event.chat_id),&mut background_notice)?;
                },
                Err(EventInterruption::Lost(lost)) => {
                    let mut recovery = agent.pause_event_delivery().await;
                    #[cfg(test)]
                    if matches!(lost, LostEvents::AnyChat) {
                        agent.record_recovery_fact(recovery_tests::RecoveryFact::Lagged);
                    }
                    thinking_previews.clear();
                    status_dirty = true;
                    let visible_chat = selected_chat.filter(|_| !overview_active);
                    if let Some(chat)=visible_chat.filter(|chat|lost.includes(*chat)) {
                        let frame_hold=console.hold_frames()?;
                        let observation=restore_view(console, agent,&mut tools,chat,*displayed_sequences.get(&chat.uuid()).unwrap_or(&0),&mut reply_lines,&mut message_groups).await?;
                        drop(frame_hold);
                        failed |= observation.snapshot.chat_loop.state==tickloop_harness::protocol::LoopState::Failed;
                        displayed_sequences.insert(chat.into(),observation.snapshot.last_sequence);
                        let status=ChatStatus::of(&observation.snapshot);
                        sync_status(&status,&mut pending,&mut permissions,&mut questions);
                        menus.note_status_requests(&status);
                        recovery.cover(observation.execution, &observation.snapshot);
                    }
                    let (mut background_snapshots, unlisted_chats) = match &lost {
                        LostEvents::AnyChat => {
                            let listed=agent.list_chats().await.into_iter().filter(|snapshot|Some(snapshot.conversation.id)!=visible_chat).collect::<Vec<_>>();
                            let unlisted=pending.values().map(|receipt|receipt.chat_id)
                                .filter(|chat|Some(*chat)!=visible_chat&&!listed.iter().any(|snapshot|snapshot.conversation.id==*chat))
                                .collect::<HashSet<_>>();
                            (listed, unlisted)
                        },
                        LostEvents::Chats(chats) => (Vec::new(), chats.iter().copied().filter(|chat|Some(*chat)!=visible_chat).collect()),
                    };
                    for chat in unlisted_chats {
                        match agent.live_snapshot(chat.into()).await {
                            Ok(snapshot)=>background_snapshots.push(snapshot),
                            Err(error) if error.code==AccessErrorCode::NotFound=>{
                                pending.retain(|_,receipt|receipt.chat_id!=chat);
                                permissions.retain(|_,request|request.chat_id!=chat);
                                questions.retain(|_,question|question.chat_id!=chat);
                            },
                            Err(error)=>return Err(error.to_string()),
                        }
                    }
                    for live in background_snapshots {
                        let execution = live.execution;
                        let snapshot = live.into_snapshot();
                        let status = ChatStatus::of(&snapshot);
                        menus.note_status_requests(&status);
                        recovery.cover(execution, &snapshot);
                        let recovery=recover_background_chat(&status,&background_label(agent,console,snapshot.id),&mut pending,&mut permissions,&mut questions);
                        failed |= recovery.failed;
                        if !interactive {
                            for notice in &recovery.run_notices {outln!(console,"Background {}",notice.text(&background_label(agent,console,snapshot.id)));}
                            for notice in &recovery.request_notices {outln!(console,"Background {notice}");}
                        }
                        if let Some(notice)=recovery.run_notices.last() {background_notice=Some((snapshot.id,notice.clone()));}
                    }
                    recovery.retain_uncovered(&mut events, &mut deferred_events);
                },
                Err(EventInterruption::Closed) => return Err("agent event stream closed".into()),
                }
            },
            event = user_shell_events.recv() => match event {
                Ok(record) if user_shell_offsets.contains_key(&record.id) => {
                    status_dirty=true;
                    let observation=observe_user_shell_record(&mut foreground_shells,&mut backgrounded_shells,&record);
                    let terminal=user_shell_is_terminal(&record);
                    if !overview_active && Some(record.chat_id)==selected_chat {
                        if let Some(foreground)=observation.foreground {console.set_foreground_shell(foreground)?;}
                        show_started_shell(console,&mut queued_shells,&record)?;
                        if observation.render_in_chat && render_user_shell_update(agent,console,&record,&mut user_shell_offsets).await? {
                            user_shell_chats.remove(&record.id);
                        }
                        if terminal {
                            user_shell_chats.remove(&record.id);
                            user_shell_offsets.remove(&record.id);
                        }
                    } else {
                        deferred_shell_updates.insert(record.id,record);
                    }
                },
                Ok(_) => {},
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    status_dirty=true;
                    let tracked_chats=user_shell_chats.values().copied().collect::<HashSet<_>>();
                    for chat in tracked_chats {
                        let status=agent.status(chat).await.map_err(|error|error.to_string())?;
                        let tracked_records=status.user_shells.into_iter().filter(|record|user_shell_offsets.contains_key(&record.id)).collect::<Vec<_>>();
                        for record in tracked_records {
                            let observation=observe_user_shell_record(&mut foreground_shells,&mut backgrounded_shells,&record);
                            let terminal=user_shell_is_terminal(&record);
                            if !overview_active && Some(record.chat_id)==selected_chat {
                                if let Some(foreground)=observation.foreground {console.set_foreground_shell(foreground)?;}
                                show_started_shell(console,&mut queued_shells,&record)?;
                                if observation.render_in_chat && render_user_shell_update(agent,console,&record,&mut user_shell_offsets).await? {
                                    user_shell_chats.remove(&record.id);
                                }
                                if terminal {
                                    user_shell_chats.remove(&record.id);
                                    user_shell_offsets.remove(&record.id);
                                }
                            } else {
                                deferred_shell_updates.insert(record.id,record);
                            }
                        }
                    }
                },
                Err(broadcast::error::RecvError::Closed) => return Err("user shell event stream closed".into()),
            },
            Some(result)=compactions.join_next_with_id(), if !compactions.is_empty() => {
                status_dirty=true;
                let task_id=match &result {Ok((id,_))=>*id,Err(error)=>error.id()};
                compaction_chats.remove(&task_id).ok_or("unknown compaction task")?;
                match result {
                    Ok((_,(chat,Ok(outcome))))=>write_compaction_notice(console, chat, &outcome.summary)?,
                    Ok((_,(_,Err(error))))=>{outln!(console, "Compaction failed: {}",safe_text(&error.to_string()));failed=true;},
                    Err(_)=>{outln!(console, "Compaction observer stopped before receiving its result.");failed=true;},
                }
            },
            completion=directed_summaries.join_next(), if !directed_summaries.is_empty() => {
                status_dirty=true;
                match completion?.ok_or("directed summary task disappeared")? {
                    SummaryTaskCompletion::Finished {metadata,result} => match *result {
                        Ok(outcome) => {
                            let effect=directed_summary::finish_success(console,outcome)?;
                            summary_recovery_required.remove(&metadata.chat);
                            displayed_sequences.insert(metadata.chat.into(),effect.snapshot.last_sequence);
                            merged_echoes.close_all();
                            tools.restore_snapshot(&effect.snapshot);
                            message_groups=MessageGroups::default();
                            sync_snapshot(&effect.snapshot,&mut pending,&mut permissions,&mut questions);
                            selected_chat=Some(metadata.chat);
                        },
                        Err(error) => {
                            let effect=directed_summary::finish_failure(console,&error)?;
                            console.finish_summary_input_transition()?;
                            failed |= effect.failed;
                        },
                    },
                    SummaryTaskCompletion::ObserverStopped {metadata}=>{
                        summary_recovery_required.insert(metadata.chat);
                        outln!(console,"Conversation summary observer for Chat {} stopped before receiving its result. Restart the Agent host to recover before continuing.",metadata.chat.public());
                        console.finish_summary_input_transition()?;
                        failed=true;
                    },
                }
                if directed_summaries.is_empty() && summary_recovery_required.is_empty() {
                    console.set_prompt_enabled(true)?;
                }
            },
            _ = global_refresh.tick(), if global_refresh_enabled => {
                match global.refresh().await {
                    Ok(_)=>{
                        if let Ok(Some(receipt))=global.reconcile_dispatch().await {
                            match agent.accept_created(receipt).await {
                                CreationAcceptance::Attached(_,submitted)=>{if let Some(receipt)=submitted {pending.insert(receipt.run_id.into(),receipt);}},
                                CreationAcceptance::AttachUnavailable(error)=>{
                                    let notice=creation_attach_unavailable(&error);
                                    console.set_status(&notice)?;
                                    report_refused_input(console,&notice,&mut failed);
                                },
                                CreationAcceptance::Rejected(error)=>{
                                    let notice=safe_text(&format!("Creation receipt resolved: {error}"));
                                    console.set_status(&notice)?;
                                    report_refused_input(console,&notice,&mut failed);
                                },
                            }
                        }
                        let projection=global.projection();
                        console.remember_agent_colors(agent_messaging::catalog_colors(&projection.rows))?;
                        // A run whose execution the daemon has lost can no longer report its end.
                        for notice in end_runs_of_lost_executions(&projection.rows,&mut pending,&mut permissions,&mut questions) {
                            failed=true;
                            if interactive {
                                let label=readable_background_label(agent,console,notice.chat);
                                console.record_execution_loss(notice.chat.into(),console::ExecutionLossNotice {
                                    run_id:notice.run.into(),
                                    summary:format!("\n{label} · Execution connection was lost · outcome unknown for that attempt; nothing was resent. Use /resume to open saved history; continuing requires an explicit new input.\n"),
                                    technical:notice.text,
                                })?;
                            } else {outln!(console,"{}",notice.text);}
                            if Some(notice.chat)!=selected_chat{background_notice=Some((notice.chat,BackgroundRunNotice { message: "Outcome unknown · execution lost · nothing resent".into(), compact_message: Some("Unknown/lost/no resend".into()), necessary: true }));}
                        }
                        console.set_creation_notice(creation_notice(&projection))?;
                        if overview_active {console.set_global_overview(Some(agent_overview(&projection)))?;}
                        let visible_rows=projection.visible.map(|execution|global.family(execution)).unwrap_or_default();
                        if let Some(chat)=selected_chat {
                            if let Some(row)=visible_rows.iter().find(|row|row.chat_id==chat) {
                                global.visible_ready(Some(row.execution));
                                console.set_family_input_targets(family_input_targets(&global.family_members(row.execution), row.execution))?;
                                let family=global.family(row.execution);
                                visible_family=family.iter().filter(|row|row.availability==ExecutionAvailability::Live).map(|row|ChatId::from(row.chat_id)).collect();
                                console.set_agent_family(if family.len()>1 {agent_rows(&family,false)}else{Vec::new()},Some(row.execution.agent_id))?;
                                console.set_task_title((row.parent.is_none() && row.has_work==Some(true)).then(|| safe_line(&row.title)))?;
                                // Only the daemon's current answer can prove a run ended; a cached copy predates it.
                                // A member is read when this View does not follow its feed yet, or when the catalog
                                // shows its work changed since its snapshot was last applied, as a run ending
                                // without its end event does. The reads go out together, so a quiet family costs
                                // none and a busy one a single round trip that Thinking deltas do not wait behind.
                                applied_member_work.retain(|member,_|visible_family.contains(&tickloop_ids::Id::from(*member)));
                                let changed=family.into_iter().filter(|row|row.availability==ExecutionAvailability::Live
                                    && (!agent.follows(row.execution) || applied_member_work.get(&row.chat_id)!=Some(&MemberWork::of(row))))
                                    .collect::<Vec<_>>();
                                let statuses=futures_util::future::join_all(changed.iter().map(|row|agent.current_status(row.chat_id))).await;
                                for (row,status) in changed.iter().zip(statuses) {
                                    if let Ok(status)=status {
                                        let in_progress=receipts_in_progress(&status).map(|receipt|receipt.run_id).collect::<HashSet<_>>();
                                        // A receipt the status no longer has in progress belongs to a run that ended,
                                        // including one whose end event never reached this View, such as one lost to a
                                        // lagged feed; its Loop may since wait for a trigger or run another input.
                                        let outcome_pending=pending.values().any(|receipt|receipt.chat_id==status.id && !in_progress.contains(&receipt.run_id));
                                        // Events already queued for this View finish the run themselves; restoring
                                        // from a snapshot now would print the reply a second way. The member is read
                                        // again on the next refresh in case they do not.
                                        if outcome_pending && !events.is_empty() && Some(status.id)==selected_chat {
                                            continue;
                                        }
                                        applied_member_work.insert(row.chat_id,MemberWork::of(row));
                                        if outcome_pending {
                                            if !overview_active && Some(status.id)==selected_chat {
                                                let restored=restore_view(console,agent,&mut tools,status.id,*displayed_sequences.get(&status.id.uuid()).unwrap_or(&0),&mut reply_lines,&mut message_groups).await?;
                                                failed |= restored.snapshot.chat_loop.state==tickloop_harness::protocol::LoopState::Failed;
                                                displayed_sequences.insert(status.id.into(),restored.snapshot.last_sequence);
                                                merged_echoes.close_all();
                                                sync_snapshot(&restored.snapshot,&mut pending,&mut permissions,&mut questions);
                                            } else {
                                                let recovery=recover_background_chat(&status,&background_label(agent,console,status.id),&mut pending,&mut permissions,&mut questions);
                                                failed |= recovery.failed;
                                                for notice in recovery.run_notices {
                                                    if !interactive {outln!(console,"Background {}",notice.text(&background_label(agent,console,status.id)));}
                                                    background_notice=Some((status.id,notice));
                                                }
                                                for notice in recovery.request_notices {
                                                    if !interactive {outln!(console,"Background {notice}");}
                                                }
                                            }
                                            continue;
                                        }
                                        sync_status(&status,&mut pending,&mut permissions,&mut questions);
                                        if Some(status.id)==selected_chat {
                                            turn_snapshot.keep_status(Arc::new(status));
                                        }
                                    }
                                }
                                if row.availability==ExecutionAvailability::Live {
                                    show_settings_warnings(console,agent.user_settings_warnings(chat.into()))?;
                                }
                            }
                            else {visible_family.clear();console.set_agent_family(Vec::new(),None)?;console.set_task_title(None)?;console.set_family_input_targets(Vec::new())?;}
                        } else {visible_family.clear();console.set_agent_family(Vec::new(),None)?;console.set_task_title(None)?;console.set_family_input_targets(Vec::new())?;}
                        let background_request_chats=permissions.values()
                            .map(|request|request.chat_id)
                            .chain(questions.values().map(|request|request.chat_id))
                            .filter(|chat|!visible_family.contains(chat))
                            .collect::<HashSet<_>>();
                        for chat in background_request_chats {
                            // A cached snapshot cannot prove that another View resolved a request.
                            if let Ok(execution)=agent.reference(chat.into()).await
                                && let Ok(observation)=agent.access().observe(ObservationRequest::Agent {
                                    execution,detail:AgentDetailQuery::Status,after:None,
                                }).await
                                && let ObservationValue::Status(status)=observation.initial
                            {
                                sync_status_requests(&status.chat,&mut permissions,&mut questions);
                            }
                        }
                        let live_visible=selected_chat.filter(|chat|visible_rows.iter().any(|row|row.chat_id==*chat && row.availability==ExecutionAvailability::Live));
                        daemon_unreachable=None;
                        let notice=if let Err(error)=agent.display_chat(if overview_active {None}else{live_visible.map(Into::into)}).await {
                            Some(format!("{VIEW_REGISTRATION_UNAVAILABLE_NOTICE} · {}",safe_line(&error.to_string())))
                        } else if let Err(error)=agent.renew_display().await {
                            Some(format!("View connection unavailable · {}",safe_line(&error.to_string())))
                        } else {None};
                        if notice.is_some() || selected_chat.is_none() || overview_active {
                            console.set_connection_notice(notice.or_else(||agent.disconnected()))?;
                        } else {
                            // A selected Agent shows its own loss as its execution notice.
                            console.set_connection_notice(None)?;
                        }
                        status_dirty=true;
                    }
                    Err(error)=>{
                        daemon_unreachable=Some(format!("{DISCONNECTED_CACHED_VIEW_NOTICE} · {}",safe_text(&error.to_string())));
                        if !overview_active && selected_chat.is_some() {
                            status_dirty=true;
                        } else {console.set_connection_notice(daemon_unreachable.clone())?;}
                    }
                }
            },
            _ = update_notice_refresh.tick(), if interactive && !visual_pacing_pending && update_monitor.is_some() => {
                installation_notice = update_monitor.as_ref().and_then(|monitor| monitor.notice());
            },
            _ = animation_refresh.tick(), if interactive && !visual_pacing_pending && (thinking_frame_due || (!prefers_reduced_motion && (!pending.is_empty() || !directed_summaries.is_empty()))) => {
                thinking_frame_due = false;
                activity_dirty = true;
            },
            _ = transcript_refresh.tick(), if interactive && transcript_refresh_due => {
                transcript_refresh_due = false;
                if let Some(chat) = selected_chat
                    && let Some(id) = console.open_transcript()?
                    && let Ok(snapshot) = agent.snapshot(chat.into()).await
                {
                    console.refresh_transcript(id, presentation::transcript_document(&snapshot, console.display_profile()?, &console.image_links()?, &console.agent_colors()?))?;
                }
            },
            _ = elapsed_refresh.tick(), if interactive && !visual_pacing_pending && (prefers_reduced_motion && running_activity.as_ref().is_some_and(|activity| activity.started_at.is_some())) => {
                activity_dirty = true;
            },
            _ = tokio::task::yield_now(), if visual_pacing_pending => {
                if let Some(reply) = reply_lines.values_mut().find(|reply| Some(reply.chat_id) == selected_chat.map(Into::into) && reply.has_complete_line()) {
                    write_next_reply_line(console, reply)?;
                }
            },
            _ = member_probe.tick(), if interactive && !overview_active => {
                // A slow host never stalls input: the probe runs beside the loop and reports back.
                if let Some(probe) = console.claim_member_probe()? {
                    let console = console.clone();
                    tokio::spawn(async move {
                        let chat = probe.chat;
                        // A failed or panicked probe keeps its type's earlier findings and the probe
                        // slot is still released, so probing never stops for the session.
                        let mut results = Vec::new();
                        for typed in probe.probes {
                            let access = typed.prober.access();
                            let probed = tokio::spawn(async move {
                                typed.prober.probe(chat, &typed.targets).await
                            })
                            .await;
                            results.push((access, probed.ok().and_then(Result::ok)));
                        }
                        let _ = console.update_member_findings(chat, results);
                    });
                }
            },
            _ = subscription_refresh.tick(), if interactive && !overview_active && console.monitor_has_entries()? => {
                monitor_refresh_due = true;
            },
            _ = prompt_watchdog_refresh.tick(), if selected_chat.is_some() => {
                let chat=selected_chat.expect("selected Chat checked above");
                check_shell_prompt_watchdog(
                    &task_center,
                    console,
                    chat,
                    &mut prompt_watchdog,
                    prompt_watchdog_epoch.elapsed(),
                ).await?;
            },
            notification = task_notifications.recv() => match notification {
                Ok(notification) if !overview_active && Some(notification.chat_id)==selected_chat => {
                    console.write_shell_notification(&notification)?;
                    status_dirty=true;
                },
                Ok(notification) => {
                    let notice=format!("{} · {}",background_label(agent,console,notification.chat_id),safe_line(&notification.summary));
                    if !interactive {outln!(console,"Background {notice}");}
                    let (message, necessary) = match notification.status {
                        TaskNotificationStatus::Completed => ("Shell task completed", false),
                        TaskNotificationStatus::Failed => ("Shell task failed", true),
                        TaskNotificationStatus::Killed => ("Shell task killed", false),
                    };
                    background_notice=Some((notification.chat_id,BackgroundRunNotice { message: message.into(), compact_message: None, necessary }));
                    status_dirty=true;
                },
                Err(broadcast::error::RecvError::Lagged(_)) => {},
                Err(broadcast::error::RecvError::Closed) => return Err("task notification stream closed".into()),
            },
            signal = tokio::signal::ctrl_c() => {
                signal.map_err(|_| "cannot listen for terminal interrupt")?;
                outln!(console, "\nClosing this View; Agent work continues.");
                return Ok(());
            }
        }
    }
}

/// Claude Code's auto-restore on interrupt: with nothing typed, asks the Agent to take back the
/// prompt of the interrupted run; when it did, the conversation is drawn again without that turn
/// and the prompt returns to the editor.
async fn take_back_interrupted_prompt(
    agent: &Agent,
    console: &Console,
    chat: ChatId,
    run: RunId,
) -> Result<Option<ChatSnapshot>, String> {
    if !console.input_is_empty()? {
        return Ok(None);
    }
    let (snapshot, draft) = match agent
        .take_back_interrupted_prompt(chat.into(), run.into())
        .await
    {
        Ok(Some(taken_back)) => taken_back,
        Ok(None) => return Ok(None),
        Err(error) => {
            console.set_status(&format!(
                "Cannot restore the interrupted prompt: {}",
                safe_line(&error.to_string())
            ))?;
            return Ok(None);
        }
    };
    console.reset_chat(chat.into())?;
    workflows::write_public_history(console, &snapshot)?;
    console.restore_interrupted_prompt(&draft.prompt, &draft.contexts)?;
    Ok(Some(snapshot))
}

async fn open_rewind(
    agent: &Agent,
    console: &Console,
    menus: &mut Menus,
    chat: Option<ChatId>,
    is_busy: bool,
) -> Result<Option<String>, String> {
    let Some(chat) = chat else {
        let status = "Nothing to rewind to yet";
        console.set_status(status)?;
        return Ok(Some(status.into()));
    };
    if is_busy {
        let status = "Interrupt or finish current work before opening rewind";
        console.set_status(status)?;
        return Ok(Some(status.into()));
    }
    let snapshot = agent
        .snapshot(chat.into())
        .await
        .map_err(|error| error.to_string())?;
    if !snapshot.permissions.is_empty() || !snapshot.questions.is_empty() {
        let status = "Resolve the current permission or question before opening rewind";
        console.set_status(status)?;
        return Ok(Some(status.into()));
    }
    menus.rewind(console, &snapshot)
}

fn set_monitor_visible(
    console: &Console,
    settings: Option<&UserSettings>,
    show: bool,
) -> Result<(), String> {
    let settings =
        settings.ok_or("No Agent state directory is configured; cannot save Monitor preference")?;
    let preference = if show {
        tickloop_code_settings::MonitorPreference::Auto
    } else {
        tickloop_code_settings::MonitorPreference::Hidden
    };
    settings
        .save_monitor_preference(preference)
        .map_err(|error| format!("Cannot save Monitor preference: {error}"))?;
    console.set_monitor_preference(preference)?;
    if show {
        console.open_monitor()?;
    }
    Ok(())
}

fn save_monitor_width(settings: Option<&UserSettings>, columns: usize) -> Result<(), String> {
    settings
        .ok_or("No Agent state directory is configured; cannot save the Monitor width")?
        .save_monitor_width(u16::try_from(columns).unwrap_or(u16::MAX))
        .map_err(|error| format!("Cannot save the Monitor width: {error}"))
}

async fn open_diff(agent: &Agent, console: &Console, chat_id: ChatId) -> Result<(), String> {
    let snapshot = match agent.snapshot(chat_id.into()).await {
        Ok(snapshot) => snapshot,
        Err(error) => {
            console.set_status(&format!(
                "Cannot open Diff: {}",
                safe_text(&error.to_string())
            ))?;
            return Ok(());
        }
    };
    let catalog = ChangeCatalog::load(&snapshot).await;
    console.show_diff(chat_id.into(), catalog)
}

async fn check_shell_prompt_watchdog(
    task_center: &ShellTaskCenter,
    console: &Console,
    chat_id: ChatId,
    watchdog: &mut ShellTaskPromptWatchdog,
    now: std::time::Duration,
) -> Result<(), String> {
    const PROMPT_TAIL_BYTES: u64 = 4096;

    let snapshot = task_center
        .snapshot(chat_id.into())
        .await
        .map_err(|error| error.to_string())?;
    for task in &snapshot.tasks {
        let trailing_output = if task.state == ShellTaskState::Background && task.output_bytes > 0 {
            let cursor = task.output_bytes.saturating_sub(PROMPT_TAIL_BYTES);
            let limit = usize::try_from(task.output_bytes - cursor)
                .unwrap_or(PROMPT_TAIL_BYTES as usize)
                .max(1);
            task_center
                .read_output(chat_id.into(), task.id, cursor, limit)
                .await
                .map(|chunk| chunk.output)
                .unwrap_or_default()
        } else {
            String::new()
        };
        if let Some(ShellTaskNotice::InteractivePrompt { task_id, .. }) =
            watchdog.observe(task, &trailing_output, now)
        {
            outln!(
                console,
                "\nBackground Shell {} may be waiting for input. Open it in Monitor to inspect it.",
                tick_protocol::ShellTaskId::from(task_id).public()
            );
        }
    }
    Ok(())
}

/// Following Claude Code's task notification style: a status-colored dot plus a summary.
fn task_notification_line(
    notification: &TaskNotification,
    display: DisplayProfile,
) -> Vec<(String, Option<Role>)> {
    let role = match notification.status {
        TaskNotificationStatus::Completed => Role::Success,
        TaskNotificationStatus::Failed => Role::Error,
        TaskNotificationStatus::Killed => Role::Warning,
    };
    vec![
        ("\n".into(), None),
        (display.glyph(Glyph::ToolCall).into(), Some(role)),
        (format!(" {}\n", safe_text(&notification.summary)), None),
    ]
}

/// What the view has read of a `!` command: where reading resumes and its latest lines.
#[derive(Default)]
struct UserShellOutput {
    offset: u64,
    latest_lines: String,
}

/// Keeps the last `count` lines of `text`.
fn keep_last_lines(text: &mut String, count: usize) {
    let trimmed = text.trim_end_matches('\n');
    if let Some((cut, _)) = trimmed.rmatch_indices('\n').nth(count.saturating_sub(1)) {
        text.drain(..=cut);
    }
}

/// Shows what a `!` command printed: its latest lines under its echo while it runs, then its
/// result as the conversation keeps it. Returns whether the command's output is complete.
async fn render_user_shell_update(
    agent: &Agent,
    console: &Console,
    record: &UserShellRecord,
    outputs: &mut HashMap<uuid::Uuid, UserShellOutput>,
) -> Result<bool, String> {
    let is_terminal = matches!(
        record.state,
        UserShellState::Completed | UserShellState::Failed | UserShellState::Unknown
    );
    let shown = outputs.entry(record.id).or_default();
    let mut offset = shown.offset;
    while record.output_bytes > offset {
        let result = match agent
            .read_user_shell_output(record.chat_id.into(), record.id, offset, 64 * 1024)
            .await
        {
            Ok(result) => result,
            Err(error) => {
                outln!(
                    console,
                    "\nCannot read user shell output: {}",
                    safe_text(&error.to_string())
                );
                return Ok(false);
            }
        };
        let value: serde_json::Value = serde_json::from_str(&result.content)
            .map_err(|error| format!("invalid user shell output record: {error}"))?;
        if let Some(text) = value["output"].as_str() {
            if console.is_interactive() {
                shown.latest_lines.push_str(text);
                keep_last_lines(&mut shown.latest_lines, USER_SHELL_PREVIEW_LINES);
            } else {
                out!(console, "{}", safe_text(text));
            }
        }
        let next_offset = value["next_offset"]
            .as_u64()
            .or_else(|| value["total_bytes"].as_u64())
            .unwrap_or(offset);
        if next_offset <= offset {
            break;
        }
        offset = next_offset;
        shown.offset = offset;
        if !is_terminal {
            break;
        }
    }
    if !is_terminal && console.is_interactive() {
        console.set_shell_preview(&presentation::running_shell_runs(
            &shown.latest_lines,
            console.display_profile()?,
        ))?;
    }
    if !is_terminal || offset < record.output_bytes {
        return Ok(false);
    }
    if console.is_interactive() {
        let output = agent
            .read_user_shell_output(record.chat_id.into(), record.id, 0, USER_SHELL_RESULT_BYTES)
            .await
            .ok()
            .and_then(|result| serde_json::from_str::<serde_json::Value>(&result.content).ok())
            .and_then(|value| value["output"].as_str().map(str::to_owned))
            .unwrap_or_default();
        let (_, response) = presentation::local_message_runs(
            &presentation::user_shell_result(record, &output),
            presentation::OutputDetail::Preview,
            console.display_profile()?,
        );
        console.set_shell_preview(&[])?;
        console.write_response(&response)?;
    } else {
        outln!(
            console,
            "\n{} user shell {} · exit {:?} · signal {:?}{}\nResult added to this Chat; enter a prompt to discuss it.",
            if record.state == UserShellState::Completed {
                "✓"
            } else {
                "Failed"
            },
            record.id,
            record.exit_code,
            record.signal,
            record
                .reason
                .as_ref()
                .map(|reason| format!(" · {}", safe_text(reason)))
                .unwrap_or_default(),
        );
    }
    outputs.remove(&record.id);
    Ok(true)
}

#[derive(Debug, PartialEq, Eq)]
struct UserShellObservation {
    render_in_chat: bool,
    /// `Some` means this Chat's foreground Shell state should change to this value.
    foreground: Option<bool>,
}

/// Adjusts the foreground Shell from `!` command state records. A user state event may be read
/// after the move-to-background handling, so commands already moved to the background are tracked
/// in `backgrounded_shells` and a stale "running" record cannot bring them back to the foreground;
/// backgrounded records always leave the foreground and are no longer tracked once finished.
fn observe_user_shell_record(
    foreground_shells: &mut HashMap<uuid::Uuid, uuid::Uuid>,
    backgrounded_shells: &mut HashSet<uuid::Uuid>,
    record: &UserShellRecord,
) -> UserShellObservation {
    if record.starts_in_background {
        backgrounded_shells.insert(record.id);
    }
    let is_backgrounded = backgrounded_shells.contains(&record.id);
    let is_terminal = user_shell_is_terminal(record);
    let was_foreground = foreground_shells.get(&record.chat_id.uuid()) == Some(&record.id);
    let mut foreground = None;
    if record.state == UserShellState::Running && !is_backgrounded {
        foreground_shells.insert(record.chat_id.into(), record.id);
        foreground = Some(true);
    }
    let is_foreground = foreground_shells.get(&record.chat_id.uuid()) == Some(&record.id);
    let render_in_chat = !is_backgrounded && (was_foreground || is_foreground || is_terminal);
    if (is_terminal || is_backgrounded) && is_foreground {
        foreground_shells.remove(&record.chat_id.uuid());
        foreground = Some(false);
    }
    if is_terminal {
        backgrounded_shells.remove(&record.id);
    }
    UserShellObservation {
        render_in_chat,
        foreground,
    }
}

/// Shows a Shell command this View sent while a turn worked as sent once it starts, in the frame
/// that takes it out of the queue.
fn show_started_shell(
    console: &Console,
    queued_shells: &mut HashSet<uuid::Uuid>,
    record: &UserShellRecord,
) -> Result<(), String> {
    if record.state == UserShellState::Queued || !queued_shells.remove(&record.id) {
        return Ok(());
    }
    let _frame_hold = console.hold_frames()?;
    console.withdraw_queued_input(record.id)?;
    console.record_consumed_input(
        record.chat_id.into(),
        record.id,
        &safe_text(&format!("!{}", record.command)),
    )?;
    Ok(())
}

fn user_shell_is_terminal(record: &UserShellRecord) -> bool {
    matches!(
        record.state,
        UserShellState::Completed | UserShellState::Failed | UserShellState::Unknown
    )
}

/// What one loop turn read about a Chat, kept for the status refresh that follows the turn, so a
/// turn reads each from the daemon at most once: the conversation, which only a turn that shows
/// part of the history reads, and the status, which every refresh needs and which a conversation
/// read in the same turn also answers. The refresh drops both, and so does any change this View
/// makes to the Chat in between.
#[derive(Default)]
struct TurnSnapshot {
    conversation: Option<Arc<ChatSnapshot>>,
    status: Option<Arc<ChatStatus>>,
}

impl TurnSnapshot {
    /// The Chat with its history.
    async fn read(
        &mut self,
        agent: &Agent,
        chat: ChatId,
    ) -> Result<Arc<ChatSnapshot>, AccessError> {
        if let Some(snapshot) = self.conversation_of(chat) {
            return Ok(snapshot);
        }
        let snapshot = Arc::new(agent.snapshot(chat.into()).await?);
        self.status = None;
        self.conversation = Some(snapshot.clone());
        Ok(snapshot)
    }

    /// What the Chat is doing now: from this turn's conversation when it read one.
    async fn status(
        &mut self,
        agent: &Agent,
        chat: ChatId,
    ) -> Result<Arc<ChatStatus>, AccessError> {
        if let Some(status) = self.status.as_ref().filter(|status| status.id == chat) {
            return Ok(status.clone());
        }
        let status = Arc::new(match self.conversation_of(chat) {
            Some(snapshot) => ChatStatus::of(&snapshot),
            None => agent.status(chat.into()).await?,
        });
        self.status = Some(status.clone());
        Ok(status)
    }

    /// This turn's conversation of `chat`, when the turn read one.
    fn conversation_of(&self, chat: ChatId) -> Option<Arc<ChatSnapshot>> {
        self.conversation
            .clone()
            .filter(|snapshot| snapshot.id == chat)
    }

    fn keep_status(&mut self, status: Arc<ChatStatus>) {
        self.status = Some(status);
    }

    fn forget(&mut self) {
        self.conversation = None;
        self.status = None;
    }
}

/// What the execution catalog says about a family member's work. Any run that starts or ends,
/// and any request that comes or goes, changes it, so while it stays as it was when the member's
/// snapshot was last applied, that snapshot has nothing new to tell.
#[derive(PartialEq)]
struct MemberWork {
    state: tickloop_harness::protocol::LoopState,
    run_id: Option<RunId>,
    updated_at: Option<u64>,
    waiting_permissions: usize,
    waiting_questions: usize,
}

impl MemberWork {
    fn of(row: &AgentSummary) -> Self {
        Self {
            state: row.state.clone(),
            run_id: row.run_id,
            updated_at: row.updated_at,
            waiting_permissions: row.waiting_permissions,
            waiting_questions: row.waiting_questions,
        }
    }
}

/// Report the selected family, or all known work in the overview. A new draft is idle;
/// stale family membership from the previously selected Chat must not leak into its report.
fn herdr_view_state(
    selected: Option<ChatId>,
    overview: bool,
    family: &[ChatId],
    mut working: impl Iterator<Item = ChatId>,
    mut waiting: impl Iterator<Item = ChatId>,
) -> herdr_agent::State {
    let includes = |chat| {
        overview
            || selected.is_some_and(|selected| {
                selected == chat
                    || (family.contains(&tickloop_ids::Id::from(selected.uuid()))
                        && family.contains(&chat))
            })
    };
    if waiting.any(includes) {
        herdr_agent::State::Blocked
    } else if working.any(includes) {
        herdr_agent::State::Working
    } else {
        herdr_agent::State::Idle
    }
}

#[test]
fn herdr_state_follows_selected_family_and_user_requests() {
    use herdr_agent::State;
    let root = ChatId::new();
    let child = ChatId::new();
    let other = ChatId::new();
    let state = |selected, overview, family: &[ChatId], working: Vec<_>, waiting: Vec<_>| {
        herdr_view_state(
            selected,
            overview,
            family,
            working.into_iter(),
            waiting.into_iter(),
        )
    };
    assert_eq!(state(Some(root), false, &[], vec![], vec![]), State::Idle);
    assert_eq!(
        state(Some(root), false, &[], vec![root], vec![]),
        State::Working
    );
    assert_eq!(
        state(Some(root), false, &[], vec![root], vec![root]),
        State::Blocked
    );
    assert_eq!(
        state(Some(root), false, &[root, child], vec![child], vec![]),
        State::Working
    );
    assert_eq!(
        state(Some(root), false, &[root, child], vec![root], vec![child]),
        State::Blocked
    );
    assert_eq!(
        state(Some(other), false, &[root, child], vec![root], vec![child]),
        State::Idle
    );
    assert_eq!(
        state(None, false, &[root, child], vec![root], vec![child]),
        State::Idle
    );
    assert_eq!(state(None, true, &[], vec![root], vec![]), State::Working);
    assert_eq!(
        state(None, true, &[], vec![root], vec![child]),
        State::Blocked
    );
}

/// Tracks the runs and requests of a Chat this View just read in full, as opened or restored.
fn sync_snapshot(
    snapshot: &ChatSnapshot,
    pending: &mut HashMap<uuid::Uuid, Receipt>,
    permissions: &mut HashMap<uuid::Uuid, PermissionRequestOwner>,
    questions: &mut HashMap<uuid::Uuid, QuestionRequest>,
) {
    sync_status(&ChatStatus::of(snapshot), pending, permissions, questions);
}

fn sync_status(
    status: &ChatStatus,
    pending: &mut HashMap<uuid::Uuid, Receipt>,
    permissions: &mut HashMap<uuid::Uuid, PermissionRequestOwner>,
    questions: &mut HashMap<uuid::Uuid, QuestionRequest>,
) {
    sync_status_requests(status, permissions, questions);
    pending.retain(|_, receipt| receipt.chat_id != status.id);
    for receipt in receipts_in_progress(status) {
        pending.insert(receipt.run_id.into(), receipt);
    }
}

/// The runs `status` still has in progress: each armed queued input and the Loop's run while it
/// runs or waits on the user. A run outside them has ended, whether or not its end event arrived.
fn receipts_in_progress(status: &ChatStatus) -> impl Iterator<Item = Receipt> + '_ {
    use tickloop_harness::protocol::LoopState;
    let queued = status
        .pending_inputs
        .iter()
        .filter(|input| input.armed)
        .map(|input| input.receipt.clone());
    let chat_loop = &status.chat_loop;
    let running = chat_loop
        .run_id
        .filter(|_| {
            matches!(
                chat_loop.state,
                LoopState::Running | LoopState::WaitingPermission | LoopState::WaitingQuestion
            )
        })
        .map(|run_id| Receipt {
            chat_id: status.id,
            loop_id: chat_loop.id,
            run_id,
        });
    queued.chain(running)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PermissionRequestOwner {
    chat_id: ChatId,
    loop_id: uuid::Uuid,
    run_id: Option<RunId>,
}

impl PermissionRequestOwner {
    fn new(chat_id: ChatId, request: &tickloop_harness::protocol::PermissionRequest) -> Self {
        Self {
            chat_id,
            loop_id: request.loop_id.into(),
            run_id: request.execution.run_id(),
        }
    }
}

fn sync_status_requests(
    status: &ChatStatus,
    permissions: &mut HashMap<uuid::Uuid, PermissionRequestOwner>,
    questions: &mut HashMap<uuid::Uuid, QuestionRequest>,
) {
    permissions.retain(|_, request| request.chat_id != status.id);
    for request in &status.permissions {
        permissions.insert(request.id, PermissionRequestOwner::new(status.id, request));
    }
    questions.retain(|_, question| question.chat_id != status.id);
    for request in &status.questions {
        questions.insert(request.id, request.clone());
    }
}

/// The Chats whose events this View lost before showing them.
enum LostEvents {
    /// The View's own event queue overflowed, so any Chat may have lost events.
    AnyChat,
    /// The daemon's feed of these Chats skipped events.
    Chats(HashSet<ChatId>),
}

impl LostEvents {
    fn includes(&self, chat: ChatId) -> bool {
        match self {
            Self::AnyChat => true,
            Self::Chats(chats) => chats.contains(&chat),
        }
    }

    /// `lost` widened by a gap in `chat`'s feed; `None` when so many gaps were announced that
    /// some of their Chats are no longer known.
    fn with_gap(lost: Option<Self>, chat: Option<ChatId>) -> Self {
        match (lost, chat) {
            (Some(Self::Chats(mut chats)), Some(chat)) => {
                chats.insert(chat);
                Self::Chats(chats)
            }
            (None, Some(chat)) => Self::Chats(HashSet::from([chat])),
            _ => Self::AnyChat,
        }
    }
}

/// Why the View has no event to show next.
enum EventInterruption {
    /// Events were lost; the View restores them from the daemon's snapshots before showing more.
    Lost(LostEvents),
    Closed,
}

/// `lost` widened by every feed gap announced since the View last read them.
fn announced_feed_gaps(
    feed_gaps: &mut broadcast::Receiver<uuid::Uuid>,
    mut lost: Option<LostEvents>,
) -> Option<LostEvents> {
    loop {
        let chat = match feed_gaps.try_recv() {
            Ok(chat) => Some(chat),
            Err(broadcast::error::TryRecvError::Lagged(_)) => None,
            Err(broadcast::error::TryRecvError::Empty | broadcast::error::TryRecvError::Closed) => {
                return lost;
            }
        };
        lost = Some(LostEvents::with_gap(lost, chat.map(Into::into)));
    }
}

/// The next event to show: a deferred one first unless reply lines are still pacing out.
/// A feed announces its gap before it forwards the events after the gap, so a gap announced by
/// the time an event arrives is restored first and no later event shows ahead of the lost ones.
async fn next_event(
    events: &mut broadcast::Receiver<tickloop_harness::protocol::Event>,
    feed_gaps: &mut broadcast::Receiver<uuid::Uuid>,
    deferred_events: &mut VecDeque<tickloop_harness::protocol::Event>,
    visual_pacing_pending: bool,
) -> Result<tickloop_harness::protocol::Event, EventInterruption> {
    let mut lost = None;
    loop {
        lost = announced_feed_gaps(feed_gaps, lost);
        if let Some(lost) = lost {
            return Err(EventInterruption::Lost(lost));
        }
        if !visual_pacing_pending && let Some(event) = deferred_events.pop_front() {
            return Ok(event);
        }
        tokio::select! {
            biased;
            gap @ (Ok(_) | Err(broadcast::error::RecvError::Lagged(_))) = feed_gaps.recv() => {
                lost = Some(LostEvents::with_gap(None, gap.ok().map(Into::into)));
            }
            received = events.recv() => match received {
                Ok(event) if feed_gaps.is_empty() => return Ok(event),
                Ok(event) => deferred_events.push_back(event),
                Err(broadcast::error::RecvError::Lagged(_)) => lost = Some(LostEvents::AnyChat),
                Err(broadcast::error::RecvError::Closed) => return Err(EventInterruption::Closed),
            },
        }
    }
}

async fn restore_view(
    console: &Console,
    agent: &Agent,
    tools: &mut ToolPresentation,
    chat: ChatId,
    after: u64,
    buffered_replies: &mut HashMap<uuid::Uuid, ReplyLines>,
    message_groups: &mut MessageGroups,
) -> Result<ExecutionObservation, String> {
    let observation = agent
        .observe(chat.into(), after)
        .await
        .map_err(|error| error.to_string())?;
    learn_agent_colors(console, agent, &observation.snapshot).await?;
    let observation = render_restored_observation(
        console,
        tools,
        chat,
        after,
        buffered_replies,
        message_groups,
        observation,
    )?;
    #[cfg(test)]
    agent.record_recovery_fact(recovery_tests::RecoveryFact::Restored {
        chat: chat.into(),
        after,
        last_sequence: observation.snapshot.last_sequence,
        reset_required: observation.reset_required,
    });
    Ok(observation)
}

fn render_restored_observation(
    console: &Console,
    tools: &mut ToolPresentation,
    chat: ChatId,
    after: u64,
    buffered_replies: &mut HashMap<uuid::Uuid, ReplyLines>,
    message_groups: &mut MessageGroups,
    observation: ExecutionObservation,
) -> Result<ExecutionObservation, String> {
    ensure_snapshot_banner(console, &observation.snapshot)?;
    let conversation_events = observation.snapshot.conversation_events();
    console.select_chat(Some(chat.into()))?;
    let failed_runs = conversation_events
        .iter()
        .filter_map(|event| matches!(event.kind, EventKind::Failed(_)).then_some(event.run_id))
        .collect::<HashSet<_>>();
    if observation.reset_required {
        buffered_replies.retain(|_, reply| reply.chat_id != chat);
    } else {
        for event in conversation_events
            .iter()
            .filter(|event| event.chat_id == chat)
        {
            if matches!(event.kind, EventKind::Failed(_)) {
                buffered_replies.remove(&event.run_id.uuid());
            } else if event.sequence <= after
                && matches!(event.kind, EventKind::Completed { .. })
                && let Some(mut reply) = buffered_replies.remove(&event.run_id.uuid())
            {
                reply.finish();
                write_pending_reply_lines(console, &mut reply)?;
            }
        }
    }
    tools.restore(
        &conversation_events
            .iter()
            .filter(|event| event.sequence <= after)
            .cloned()
            .collect::<Vec<_>>(),
    );
    *message_groups = MessageGroups::default();
    for event in conversation_events
        .iter()
        .filter(|event| observation.reset_required || event.sequence <= after)
    {
        let message_id = message_groups.message_id(event);
        if matches!(event.kind, EventKind::Thinking(_) | EventKind::Text(_))
            && let Some(id) = message_id
        {
            message_groups.displayed.insert(id);
        }
    }
    if observation.reset_required {
        console.reset_chat(chat.into())?;
        tools.restore_snapshot(&observation.snapshot);
        workflows::write_public_history(console, &observation.snapshot)?;
        if observation.snapshot.chat_loop.state == tickloop_harness::protocol::LoopState::Failed {
            let error=observation.snapshot.events.iter().rev().find_map(|event| {
                if let EventKind::Failed(error)=&event.kind {Some(error.as_str())} else {None}
            }).unwrap_or("The execution failed before this View received its final update; inspect the saved execution history.");
            outln!(console, "Failed: {}", safe_text(error));
        }
    } else {
        let mut thinking_runs = HashSet::new();
        for event in conversation_events
            .iter()
            .filter(|event| event.sequence <= after)
        {
            if matches!(event.kind, EventKind::Thinking(_)) {
                thinking_runs.insert(event.run_id);
            } else if ends_thinking(&event.kind) {
                thinking_runs.remove(&event.run_id);
            }
        }
        for event in &observation.events {
            if matches!(
                event.kind,
                EventKind::Started | EventKind::InputMerged { .. }
            ) {
                show_consumed_family_input(console, &observation.snapshot, event.run_id)?;
            }
            if matches!(
                event.kind,
                EventKind::Started | EventKind::InputMerged { .. }
            ) {
                consume_input_echo_from_snapshot(
                    console,
                    &observation.snapshot,
                    event.run_id.into(),
                )?;
            }
            if matches!(
                event.kind,
                EventKind::Thinking(_)
                    | EventKind::AssistantMessageCommitted { .. }
                    | EventKind::ToolStarted { .. }
                    | EventKind::PermissionRequested { .. }
                    | EventKind::QuestionAsked { .. }
                    | EventKind::ModelRetry { .. }
                    | EventKind::Completed { .. }
                    | EventKind::Interrupted
                    | EventKind::Failed(_)
            ) && let Some(mut reply) = buffered_replies.remove(&event.run_id.uuid())
            {
                reply.finish();
                write_pending_reply_lines(console, &mut reply)?;
            }
            if matches!(event.kind, EventKind::Failed(_)) {
                buffered_replies.remove(&event.run_id.uuid());
            }
            let message_id = message_groups.message_id(event);
            let starts_thinking = matches!(event.kind, EventKind::Thinking(_))
                && message_id
                    .as_ref()
                    .is_some_and(|id| message_groups.displayed.insert(id.clone()));
            let starts_assistant_message = matches!(event.kind, EventKind::Text(_))
                && message_id
                    .as_ref()
                    .is_some_and(|id| message_groups.displayed.insert(id.clone()));
            let follows_thinking = starts_assistant_message
                && message_id.as_ref().is_some_and(|id| {
                    message_groups
                        .displayed
                        .contains(&id.replace(":assistant:", ":thinking:"))
                });
            let console = &console.for_message(message_id.clone());
            if matches!(event.kind, EventKind::Thinking(_)) {
                thinking_runs.insert(event.run_id);
            } else if ends_thinking(&event.kind)
                && thinking_runs.remove(&event.run_id)
                && console.is_interactive()
            {
                console.write_pieces(&[presentation::thinking_label()])?;
            }
            if let Some(call_id) = tools.nested_call_to_learn(event)
                && let Some(nested) = observation.snapshot.operations.iter().find(|operation| {
                    operation.run_id == event.run_id && operation.call_id == call_id
                })
            {
                tools.learn_nested_call(event, nested);
            }
            let facts = tools.call_facts(event, &observation.snapshot);
            let interrupted_by_rejection = tools.ended_with_rejection(event.run_id.into());
            if tools.present_event(console, event, &facts)? {
                continue;
            }
            match &event.kind {
                EventKind::FamilyInputUpdated { record } => console.write(
                    &presentation::family_input_text(record, presentation::OutputDetail::Preview),
                )?,
                EventKind::FamilyRunCompleted { completion } => {
                    show_family_completion(console, completion, &observation.snapshot)?
                }
                EventKind::Thinking(thinking) => {
                    if !console.is_interactive() {
                        if starts_thinking {
                            console.write("\nThinking: ")?;
                        }
                        console.write(&safe_text(thinking))?;
                    }
                }
                EventKind::Text(text) if !failed_runs.contains(&event.run_id) => {
                    if console.is_interactive() {
                        let reply = buffered_replies
                            .entry(event.run_id.into())
                            .or_insert_with(|| ReplyLines::new(event.chat_id.into(), message_id));
                        reply.push(&safe_text(text));
                        write_pending_reply_lines(console, reply)?;
                    } else {
                        if follows_thinking {
                            console.write("\nAssistant: ")?;
                        }
                        console.write_markdown(&safe_text(text), starts_assistant_message)?;
                    }
                }
                EventKind::Completed { .. } => {
                    let debug = completion_debug(console, &observation.snapshot, event.run_id)?;
                    write_completion_notice(console, None, debug.as_ref())?;
                }
                EventKind::Started if console.is_interactive() => {
                    console.hide_completion_notice()?;
                }
                EventKind::Failed(error) => outln!(console, "\nFailed: {}", safe_text(error)),
                EventKind::ModelRetry {
                    kind,
                    error,
                    attempt,
                    max_retries,
                    delay_ms,
                } => {
                    let (failure, retry) =
                        model_retry_parts(*kind, error, *attempt, *max_retries, *delay_ms);
                    console.write_model_retry(
                        event.run_id.into(),
                        *kind,
                        *attempt,
                        &[(failure, Some(Role::Error)), (retry, Some(Role::MutedText))],
                    )?;
                }
                EventKind::Interrupted if console.is_interactive() => {
                    if !interrupted_by_rejection {
                        console.write_response(&[(
                            format!(
                                "{}\n",
                                presentation::interrupted_notice(console.display_profile()?)
                            ),
                            Some(Role::MutedText),
                        )])?;
                    }
                }
                EventKind::Interrupted => outln!(console, "\nInterrupted"),
                EventKind::ToolStarted { name, input, .. } => outln!(
                    console,
                    "\nTool {} · {}",
                    safe_text(name),
                    safe_text(&input.to_string())
                ),
                EventKind::ToolCompleted { result, .. } => outln!(
                    console,
                    "\nTool {}: {}",
                    if result.is_error {
                        "failed"
                    } else {
                        "completed"
                    },
                    safe_text(&result.content)
                ),
                EventKind::PlanUpdated { plan } => console.write_plan(
                    &format!("Plan version {}:", plan.version),
                    &safe_text(&plan.markdown),
                )?,
                // The Todo list shows progress; only plain output writes it out.
                EventKind::ProgressUpdated { progress } if !console.is_interactive() => {
                    outln!(console, "\nProgress:");
                    for todo in &progress.todos {
                        outln!(console, "  {:?}: {}", todo.status, safe_text(&todo.content));
                    }
                }
                EventKind::PermissionChecked { decision, reason }
                    if *decision != Decision::Allow =>
                {
                    outln!(
                        console,
                        "\nAuto review: {decision:?} · {}",
                        safe_text(reason)
                    )
                }
                EventKind::ContextCompaction { status, error } => outln!(
                    console,
                    "\nContext compaction: {status:?} · {}",
                    safe_text(error.as_deref().unwrap_or(""))
                ),
                _ => {}
            }
        }
    }
    reconcile_consumed_input_echoes(console, &observation.snapshot)?;
    if !console.is_interactive() {
        outln!(console,);
        outln!(console, "Loop: {:?}", observation.snapshot.chat_loop.state);
    }
    for input in observation
        .snapshot
        .pending_inputs
        .iter()
        .filter(|input| !input.armed)
    {
        if console.is_interactive() {
            outln!(
                console,
                "Saved input awaiting manual continuation:\n{}",
                safe_text(&input.input)
            );
        } else {
            outln!(
                console,
                "Saved input awaiting manual continuation: Run {}\n{}",
                input.receipt.run_id.public(),
                safe_text(&input.input)
            );
        }
    }
    for request in observation
        .snapshot
        .permissions
        .iter()
        .filter(|_| !console.is_interactive())
    {
        outln!(
            console,
            "Permission request: {0} · {1}\nOperation: {2}\nUse /allow {0} [once|session|persistent] or /deny {0}.",
            shown_id(request.id),
            safe_text(&request.name),
            safe_text(&request.input.to_string()),
        );
    }
    for request in &observation.snapshot.questions {
        print_question(console, request)?;
    }
    for command in observation
        .snapshot
        .commands
        .iter()
        .filter(|_| !console.is_interactive())
    {
        print_command(console, command)?;
    }

    Ok(observation)
}

/// Lists a pending question for a View without a terminal; a terminal asks it in its dialog.
fn print_question(console: &Console, request: &QuestionRequest) -> Result<(), String> {
    if console.is_interactive() {
        return Ok(());
    }
    outln!(console, "\nQuestion: {}", shown_id(request.id));
    let total = request.questions.len();
    for (index, question) in request.questions.iter().enumerate() {
        for line in questions::question_lines(question, index, total) {
            outln!(console, "{line}");
        }
    }
    outln!(
        console,
        "Answer each question on its own line: an option number or label, numbers separated by commas where more than one is allowed, or your own text. /answer {} ANSWER answers its next question.",
        shown_id(request.id)
    );
    Ok(())
}

/// Answers the next question of `request` with typed text, in a View without a terminal.
async fn answer_typed_question(
    agent: &Agent,
    console: &Console,
    menus: &mut Menus,
    questions: &mut HashMap<uuid::Uuid, QuestionRequest>,
    request: &QuestionRequest,
    text: &str,
) -> Result<(), String> {
    match menus.answer_typed(agent, request, text).await {
        Ok(None) => {
            questions.remove(&request.id);
            outln!(console, "Answer accepted.");
        }
        Ok(Some(next_question)) => {
            outln!(console, "Answer recorded. Next question:");
            for line in next_question {
                outln!(console, "{line}");
            }
        }
        Err(error) => outln!(console, "Cannot answer question: {}", safe_text(&error)),
    }
    Ok(())
}

async fn parse_after_turn(
    agent: &Agent,
    chat: ChatId,
    command: &str,
) -> Result<AfterTurnAction, AccessError> {
    use tickloop_code_access::{AfterTurnAction, ConfigurationChange, HistoryAction, UserInput};

    let invalid = |message: &str| AccessError::invalid(message);
    let (name, arguments) = command
        .trim()
        .split_once(char::is_whitespace)
        .unwrap_or((command.trim(), ""));
    let arguments = arguments.trim();
    match name {
        "/compact" => Ok(AfterTurnAction::History {
            action: HistoryAction::Compact {
                instructions: (!arguments.is_empty()).then(|| arguments.to_owned()),
            },
        }),
        "/cwd" | "/add-dir" => {
            let mut snapshot = agent.snapshot(chat.into()).await?;
            let mut folder = snapshot.folder.clone().ok_or_else(|| {
                AccessError::new(
                    AccessErrorCode::Unsupported,
                    "this Agent has no working directory",
                )
            })?;
            if name == "/cwd" {
                folder = folder.join(arguments);
            } else {
                snapshot.additional_directories.push(folder.join(arguments));
            }
            Ok(AfterTurnAction::Configure {
                change: ConfigurationChange::Directories {
                    folder,
                    additional: snapshot.additional_directories,
                },
            })
        }
        "/rule" => {
            let fields = arguments.split_whitespace().collect::<Vec<_>>();
            let view = agent.permissions_view(chat.into()).await?;
            let change = if fields.len() == 2 && fields[0] == "remove" {
                PermissionChange::RemoveRule {
                    id: fields[1]
                        .parse()
                        .map_err(|_| invalid("rule ID must be a UUID"))?,
                }
            } else if (2..=3).contains(&fields.len()) {
                let decision = match fields[0] {
                    "allow" => Decision::Allow,
                    "ask" => Decision::Ask,
                    "deny" => Decision::Deny,
                    _ => return Err(invalid("rule decision must be allow, ask or deny")),
                };
                let persistent = match fields.get(2).copied().unwrap_or("session") {
                    "session" => false,
                    "persistent" => true,
                    _ => return Err(invalid("rule scope must be session or persistent")),
                };
                PermissionChange::UpsertRule {
                    rule: PermissionRule {
                        id: tickloop_ids::generate(),
                        decision,
                        tool: fields[1].into(),
                        specifier: None,
                        input: None,
                        loop_id: None,
                        persistent,
                    },
                }
            } else {
                return Err(invalid(
                    "use /rule allow|ask|deny TOOL [session|persistent], or /rule remove RULE_ID",
                ));
            };
            Ok(AfterTurnAction::Configure {
                change: ConfigurationChange::PermissionChange {
                    change,
                    expected: view.revision,
                },
            })
        }
        _ => {
            let skill = name
                .strip_prefix('/')
                .ok_or_else(|| invalid("after-turn actions must be explicit commands"))?;
            if !agent.has_skill(chat.into(), skill).await? {
                return Err(invalid("unknown after-turn command or Skill"));
            }
            Ok(AfterTurnAction::Submit {
                input: UserInput::Skill {
                    name: skill.into(),
                    arguments: arguments.into(),
                },
                origin: None,
            })
        }
    }
}

async fn command_control(
    console: &Console,
    agent: &Agent,
    chat: ChatId,
    command: &str,
) -> Result<(), String> {
    let fields = command.split_whitespace().collect::<Vec<_>>();
    if fields.len() < 2 || fields.len() > 4 {
        return Err("use /background OPERATION, /stop-command OPERATION, or /command-output OPERATION [OFFSET] [LIMIT]".into());
    }
    let operation = fields[1]
        .parse()
        .map_err(|_| "operation ID must be a UUID")?;
    if fields[0] == "/command-output" {
        let offset = fields
            .get(2)
            .map(|value| value.parse())
            .transpose()
            .map_err(|_| "offset must be a nonnegative integer")?
            .unwrap_or(0);
        let limit = fields
            .get(3)
            .map(|value| value.parse())
            .transpose()
            .map_err(|_| "limit must be a positive integer")?
            .unwrap_or(COMMAND_OUTPUT_BYTES);
        let output = agent
            .read_command(chat.into(), operation, offset, limit)
            .await
            .map_err(|error| error.to_string())?;
        outln!(console, "{}", safe_text(&output.content));
        if let Some(path) = output.output_ref {
            outln!(
                console,
                "Full output: {}",
                safe_text(&path.to_string_lossy())
            );
        }
    } else {
        if fields.len() != 2 {
            return Err("background and stop-command accept only an operation ID".into());
        }
        let state = if fields[0] == "/background" {
            agent.background_command(chat.into(), operation).await
        } else {
            agent.stop_command(chat.into(), operation).await
        }
        .map_err(|error| error.to_string())?;
        print_command(console, &state)?;
    }
    Ok(())
}

fn print_command(
    console: &Console,
    command: &tickloop_harness::protocol::CommandSnapshot,
) -> Result<(), String> {
    outln!(
        console,
        "\nCommand: {} · {:?} · PID {:?}\n{}\nDirectory: {}\nExit code: {:?} · Signal: {:?} · Output bytes: {}\nFull output: {}",
        command.operation_id,
        command.state,
        command.pid,
        safe_text(&command.command),
        safe_text(&command.cwd.to_string_lossy()),
        command.exit_code,
        command.signal,
        command.output_bytes,
        safe_text(&command.output_ref.to_string_lossy())
    );
    if let Some(reason) = &command.reason {
        outln!(console, "Reason: {}", safe_text(reason));
    }
    Ok(())
}

fn normalize_state_root(root: PathBuf) -> PathBuf {
    let mut existing = root.as_path();
    let mut missing = Vec::new();
    loop {
        if let Ok(mut resolved) = existing.canonicalize() {
            for name in missing.into_iter().rev() {
                resolved.push(name);
            }
            return resolved;
        }
        let Some(name) = existing.file_name() else {
            return root;
        };
        missing.push(name.to_owned());
        let Some(parent) = existing.parent() else {
            return root;
        };
        existing = if parent.as_os_str().is_empty() {
            std::path::Path::new(".")
        } else {
            parent
        };
    }
}

/// Shells and local management can create a Chat without choosing a model. Remember only those
/// new Chats, so their first model input is gated without affecting resumed history.
async fn ensure_chat_with_model_setup(
    console: &Console,
    agent: &Agent,
    menus: &mut Menus,
    selected: &mut Option<ChatId>,
    workdir: &std::path::Path,
) -> Result<uuid::Uuid, String> {
    let creating = selected.is_none_or(|chat| agent.first_create_pending(chat.into()));
    let result = ensure_chat(
        console,
        agent,
        selected,
        workdir,
        !menus.model_setup_pending(),
    )
    .await;
    if creating && let Some(chat) = *selected {
        menus.model_setup_created_chat(chat.into());
    }
    result
}

async fn ensure_chat(
    console: &Console,
    agent: &Agent,
    selected: &mut Option<ChatId>,
    workdir: &std::path::Path,
    model_selected: bool,
) -> Result<uuid::Uuid, String> {
    if let Some(id) = selected
        && !agent.first_create_pending((*id).into())
    {
        return Ok((*id).into());
    }
    let draft = agent
        .configuration(ConfigTarget::NewDraft)
        .await
        .map_err(|error| error.to_string())?;
    let startup_banner = if model_selected {
        startup_banner_spans(&draft.model, workdir)
    } else {
        startup_banner_with_model(MODEL_SETUP_PROMPT.into(), workdir)
    };
    let (id, unapplied) = match agent.create_from_draft(workdir.to_path_buf()).await {
        Ok(id) => (id, None),
        Err(DraftCreationError::NotCreated(error)) => {
            return Err(format!(
                "New Agent was not created: {}; input kept",
                safe_line(&error.to_string())
            ));
        }
        Err(DraftCreationError::RegisteredFailed {
            command_id,
            agent_id,
            error,
        }) => {
            let (command_id, agent_id) = (shown_id(command_id), shown_id(agent_id));
            return Err(format!(
                "Creation {command_id} failed after Agent {agent_id} was registered: {}; it received no input. Open it from Agents, or /acknowledge-create {command_id} to keep this draft and create again; input kept",
                safe_line(&error)
            ));
        }
        Err(DraftCreationError::Unapplied { chat, error }) => (
            chat,
            Some(format!(
                "New Agent created, but a staged choice could not be applied yet: {}; send again to continue. Input kept and not sent",
                safe_line(&error.to_string())
            )),
        ),
        Err(DraftCreationError::ChoiceUnknown { chat, command_id }) => (
            chat,
            Some(format!(
                "New Agent created; staged choice {0} has an unknown result and was not resent. Send again once its receipt is available, or /acknowledge-create {0} to cancel the remaining choices. Input kept and not sent",
                shown_id(command_id)
            )),
        ),
        Err(DraftCreationError::ChoiceRefused {
            chat,
            command_id,
            error,
        }) => (
            chat,
            Some(format!(
                "New Agent created, but staged choice {0} was refused: {1}. /acknowledge-create {0} cancels the remaining choices. Input kept and not sent",
                shown_id(command_id),
                safe_line(&error)
            )),
        ),
        Err(DraftCreationError::AttachUnavailable { chat, error }) => (
            chat,
            Some(format!(
                "New Agent created; local connection unavailable · {}; input kept, send again to attach",
                safe_line(&error.to_string())
            )),
        ),
        Err(DraftCreationError::Unconfirmed(command_id)) => {
            let command_id = shown_id(command_id);
            return Err(format!(
                "New Agent creation {command_id} is unconfirmed; nothing was resent. Send again after reconnecting to check its receipt; input kept"
            ));
        }
    };
    *selected = Some(id.into());
    console.select_created_chat(id, &startup_banner)?;
    let status = agent.status(id).await.map_err(|error| error.to_string())?;
    console.set_input_context(Some(id), workdir, &status.additional_directories)?;
    agent
        .display_chat(Some(id))
        .await
        .map_err(|error| error.to_string())?;
    if !console.is_interactive() {
        outln!(console, "Chat: {}", ChatId::from(id).public());
    }
    show_settings_warnings(console, agent.user_settings_warnings(id))?;
    if let Some(message) = unapplied {
        return Err(message);
    }
    Ok(id)
}

/// Echoes of accepted messages that may read as one block with the input merged just before
/// them. An echo continues its predecessor only when consecutive live events of one chat merged
/// both into the same run; any other event, a gap or an input without a confirmed echo ends the
/// block. Saved history and other Views never join.
#[derive(Default)]
struct MergedEchoes {
    /// Accepted message runs not yet merged, started or taken back, with the echo shown for each.
    accepted: HashMap<uuid::Uuid, InputEcho>,
    input_blocks: HashMap<uuid::Uuid, u64>,
    submission_block: u64,
    /// Per chat, the latest live merge a following merge may continue.
    open: HashMap<uuid::Uuid, MergeCursor>,
}

/// A chat's latest live InputMerged: the run it joined, its event sequence and the input's echo.
struct MergeCursor {
    into_run: RunId,
    sequence: u64,
    echo: InputEcho,
    submission_block: Option<u64>,
}

impl MergedEchoes {
    fn accepted(&mut self, run: RunId, echo: Option<InputEcho>) {
        self.input_blocks
            .entry(run.into())
            .or_insert(self.submission_block);
        self.consumed_echo(run, echo);
    }

    fn consumed_echo(&mut self, run: RunId, echo: Option<InputEcho>) {
        if let Some(echo) = echo {
            self.accepted.insert(run.into(), echo);
        }
    }

    /// Follows one event of a chat shown through `displayed_through`; returns the two echoes to
    /// show as one block, if any. An event a restore or reload already showed only ends the block.
    fn observe(
        &mut self,
        event: &tickloop_harness::protocol::Event,
        displayed_through: u64,
    ) -> Option<(InputEcho, InputEcho)> {
        let previous = self.open.remove(&event.chat_id.uuid());
        // A run merged, started, taken back or ended no longer needs its echo, even when a
        // restore already showed that event.
        let consumed = matches!(
            event.kind,
            EventKind::InputMerged { .. }
                | EventKind::Started
                | EventKind::InputTakenBack
                | EventKind::Completed { .. }
                | EventKind::Failed(_)
                | EventKind::Interrupted
        );
        let current = consumed
            .then(|| self.accepted.remove(&event.run_id.uuid()))
            .flatten();
        let submission_block = consumed
            .then(|| self.input_blocks.remove(&event.run_id.uuid()))
            .flatten();
        if event.sequence <= displayed_through {
            return None;
        }
        let EventKind::InputMerged { into_run_id } = &event.kind else {
            return None;
        };
        let current = current?;
        self.open.insert(
            event.chat_id.into(),
            MergeCursor {
                into_run: (*into_run_id),
                sequence: event.sequence,
                echo: current,
                submission_block,
            },
        );
        let previous = previous?;
        (previous.into_run == *into_run_id
            && previous.sequence + 1 == event.sequence
            && submission_block.is_some()
            && previous.submission_block == submission_block)
            .then_some((previous.echo, current))
    }

    /// Ends every open block, as after missed events or when the View shows something else.
    fn close_all(&mut self) {
        self.open.clear();
    }

    fn next_submission_block(&mut self) {
        self.submission_block = self.submission_block.wrapping_add(1);
    }
}

/// Selects, or creates from the New Agent draft, the Agent `input` goes to, and keeps the input's
/// echo where the input is: an attempt whose Agent could not be created or configured is restored
/// to the editor unsent, so its echo is withdrawn; an input that created its Agent is echoed in
/// that Agent's view instead of the draft. The inner result is the Agent, or why it is
/// unavailable for this input; the outer error is a failed Console update.
async fn ensure_chat_for_input(
    console: &Console,
    agent: &Agent,
    selected: &mut Option<ChatId>,
    workdir: &std::path::Path,
    retry_of: Option<uuid::Uuid>,
    input: &str,
    echo: &mut Option<InputEcho>,
) -> Result<Result<uuid::Uuid, String>, String> {
    let echoed_in = *selected;
    let creates_agent =
        retry_of.is_none() && echoed_in.is_none_or(|chat| agent.first_create_pending(chat.into()));
    let chat = match ensure_chat(console, agent, selected, workdir, true).await {
        Ok(chat) => chat,
        Err(error) => {
            if creates_agent && let Some(echo) = echo.take() {
                console.withdraw_echo(echoed_in.map(Into::into), echo)?;
                if *selected != echoed_in {
                    console.withdraw_echo(selected.map(Into::into), echo)?;
                }
            }
            return Ok(Err(error));
        }
    };
    if creates_agent && echoed_in != Some(chat.into()) {
        if let Some(echo) = echo.take() {
            console.withdraw_echo(echoed_in.map(Into::into), echo)?;
            console.withdraw_echo(Some(chat), echo)?;
        }
        *echo = console.record_input(input)?;
    }
    Ok(Ok(chat))
}

/// How an input left the View.
enum InputSend<T> {
    /// Sent now, or adopted from the earlier attempt `adopted` that this unchanged draft retried.
    Accepted {
        value: T,
        adopted: Option<uuid::Uuid>,
    },
    /// This input's acceptance is unknown; its restored draft retries it by this id.
    Unknown(uuid::Uuid, AccessError),
    /// An earlier input's acceptance is still unknown, so this one was not sent.
    Held(AccessError),
    Failed(AccessError),
}

/// Sends an input to `chat`. For an unchanged restored draft (`retry_of`), it first reads the
/// earlier attempt's receipt: an accepted attempt is adopted and nothing is sent, an unknown one
/// stays held, and a refused one lets this input go out as new.
async fn send_input<T>(
    agent: &Agent,
    chat: ChatId,
    retry_of: Option<uuid::Uuid>,
    send: impl std::future::Future<Output = Result<T, AccessError>>,
    adopt: impl FnOnce(CommandResult) -> Option<T>,
) -> InputSend<T> {
    if let Some(id) = retry_of {
        match agent.resolve_input(chat.into(), id).await {
            InputResolution::Accepted(result) => {
                return match adopt(*result) {
                    Some(value) => InputSend::Accepted {
                        value,
                        adopted: Some(id),
                    },
                    None => InputSend::Failed(AccessError::invalid(format!(
                        "Input {} was accepted as another kind of input; nothing was resent",
                        shown_id(id)
                    ))),
                };
            }
            InputResolution::Unknown => {
                return InputSend::Unknown(
                    id,
                    AccessError::invalid(format!(
                        "Input {0} still has an unknown acceptance result; nothing was resent. /acknowledge-input {0} releases it without resending",
                        shown_id(id)
                    )),
                );
            }
            InputResolution::Refused | InputResolution::Released => {}
        }
    }
    let before = agent.unknown_input(chat.into());
    match send.await {
        Ok(value) => InputSend::Accepted {
            value,
            adopted: None,
        },
        Err(error) => match agent.unknown_input(chat.into()) {
            Some(id) if Some(id) != before => InputSend::Unknown(id, error),
            Some(_) => InputSend::Held(error),
            None => InputSend::Failed(error),
        },
    }
}

fn submitted_receipt(result: CommandResult) -> Option<Receipt> {
    match result {
        CommandResult::Submitted(receipt) => Some(receipt),
        _ => None,
    }
}

fn input_adopted(id: uuid::Uuid) -> String {
    format!(
        "Input {} was already accepted; nothing was resent",
        shown_id(id)
    )
}

fn consumed_input_text(
    snapshot: &ChatSnapshot,
    input: uuid::Uuid,
) -> Option<std::borrow::Cow<'_, str>> {
    let trigger = snapshot.trigger_origins.get(&tickloop_ids::Id::from(input));
    if matches!(
        trigger,
        Some(tickloop_harness::protocol::TriggerIdentity::GoalContinuation { .. })
    ) {
        return None;
    }
    snapshot
        .messages
        .iter()
        .enumerate()
        .find_map(|(index, message)| {
            if snapshot.message_runs.get(&index) != Some(&tickloop_ids::Id::from(input))
                || message.role != tickloop_harness::protocol::Role::User
                || snapshot.host_notes.contains_key(&index)
            {
                return None;
            }
            if snapshot
                .message_ids
                .get(&index)
                .is_some_and(|id| snapshot.agent_message_origins.contains_key(id))
            {
                return None;
            }
            message.content.iter().find_map(|content| match content {
                tickloop_harness::protocol::Content::Text { text }
                    if presentation::typed_local_message(text, false).is_none()
                        && !conversation::is_interruption_placeholder(message.role, text) =>
                {
                    Some(trigger.map_or_else(
                        || std::borrow::Cow::Borrowed(text.as_str()),
                        |trigger| trigger.display_input(text),
                    ))
                }
                _ => None,
            })
        })
}

fn show_consumed_family_input(
    console: &Console,
    snapshot: &ChatSnapshot,
    run: RunId,
) -> Result<(), String> {
    for (index, message) in snapshot.messages.iter().enumerate() {
        let Some(id) = snapshot.message_ids.get(&index).filter(|id| {
            snapshot.agent_message_origins.contains_key(id)
                || snapshot.human_family_origins.contains_key(id)
        }) else {
            continue;
        };
        if snapshot.message_runs.get(&index) != Some(&run)
            || console.input_is_echoed(snapshot.id.into(), *id)?
        {
            continue;
        }
        if let Some(origin) = snapshot.human_family_origins.get(id) {
            console.write_roles(&[(
                format!(
                    "\nUser message · from @{} View\n",
                    safe_text(&origin.source_handle)
                ),
                Some(Role::MutedText),
            )])?;
            for content in &message.content {
                if let tickloop_harness::protocol::Content::Text { text } = content {
                    console.write(&format!("{}\n", safe_text(text)))?;
                }
            }
        }
        let mut awaits_completion = false;
        let agent_texts = message
            .content
            .iter()
            .filter(|_| snapshot.agent_message_origins.contains_key(id))
            .filter_map(|content| match content {
                tickloop_harness::protocol::Content::Text { text } => Some(text),
                _ => None,
            });
        for text in agent_texts {
            let Some(local) = presentation::typed_local_message(text, true) else {
                continue;
            };
            match agent_messaging::agent_input(snapshot, Some(*id), &local) {
                AgentInput::Result(completion) => {
                    show_family_completion(console, completion, snapshot)?;
                }
                AgentInput::PendingResult => awaits_completion = true,
                AgentInput::Message => {
                    if let Some(message) = AgentMessage::of(&local) {
                        write_block(
                            console,
                            agent_messaging::message_runs(
                                &message,
                                presentation::OutputDetail::Preview,
                                console.display_profile()?,
                            ),
                        )?;
                    }
                }
            }
        }
        // A report whose run has not ended shows with the child's completion.
        if !awaits_completion {
            console.mark_input_echoes(snapshot.id.into(), [*id])?;
        }
    }
    Ok(())
}

/// Shows a background child's completion once, as one block with the report it delivered;
/// whichever of the two reaches this View first draws it. A foreground child's report is under
/// its call. A notice that later failed to reach the parent adds that line on its own.
fn show_family_completion(
    console: &Console,
    completion: &tickloop_harness::protocol::FamilyCompletion,
    snapshot: &ChatSnapshot,
) -> Result<(), String> {
    if !completion.notification.is_background() {
        console.mark_input_echoes(snapshot.id.into(), [completion.child_run_id.uuid()])?;
        return Ok(());
    }
    if console.input_is_echoed(snapshot.id.into(), completion.child_run_id.into())? {
        if let tickloop_harness::protocol::SubagentNotificationDelivery::Rejected(rejection) =
            &completion.notification
        {
            console.write_roles(&[(
                format!(
                    "\nNotification for @{} not delivered: {}\n",
                    safe_line(&completion.handle),
                    safe_line(&rejection.message)
                ),
                Some(Role::MutedText),
            )])?;
        }
        return Ok(());
    }
    write_block(
        console,
        agent_messaging::completion_runs(
            completion,
            snapshot,
            &console.agent_colors()?,
            presentation::OutputDetail::Preview,
            console.display_profile()?,
        ),
    )?;
    console.mark_input_echoes(snapshot.id.into(), [completion.child_run_id.uuid()])
}

/// An Agent block: its heading and the body indented under it.
fn write_block(
    console: &Console,
    (heading, body): (presentation::RoleRuns, presentation::RoleRuns),
) -> Result<(), String> {
    let mut pieces = console::ConversationPiece::block_runs(&heading);
    if !body.is_empty() {
        pieces.push(console::ConversationPiece::Body(body));
    }
    console.write_pieces(&pieces)
}

/// Learns the Agent Colors of the Agents `snapshot`'s conversation names that this View has not
/// seen yet, from the Agent catalog.
async fn learn_agent_colors(
    console: &Console,
    agent: &Agent,
    snapshot: &ChatSnapshot,
) -> Result<(), String> {
    let known = console.agent_colors()?;
    let unknown = agent_messaging::named_agents(snapshot)
        .into_iter()
        .filter(|chat| !known.contains_key(&chat.uuid()))
        .collect::<Vec<_>>();
    if unknown.is_empty() {
        return Ok(());
    }
    let Ok(rows) = agent.catalog().await else {
        return Ok(());
    };
    let mut colors = agent_messaging::catalog_colors(&rows);
    for chat in unknown {
        colors.entry(chat.into()).or_insert(None);
    }
    console.remember_agent_colors(colors)
}

async fn track_accepted_echo(
    console: &Console,
    agent: &Agent,
    receipt: &Receipt,
    echo: Option<InputEcho>,
) -> Result<Option<InputEcho>, String> {
    if !console.is_interactive() {
        return Ok(echo);
    }
    let snapshot = agent.snapshot(receipt.chat_id.into()).await.ok();
    // The input is listed where the Agent queued it, or shown as sent when its turn already took
    // it, in one frame that also drops the provisional queue entry or echo it had.
    let _frame_hold = console.hold_frames()?;
    if let Some(snapshot) = &snapshot {
        console.set_queued_inputs(receipt.chat_id.into(), snapshot.queued_inputs())?;
        if consumed_input_text(snapshot, receipt.run_id.into()).is_some() {
            if echo.is_some() {
                console.mark_input_echoes(receipt.chat_id.into(), [receipt.run_id.uuid()])?;
                if let Some(skill) = snapshot.user_skill_invocations.get(&receipt.run_id) {
                    console.record_skill_invocation(
                        receipt.chat_id.into(),
                        receipt.run_id.into(),
                        skill,
                    )?;
                }
                return Ok(echo);
            }
            return consume_input_echo_from_snapshot(console, snapshot, receipt.run_id.into());
        }
    }
    if let Some(echo) = echo {
        console.withdraw_echo(Some(receipt.chat_id.into()), echo)?;
    }
    Ok(None)
}

async fn show_consumed_input(
    console: &Console,
    agent: &Agent,
    turn_snapshot: &mut TurnSnapshot,
    event: &tickloop_harness::protocol::Event,
) -> Result<Option<InputEcho>, String> {
    if console.input_is_echoed(event.chat_id.into(), event.run_id.into())? {
        return Ok(None);
    }
    if console.is_interactive() {
        console.defer_input_echo(event.chat_id.into(), event.run_id.into())?;
    }
    let Ok(snapshot) = turn_snapshot.read(agent, event.chat_id).await else {
        return Ok(None);
    };
    if !console.is_interactive()
        && !matches!(
            snapshot.trigger_origins.get(&event.run_id),
            Some(tickloop_harness::protocol::TriggerIdentity::GoalContinuation { .. })
        )
    {
        return Ok(None);
    }
    consume_input_echo_from_snapshot(console, &snapshot, event.run_id.into())
}

fn consume_input_echo_from_snapshot(
    console: &Console,
    snapshot: &ChatSnapshot,
    input: uuid::Uuid,
) -> Result<Option<InputEcho>, String> {
    if console.input_is_echoed(snapshot.id.into(), input)? {
        return Ok(None);
    }
    if matches!(
        snapshot.trigger_origins.get(&tickloop_ids::Id::from(input)),
        Some(tickloop_harness::protocol::TriggerIdentity::GoalContinuation { .. })
    ) {
        let _frame_hold = console.hold_frames()?;
        console.set_queued_inputs(snapshot.id.into(), snapshot.queued_inputs())?;
        console.flash_trigger_input(snapshot, input)?;
        console.write_roles(&goal_view::continuation_runs())?;
        console.mark_input_echoes(snapshot.id.into(), [input])?;
        return Ok(None);
    }
    let deliveries = snapshot
        .messages
        .iter()
        .enumerate()
        .filter(|(index, message)| {
            snapshot.message_runs.get(index) == Some(&tickloop_ids::Id::from(input))
                && message.role == tickloop_harness::protocol::Role::User
                && !snapshot.host_notes.contains_key(index)
        })
        .flat_map(|(_, message)| &message.content)
        .filter_map(|content| match content {
            tickloop_harness::protocol::Content::Text { text } => {
                presentation::typed_local_message(text, false)
            }
            _ => None,
        })
        .filter(|message| {
            matches!(
                message,
                tickloop_harness::protocol::LocalMessage::ArtifactRequest { .. }
                    | tickloop_harness::protocol::LocalMessage::TickDelivery { .. }
            )
        })
        .collect::<Vec<_>>();
    if !deliveries.is_empty() {
        console.record_local_deliveries(snapshot, input, &deliveries)?;
        return Ok(None);
    }
    match consumed_input_text(snapshot, input) {
        Some(text) => {
            // The input leaves the queue in the frame that shows it sent.
            let _frame_hold = console.hold_frames()?;
            console.set_queued_inputs(snapshot.id.into(), snapshot.queued_inputs())?;
            console.flash_trigger_input(snapshot, input)?;
            let echo =
                console.record_consumed_input(snapshot.id.into(), input, &safe_text(&text))?;
            if let Some(skill) = snapshot.user_skill_invocations.get(&input.into()) {
                console.record_skill_invocation(snapshot.id.into(), input, skill)?;
            }
            Ok(echo)
        }
        None => {
            let mut has_unshown_agent_message = false;
            for (index, id) in &snapshot.message_ids {
                if snapshot.message_runs.get(index) == Some(&tickloop_ids::Id::from(input))
                    && snapshot.agent_message_origins.contains_key(id)
                    && !console.input_is_echoed(snapshot.id.into(), *id)?
                {
                    has_unshown_agent_message = true;
                    break;
                }
            }
            if !has_unshown_agent_message && snapshot.message_runs.values().any(|run| *run == input)
            {
                console.finish_deferred_input_echo(snapshot.id.into(), input)?;
            }
            Ok(None)
        }
    }
}

fn reconcile_consumed_input_echoes(
    console: &Console,
    snapshot: &ChatSnapshot,
) -> Result<(), String> {
    let mut inputs = console.awaiting_input_echoes(snapshot.id.into())?;
    inputs.sort_by_key(|input| {
        snapshot
            .message_runs
            .iter()
            .find_map(|(index, run)| (*run == *input).then_some(*index))
            .unwrap_or(usize::MAX)
    });
    for input in inputs {
        show_consumed_family_input(console, snapshot, input.into())?;
        consume_input_echo_from_snapshot(console, snapshot, input)?;
    }
    Ok(())
}

/// Follows an accepted run. An adopted run may have ended while its acceptance was unknown, so
/// its state comes from the Agent rather than from the receipt.
async fn track_accepted_input(
    console: &Console,
    agent: &Agent,
    receipt: Receipt,
    adopted: Option<uuid::Uuid>,
    pending: &mut HashMap<uuid::Uuid, Receipt>,
    permissions: &mut HashMap<uuid::Uuid, PermissionRequestOwner>,
    questions: &mut HashMap<uuid::Uuid, QuestionRequest>,
) -> Result<(), String> {
    let Some(id) = adopted else {
        pending.insert(receipt.run_id.into(), receipt);
        return Ok(());
    };
    outln!(console, "{}", input_adopted(id));
    if let Ok(snapshot) = agent.snapshot(receipt.chat_id.into()).await {
        sync_snapshot(&snapshot, pending, permissions, questions);
    }
    Ok(())
}

enum RuleCommand {
    Remove(uuid::Uuid),
    Add(PermissionRule),
}

fn parse_rule_command(command: &str) -> Result<RuleCommand, String> {
    let fields = command.split_whitespace().collect::<Vec<_>>();
    if fields.len() == 3 && fields[1] == "remove" {
        return tickloop_ids::base62::decode_either(fields[2])
            .map(RuleCommand::Remove)
            .ok_or_else(|| format!("{} is not a rule ID", fields[2]));
    }
    if !(3..=4).contains(&fields.len()) {
        return Err(
            "use /rule allow|ask|deny TOOL [session|persistent], or /rule remove UUID".into(),
        );
    }
    let decision = match fields[1] {
        "allow" => Decision::Allow,
        "ask" => Decision::Ask,
        "deny" => Decision::Deny,
        _ => return Err("rule decision must be allow, ask or deny".into()),
    };
    let persistent = match fields.get(3).copied().unwrap_or("session") {
        "session" => false,
        "persistent" => true,
        _ => return Err("rule scope must be session or persistent".into()),
    };
    Ok(RuleCommand::Add(PermissionRule {
        id: tickloop_ids::generate(),
        decision,
        tool: fields[2].into(),
        specifier: None,
        input: None,
        loop_id: None,
        persistent,
    }))
}

fn rule_added_message(rule: &PermissionRule, scope: &str) -> String {
    format!(
        "Rule added: {} · {:?} · {} · {} · {scope}",
        shown_id(rule.id),
        rule.decision,
        safe_text(&rule.tool),
        if rule.persistent {
            "persistent"
        } else {
            "session"
        }
    )
}

async fn update_rule(agent: &Agent, chat: ChatId, command: &str) -> Result<String, String> {
    let command = parse_rule_command(command)?;
    let view = agent
        .permissions_view(chat.into())
        .await
        .map_err(|error| error.to_string())?;
    match command {
        RuleCommand::Remove(id) => {
            if !view
                .rules
                .iter()
                .any(|entry| entry.rule.id == id && entry.editable)
            {
                return Err("rule does not exist".into());
            }
            agent
                .apply_permission_change(
                    chat.into(),
                    PermissionChange::RemoveRule { id },
                    view.revision,
                )
                .await
                .map_err(|error| error.to_string())?;
            Ok(format!("Rule removed: {}", shown_id(id)))
        }
        RuleCommand::Add(rule) => {
            let message = rule_added_message(&rule, "all inputs in this Chat");
            agent
                .apply_permission_change(
                    chat.into(),
                    PermissionChange::UpsertRule { rule },
                    view.revision,
                )
                .await
                .map_err(|error| error.to_string())?;
            Ok(message)
        }
    }
}

/// A `/rule` on the New Agent: persistent rules are saved for its project at once, as on an
/// Agent; session rules wait for the first input to create the Agent.
fn stage_draft_rule(
    agent: &Agent,
    project: &std::path::Path,
    command: &str,
) -> Result<String, String> {
    let (change, saved_message, staged_message) = match parse_rule_command(command)? {
        RuleCommand::Remove(id) => (
            PermissionChange::RemoveRule { id },
            format!("Rule removed: {}", shown_id(id)),
            format!("Rule removed from the New Agent: {}", shown_id(id)),
        ),
        RuleCommand::Add(rule) => {
            let saved = rule_added_message(&rule, "saved for this project");
            let staged =
                rule_added_message(&rule, "applies when the first input creates the New Agent");
            (PermissionChange::UpsertRule { rule }, saved, staged)
        }
    };
    match agent
        .save_project_rule_change(project, &change, None)
        .map_err(|error| error.to_string())?
    {
        RuleSave::Saved => return Ok(saved_message),
        RuleSave::Conflict => {
            return Err(
                "saved rules changed elsewhere while saving; nothing was written, try again".into(),
            );
        }
        RuleSave::NotSaved => {}
    }
    agent
        .stage_rule_change(change)
        .map_err(|error| error.to_string())?;
    Ok(staged_message)
}

async fn show_directory_listing(
    console: &Console,
    agent: &Agent,
    target: ConfigTarget,
    folder: &std::path::Path,
    additional: &[PathBuf],
) -> Result<(), String> {
    let sources = agent.directory_sources(target, folder).await;
    outln!(
        console,
        "Working directory: {}",
        safe_text(&folder.to_string_lossy())
    );
    for directory in additional {
        let source = sources
            .as_ref()
            .map(|sources| permissions_surface::directory_source_label(sources, directory))
            .unwrap_or_else(|_| "source unavailable".into());
        outln!(
            console,
            "Additional directory: {} · {}",
            safe_text(&directory.to_string_lossy()),
            source
        );
    }
    Ok(())
}

/// `/cwd PATH` and `/add-dir PATH` on the New Agent: the directory new Agents start in and the
/// extra directories the first input's Agent receives, resolved against the dispatch directory.
async fn stage_draft_directories(
    agent: &Agent,
    global: &mut crate::agent_console::AgentConsole,
    line: &str,
) -> Result<(PathBuf, Vec<PathBuf>), String> {
    let directory = global.projection().dispatch_directory;
    let resolve = |path: &str| -> Result<PathBuf, String> {
        let path = directory.join(path.trim());
        path.canonicalize()
            .ok()
            .filter(|path| path.is_dir())
            .ok_or_else(|| format!("{} is not a directory", path.display()))
    };
    if let Some(path) = line.strip_prefix("/cwd ") {
        let directory = resolve(path)?;
        global
            .interpret(crate::agent_console::Intent::DispatchDirectory(
                directory.clone(),
            ))
            .map_err(|error| error.to_string())?;
        agent.set_new_agent_directory(directory.clone());
        let additional = agent
            .configuration(ConfigTarget::NewDraft)
            .await
            .map_err(|error| error.to_string())?
            .additional_directories;
        return Ok((directory, additional));
    }
    let mut additional = agent
        .configuration(ConfigTarget::NewDraft)
        .await
        .map_err(|error| error.to_string())?
        .additional_directories;
    if let Some(path) = line.strip_prefix("/add-dir ") {
        additional.push(resolve(path)?);
        additional.sort();
        additional.dedup();
        agent
            .change_configuration(
                ConfigTarget::NewDraft,
                ConfigurationChange::Directories {
                    folder: directory.clone(),
                    additional: additional.clone(),
                },
            )
            .await
            .map_err(|error| error.to_string())?;
    }
    Ok((directory, additional))
}

/// Finds the Chat owning a request id among all Chats loaded in this process; if none is found, the
/// foreground Chat reports it.
async fn answer_permission(
    agent: &Agent,
    selected_chat: Option<ChatId>,
    command: &str,
) -> Result<uuid::Uuid, String> {
    let fields = command.split_whitespace().collect::<Vec<_>>();
    if !(2..=3).contains(&fields.len()) {
        return Err("use /allow REQUEST [once|session|persistent] or /deny REQUEST".into());
    }
    let request = tickloop_ids::base62::decode_either(fields[1])
        .ok_or_else(|| format!("{} is not a permission request ID", fields[1]))?;
    let scope = match fields.get(2).copied().unwrap_or("once") {
        "once" => PermissionScope::Once,
        "session" => PermissionScope::Session,
        "persistent" => PermissionScope::Persistent,
        _ => return Err("scope must be once, session or persistent".into()),
    };
    let chat = agent
        .list_chats()
        .await
        .into_iter()
        .find(|snapshot| {
            snapshot
                .permissions
                .iter()
                .any(|pending| pending.id == request)
        })
        .map(|snapshot| snapshot.conversation.id)
        .or(selected_chat)
        .ok_or("no Chat is selected")?;
    agent
        .answer_permission_with_scope(chat.into(), request, fields[0] == "/allow", scope)
        .await
        .map_err(|error| error.to_string())?;
    Ok(request)
}

async fn open_chat(
    agent: &Agent,
    command: &str,
    current_chat: Option<ChatId>,
) -> Result<tickloop_harness::protocol::ChatSnapshot, String> {
    let fields = command.split_whitespace().collect::<Vec<_>>();
    let [_, chat] = fields.as_slice() else {
        return Err("use /resume [CHAT]".into());
    };
    let chat =
        ChatId::parse(chat).ok_or("not a Chat ID; use /resume to choose a saved conversation")?;
    if current_chat == Some(chat) {
        return Err(format!("Chat {} is already open.", chat.public()));
    }
    let snapshot = agent
        .resume_chat(chat.into())
        .await
        .map_err(|error| match error.code {
            AccessErrorCode::NotFound => format!("Chat {} was not found.", chat.public()),
            _ => format!("Failed to resume Chat {}: {error}", chat.public()),
        })?;
    Ok(snapshot)
}

/// User settings warnings shown most recently. Settings are read at startup and on every new or
/// resumed Chat, and the same set of warnings is shown only once; cleared once settings become
/// valid again, so a later failure shows them again.
static SHOWN_SETTINGS_WARNINGS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

fn show_settings_warnings(console: &Console, warnings: Vec<String>) -> Result<(), String> {
    let mut shown = SHOWN_SETTINGS_WARNINGS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for warning in warnings.iter().filter(|warning| !shown.contains(warning)) {
        write_warning(console, warning)?;
    }
    *shown = warnings;
    Ok(())
}

/// A finished compaction as Claude Code marks it: one muted `⏺ Conversation compacted` line, with
/// the summary left to the detailed transcript.
fn write_compaction_notice(console: &Console, chat: ChatId, summary: &str) -> Result<(), String> {
    if console.is_interactive() {
        console.write_roles(&[(
            format!(
                "\n{} Conversation compacted (ctrl+o for history)\n",
                console.display_profile()?.glyph(Glyph::TurnSummary)
            ),
            Some(Role::MutedText),
        )])
    } else {
        console.write(&format!(
            "Context compacted for Chat {}. Earlier messages stay in the detailed transcript and are no longer sent to the model.\n{}\n",
            chat.public(),
            safe_text(summary)
        ))
    }
}

fn write_warning(console: &Console, warning: &str) -> Result<(), String> {
    if console.is_interactive() {
        console.write_roles(&[
            ("\n".into(), None),
            (
                console.display_profile()?.glyph(Glyph::Warning).into(),
                Some(Role::Warning),
            ),
            (format!(" {}\n", safe_line(warning)), Some(Role::Warning)),
        ])?;
    } else {
        outln!(console, "Warning: {}", safe_line(warning));
    }
    Ok(())
}

/// Writes the history of the Chat just brought to the foreground into the view. Like Claude Code,
/// no line follows the history: the input itself invites the next message, and sending it still
/// resumes receiving Triggers when they were paused.
fn show_opened_chat(
    console: &Console,
    agent: &Agent,
    snapshot: &tickloop_harness::protocol::ChatSnapshot,
) -> Result<(), String> {
    ensure_snapshot_banner(console, snapshot)?;
    console.reset_chat(snapshot.id.into())?;
    if !console.is_interactive() {
        outln!(console, "Chat: {}", snapshot.id.public());
    }
    for warning in agent.transcript_load_warnings(snapshot.id.into()) {
        write_warning(console, &warning)?;
    }
    show_settings_warnings(console, agent.user_settings_warnings(snapshot.id.into()))?;
    workflows::write_public_history(console, snapshot)
}

/// What a background notice calls an Agent before this View has received its snapshot.
const UNLABELED_BACKGROUND_AGENT: &str = "Background Agent · label unavailable · open Agents";

/// A background Agent as notices name it. Pipe output keeps the Chat identifier that scripts
/// match against its `Chat:` line; the interactive View names the Agent by its title.
fn background_label(agent: &Agent, console: &Console, chat: ChatId) -> String {
    if console.is_interactive() {
        readable_background_label(agent, console, chat)
    } else {
        format!("Chat {}", chat.public())
    }
}

/// The title this View last saw for `chat`, with the reading number its rows carry; never a
/// generated identifier.
fn readable_background_label(agent: &Agent, console: &Console, chat: ChatId) -> String {
    agent
        .cached_title(chat.into())
        .and_then(|(agent_id, title)| console.agent_name(agent_id, &safe_line(&title)).ok())
        .unwrap_or_else(|| UNLABELED_BACKGROUND_AGENT.into())
}

const REPLY_COMPLETED_NOTICE: &str = "reply completed";

/// Notice shown in the status bar when a background Chat's run ends; not written into the
/// foreground conversation.
#[derive(Clone, Debug, Eq, PartialEq)]
struct BackgroundRunNotice {
    message: String,
    compact_message: Option<String>,
    necessary: bool,
}

impl BackgroundRunNotice {
    fn text(&self, label: &str) -> String {
        format!("{label} · {}", self.message)
    }
}

fn background_run_notice(event: &tickloop_harness::protocol::Event) -> Option<BackgroundRunNotice> {
    let (message, necessary) = match &event.kind {
        EventKind::Completed { .. } => (REPLY_COMPLETED_NOTICE.to_owned(), false),
        EventKind::Failed(error) => (format!("failed: {}", safe_line(error)), true),
        EventKind::Interrupted => ("interrupted".to_owned(), false),
        _ => return None,
    };
    Some(BackgroundRunNotice {
        message,
        compact_message: None,
        necessary,
    })
}

/// Single-line notice printed in pipe mode when a background Chat raises a permission request or
/// question.
fn background_request_notice(
    event: &tickloop_harness::protocol::Event,
    label: &str,
) -> Option<String> {
    let request = match &event.kind {
        EventKind::PermissionRequested { request } => {
            format!("permission request {}", shown_id(request.id))
        }
        EventKind::QuestionAsked { request } => format!("question {}", shown_id(request.id)),
        _ => return None,
    };
    Some(format!("{label} · {request}"))
}

/// Pipe mode prints background Chat notices, while interactive mode keeps run-end notices for the
/// status bar; returns whether the status bar needs a refresh.
fn announce_background_event(
    console: &Console,
    event: &tickloop_harness::protocol::Event,
    label: &str,
    background_notice: &mut Option<(ChatId, BackgroundRunNotice)>,
) -> Result<bool, String> {
    let interactive = console.is_interactive();
    if let Some(notice) = background_request_notice(event, label) {
        if !interactive {
            outln!(console, "Background {notice}");
        }
        return Ok(true);
    }
    let Some(notice) = background_run_notice(event) else {
        return Ok(false);
    };
    if !interactive {
        outln!(console, "Background {}", notice.text(label));
    }
    *background_notice = Some((event.chat_id, notice));
    Ok(true)
}

/// Tracks pending runs and pending requests for all Chats, whether or not the event comes from the
/// foreground Chat. Runs waiting on a permission or question always count as pending, matching how
/// `sync_snapshot` restores them from a snapshot.
fn track_run_requests(
    event: &tickloop_harness::protocol::Event,
    pending: &mut HashMap<uuid::Uuid, Receipt>,
    permissions: &mut HashMap<uuid::Uuid, PermissionRequestOwner>,
    questions: &mut HashMap<uuid::Uuid, QuestionRequest>,
) {
    let receipt = || Receipt {
        chat_id: event.chat_id,
        loop_id: event.loop_id,
        run_id: event.run_id,
    };
    match &event.kind {
        EventKind::Completed { .. } | EventKind::Failed(_) | EventKind::Interrupted => {
            permissions.retain(|_, request| request.run_id != Some(event.run_id));
            questions.retain(|_, question| question.run_id != event.run_id);
            pending.remove(&event.run_id.uuid());
        }
        EventKind::InputMerged { .. } | EventKind::InputTakenBack => {
            pending.remove(&event.run_id.uuid());
        }
        EventKind::PermissionRequested { request } => {
            if let Some(run_id) = request.execution.run_id() {
                pending.entry(run_id.into()).or_insert_with(receipt);
            }
            permissions.insert(
                request.id,
                PermissionRequestOwner::new(event.chat_id, request),
            );
        }
        EventKind::QuestionAsked { request } => {
            pending.entry(event.run_id.into()).or_insert_with(receipt);
            questions.insert(request.id, request.clone());
        }
        _ => {}
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BackgroundWaitKind {
    Approval,
    Answer,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BackgroundWait {
    chat_id: ChatId,
    request_id: uuid::Uuid,
    kind: BackgroundWaitKind,
}

/// Requests awaiting the user in a non-foreground Chat; permission requests take precedence over
/// questions.
fn background_wait(
    selected_chat: Option<ChatId>,
    permissions: &HashMap<uuid::Uuid, PermissionRequestOwner>,
    questions: &HashMap<uuid::Uuid, QuestionRequest>,
) -> Option<BackgroundWait> {
    let approval = permissions.iter().find_map(|(request_id, request)| {
        let chat_id = request.chat_id;
        (Some(chat_id) != selected_chat).then_some(BackgroundWait {
            chat_id,
            request_id: *request_id,
            kind: BackgroundWaitKind::Approval,
        })
    });
    approval.or_else(|| {
        questions
            .values()
            .find(|question| Some(question.chat_id) != selected_chat)
            .map(|question| BackgroundWait {
                chat_id: question.chat_id,
                request_id: question.id,
                kind: BackgroundWaitKind::Answer,
            })
    })
}

/// A run that stopped being waited for because the daemon reports its execution Lost.
struct LostRunNotice {
    chat: ChatId,
    run: RunId,
    text: String,
}

fn execution_connection_notice(
    rows: &[AgentSummary],
    chat: ChatId,
    disconnected: bool,
    cached_completed: bool,
    daemon_unreachable: bool,
) -> Option<console::ExecutionConnectionNotice> {
    if rows
        .iter()
        .any(|row| row.chat_id == chat && row.availability == ExecutionAvailability::Lost)
    {
        Some(if cached_completed {
            console::ExecutionConnectionNotice::LostHistory
        } else {
            console::ExecutionConnectionNotice::Lost
        })
    } else if disconnected {
        Some(console::ExecutionConnectionNotice::CachedExecution)
    } else if daemon_unreachable {
        Some(console::ExecutionConnectionNotice::CachedView)
    } else {
        None
    }
}

/// Stops waiting for runs whose execution the daemon reports Lost; their outcome is unknown and
/// nothing is resent.
fn end_runs_of_lost_executions(
    rows: &[AgentSummary],
    pending: &mut HashMap<uuid::Uuid, Receipt>,
    permissions: &mut HashMap<uuid::Uuid, PermissionRequestOwner>,
    questions: &mut HashMap<uuid::Uuid, QuestionRequest>,
) -> Vec<LostRunNotice> {
    let lost = rows
        .iter()
        .filter(|row| row.availability == ExecutionAvailability::Lost)
        .map(|row| row.chat_id)
        .collect::<HashSet<_>>();
    permissions.retain(|_, request| !lost.contains(&request.chat_id.uuid()));
    questions.retain(|_, request| !lost.contains(&request.chat_id.uuid()));
    let ended = pending
        .values()
        .filter(|receipt| lost.contains(&receipt.chat_id.uuid()))
        .cloned()
        .collect::<Vec<_>>();
    ended
        .into_iter()
        .map(|receipt| {
            pending.remove(&receipt.run_id.uuid());
            permissions.retain(|_, request| request.run_id != Some(receipt.run_id));
            questions.retain(|_, question| question.run_id != receipt.run_id);
            LostRunNotice {
                chat: receipt.chat_id,
                run: receipt.run_id,
                text: format!(
                    "Run {} has an unknown outcome: its Agent execution was lost; nothing was resent. /resume {} loads its history",
                    receipt.run_id.public(), receipt.chat_id.public()
                ),
            }
        })
        .collect()
}

/// The message the process exits with when input ends while a request is still waiting; it names
/// the owning Chat only when that Chat is in the background.
fn input_closed_while_waiting(
    selected_chat: Option<ChatId>,
    permissions: &HashMap<uuid::Uuid, PermissionRequestOwner>,
    questions: &HashMap<uuid::Uuid, QuestionRequest>,
) -> Option<String> {
    let is_foreground_permission =
        |request: &PermissionRequestOwner| Some(request.chat_id) == selected_chat;
    if permissions.values().any(is_foreground_permission) {
        return Some(
            "input closed while waiting for permission; no pending operation was approved".into(),
        );
    }
    let wait = background_wait(selected_chat, permissions, questions);
    if let Some(wait) = wait.filter(|wait| wait.kind == BackgroundWaitKind::Approval) {
        return Some(format!(
            "input closed while background Chat {} was waiting for permission request {}; no pending operation was approved",
            wait.chat_id.public(),
            shown_id(wait.request_id)
        ));
    }
    if questions
        .values()
        .any(|question| Some(question.chat_id) == selected_chat)
    {
        return Some("input closed while waiting for clarification; no answer was assumed".into());
    }
    wait.map(|wait| {
        format!(
            "input closed while background Chat {} was waiting for an answer to question {}; no answer was assumed",
            wait.chat_id.public(),
            shown_id(wait.request_id)
        )
    })
}

#[derive(Debug, Default, Eq, PartialEq)]
struct BackgroundRecovery {
    run_notices: Vec<BackgroundRunNotice>,
    request_notices: Vec<String>,
    failed: bool,
}

/// After the event stream lags, corrects a background Chat's pending runs and requests from a
/// snapshot, and restores lost run-end and request notices.
fn recover_background_chat(
    status: &ChatStatus,
    label: &str,
    pending: &mut HashMap<uuid::Uuid, Receipt>,
    permissions: &mut HashMap<uuid::Uuid, PermissionRequestOwner>,
    questions: &mut HashMap<uuid::Uuid, QuestionRequest>,
) -> BackgroundRecovery {
    use tickloop_harness::protocol::LoopState;
    let runs_before = pending
        .values()
        .filter(|receipt| receipt.chat_id == status.id)
        .cloned()
        .collect::<Vec<_>>();
    let known_permissions = permissions.keys().copied().collect::<HashSet<_>>();
    let known_questions = questions.keys().copied().collect::<HashSet<_>>();
    sync_status(status, pending, permissions, questions);
    let mut recovery = BackgroundRecovery::default();
    let run_ended = runs_before
        .iter()
        .any(|receipt| !pending.contains_key(&receipt.run_id.uuid()));
    let outcome = match status.chat_loop.state {
        LoopState::Failed => Some("failed"),
        LoopState::Interrupted => Some("interrupted"),
        LoopState::Running
        | LoopState::Queued
        | LoopState::WaitingPermission
        | LoopState::WaitingQuestion => None,
        LoopState::Idle | LoopState::WaitingPlan => Some(REPLY_COMPLETED_NOTICE),
    };
    if run_ended && let Some(outcome) = outcome {
        recovery.failed = outcome == "failed";
        recovery.run_notices.push(BackgroundRunNotice {
            message: outcome.into(),
            compact_message: None,
            necessary: outcome == "failed",
        });
    }
    for request in &status.permissions {
        if !known_permissions.contains(&request.id) {
            recovery.request_notices.push(format!(
                "{label} · permission request {}",
                shown_id(request.id)
            ));
        }
    }
    for request in &status.questions {
        if !known_questions.contains(&request.id) {
            recovery
                .request_notices
                .push(format!("{label} · question {}", shown_id(request.id)));
        }
    }
    recovery
}

fn initial_context_usage(model: &ModelConfig) -> Option<ContextGauge> {
    model
        .maximum_context_window_tokens()
        .filter(|limit| *limit > 0)
        .map(|limit| (0, limit).into())
}

fn context_usage(status: &ChatStatus) -> Option<ContextGauge> {
    let limit = status
        .model
        .maximum_context_window_tokens()
        .or(status.context_window_tokens)
        .filter(|limit| *limit > 0)?;
    Some(ContextGauge {
        used_tokens: status.context_tokens?,
        window_tokens: limit,
        is_nearly_full: status.is_context_nearly_full(),
    })
}

/// The Chats this View has warned are nearly out of context. A Chat is warned once as it crosses
/// into the warning buffer, and again only after its context falls back below it, as compaction
/// makes it.
#[derive(Default)]
struct ContextWarnings(HashSet<uuid::Uuid>);

impl ContextWarnings {
    /// The notice to show when `status` has just crossed into the warning buffer.
    fn crossing_notice(&mut self, status: &ChatStatus) -> Option<String> {
        let chat = status.id.uuid();
        if !status.is_context_nearly_full() {
            self.0.remove(&chat);
            return None;
        }
        let line = status.auto_compact_tokens?;
        self.0.insert(chat).then(|| {
            format!(
                "Context nearly full · auto-compact at {} tokens · /compact to compact now",
                presentation::compact_count(line)
            )
        })
    }
}

/// A model retry as Claude Code reports it: what failed, then the retry and its attempt, which
/// the conversation shows muted after the failure in the error colour.
fn model_retry_parts(
    kind: ModelRequestKind,
    error: &str,
    attempt: usize,
    max_retries: usize,
    delay_ms: u64,
) -> (String, String) {
    let request = match kind {
        ModelRequestKind::PermissionCheck => "Auto review",
        ModelRequestKind::WebFetch => "WebFetch",
        ModelRequestKind::Title => "Chat title",
        ModelRequestKind::ShellDescription => "Shell description",
        _ => "Model request",
    };
    (
        format!("{request}: {}", safe_text(error).replace('\n', " ")),
        format!(
            " · Retrying in {:.1}s… (attempt {}/{})",
            delay_ms as f64 / 1000.0,
            attempt + 1,
            max_retries + 1,
        ),
    )
}

/// Edits standalone terminal input using the Console's private temporary file and editor rules.
/// The caller suspends raw input while the editor owns the terminal.
pub fn edit_external_text(
    draft: &str,
    directory: &std::path::Path,
    cancellation: &tokio_util::sync::CancellationToken,
) -> Result<Option<String>, String> {
    use external_editor::ExternalEditorOutcome;
    match external_editor::edit_prompt_with_cancellation(draft, directory, Some(cancellation)) {
        ExternalEditorOutcome::Updated { content } => Ok(Some(content)),
        ExternalEditorOutcome::Unchanged | ExternalEditorOutcome::Cancelled { .. } => Ok(None),
        ExternalEditorOutcome::Unavailable => {
            Err("No external editor found; set VISUAL or EDITOR".into())
        }
        ExternalEditorOutcome::Failed(error) => Err(error.to_string()),
    }
}

/// Claude Code's output count, `· ↓ 14.5k tokens`; input tokens are not part of the summary.
fn usage_text(usage: &Usage) -> String {
    usage.output_tokens.map_or_else(String::new, |tokens| {
        format!(" · ↓ {} tokens", presentation::compact_count(tokens))
    })
}

pub fn safe_text(text: &str) -> String {
    text.chars()
        .filter(|character| !character.is_control() || *character == '\n' || *character == '\t')
        .collect()
}

pub fn safe_line(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            character if character.is_control() => {
                escaped.push_str(&format!("\\u{{{:x}}}", character as u32));
            }
            character => escaped.push(character),
        }
    }
    escaped
}

/// An id of the Console's own (a command, an input, a rule, a question) as a person reads and
/// types it back.
fn shown_id(id: impl Into<uuid::Uuid>) -> String {
    tickloop_ids::base62::encode(id.into())
}

fn creation_notice(projection: &crate::agent_console::Projection) -> Option<String> {
    projection.unconfirmed_dispatch.map(|command_id| {
        let command_id = shown_id(command_id);
        format!("Creation {command_id} unknown; inspect Agents, then /acknowledge-create {command_id} to allow another submission without replay")
    })
}

/// Piped input that hit a failure exits non-zero, also when it ends with /exit; an interactive
/// View has already shown every failure.
fn view_exit(interactive: bool, failed: bool) -> Result<(), String> {
    if failed && !interactive {
        Err("one or more tasks failed; review the errors above".into())
    } else {
        Ok(())
    }
}

/// Piped input has no editor or status line to hold a refused input, so the reason goes to
/// stderr and the process ends as failed.
fn report_refused_input(console: &Console, reason: &str, failed: &mut bool) {
    if !console.is_interactive() {
        eprintln!("Failed: {}", safe_line(reason));
        *failed = true;
    }
}

fn creation_attach_unavailable(error: &AccessError) -> String {
    format!(
        "New Agent created; local connection unavailable · {}",
        safe_line(&error.to_string())
    )
}

fn show_creation_feedback(
    console: &Console,
    projection: &crate::agent_console::Projection,
) -> Result<(), String> {
    console.set_creation_notice(creation_notice(projection))?;
    console.set_global_overview(Some(agent_overview(projection)))
}

fn family_input_targets(
    rows: &[AgentSummary],
    viewed: ExecutionRef,
) -> Vec<file_mentions::FamilyMentionTarget> {
    let mut targets = rows
        .iter()
        .map(|row| file_mentions::FamilyMentionTarget {
            execution: row.execution,
            handle: row
                .subagent
                .as_ref()
                .map_or("main", |registration| registration.handle.as_str())
                .into(),
            description: row
                .subagent
                .as_ref()
                .map(|registration| registration.description.clone())
                .unwrap_or_else(|| row.title.clone()),
            available: row.execution != viewed && row.availability == ExecutionAvailability::Live,
        })
        .collect::<Vec<_>>();
    targets.sort_by_key(|target| {
        (
            target.handle != "main",
            rows.iter()
                .find(|row| row.execution == target.execution)
                .and_then(|row| {
                    row.subagent
                        .as_ref()
                        .map(|registration| registration.created_at)
                }),
            target.handle.clone(),
        )
    });
    targets
}

fn agent_overview(projection: &crate::agent_console::Projection) -> console::AgentOverview {
    let tree = agent_rows(&projection.rows, true);
    let mut visible = projection.visible;
    let mut visited = HashSet::new();
    while let Some(execution) = visible.filter(|execution| visited.insert(*execution)) {
        match projection
            .rows
            .iter()
            .find(|row| row.execution == execution)
            .and_then(|row| row.parent)
        {
            Some(parent) if projection.rows.iter().any(|row| row.execution == parent) => {
                visible = Some(parent)
            }
            _ => break,
        }
    }
    let mut rows = tree
        .iter()
        .filter(|row| {
            row.parent_id.is_none()
                && projection.rows.iter().any(|summary| {
                    summary.execution.agent_id == row.agent_id
                        && summary.subagent.is_none()
                        && crate::agent_console::is_listed(summary)
                })
        })
        .cloned()
        .chain(projection.ended.iter().map(ended_agent_row))
        .collect::<Vec<_>>();
    // Each group lists its newest change first; for a finished row that is when it ended, as
    // Claude Code orders its Completed group.
    rows.sort_by_key(|row| (row.group, std::cmp::Reverse(row.updated_at)));
    console::AgentOverview {
        rows,
        dispatch_directory: projection.dispatch_directory.clone(),
        created: projection.created.map(|execution| execution.agent_id),
        visible: visible.map(|execution| execution.agent_id),
    }
}

/// An ended root Agent's overview row, keyed by its Chat: there is no execution to name. Its
/// status says why it ended, so it reads apart from a run that failed while its Agent still runs.
fn ended_agent_row(agent: &tickloop_code_access::EndedAgent) -> console::AgentRow {
    use crate::agent_console::AgentState;
    use tickloop_code_access::EndReason;
    let agent_state = AgentState::of_ended(agent);
    let reason = match agent.reason {
        EndReason::DaemonStopped => Some("ended when the daemon stopped"),
        EndReason::DaemonOff => Some("ended while the daemon was off"),
        EndReason::WorkerLost => Some("worker lost"),
        EndReason::Finished if agent_state == AgentState::Failed => Some("run failed"),
        EndReason::UserStopped | EndReason::Finished => None,
    };
    let status = [Some(agent_state.label()), reason]
        .into_iter()
        .flatten()
        .chain((!agent.resumable).then_some("history gone"))
        .collect::<Vec<_>>()
        .join(" · ");
    console::AgentRow {
        agent_id: agent.record.chat_id,
        parent_id: None,
        title: safe_line(&agent.title),
        status,
        state: agent.last_state.clone(),
        project: shown_folder(agent.folder.as_deref())
            .to_string_lossy()
            .into_owned(),
        workspace: agent.workspace.clone(),
        approvals: 0,
        questions: 0,
        agent_state,
        group: agent_state.group(),
        glyph: console::status_glyph(agent_state, false),
        preview: agent.preview.as_deref().map(safe_line),
        updated_at: Some(agent.ended_at),
        run_started_at: None,
        subagent: None,
        ended: Some(console::EndedRow {
            chat_id: agent.record.chat_id,
            reason: agent.reason,
            resumable: agent.resumable,
        }),
    }
}

/// Sends a peek panel reply to the Agent of the overview row `agent_id`. Replying to an ended row
/// is the user continuing it: its saved history loads first, and the row it now runs under comes
/// back with the status to show.
async fn send_peek_reply(
    agent: &Agent,
    projection: &crate::agent_console::Projection,
    agent_id: uuid::Uuid,
    text: String,
) -> Result<(String, Option<uuid::Uuid>), String> {
    if let Some(row) = projection
        .rows
        .iter()
        .find(|row| row.execution.agent_id == agent_id)
    {
        agent
            .submit(row.chat_id, text)
            .await
            .map_err(|error| error.to_string())?;
        return Ok((format!("Sent to {}", safe_line(&row.title)), None));
    }
    let ended = projection
        .ended
        .iter()
        .find(|ended| ended.record.chat_id == agent_id)
        .ok_or("this Agent is no longer listed")?;
    let chat = ended.record.chat_id;
    agent
        .resume_chat(chat)
        .await
        .map_err(|error| error.to_string())?;
    agent
        .submit(chat, text)
        .await
        .map_err(|error| error.to_string())?;
    let loaded = agent
        .reference(chat)
        .await
        .ok()
        .map(|execution| execution.agent_id);
    Ok((
        format!("Resumed {} and sent your reply", safe_line(&ended.title)),
        loaded,
    ))
}

/// Answers the question the Agent of the overview row `agent_id` waits on with the option a number
/// in the peek panel picked.
async fn answer_peek(
    agent: &Agent,
    projection: &crate::agent_console::Projection,
    agent_id: uuid::Uuid,
    request_id: uuid::Uuid,
    option: String,
) -> Result<(), AccessError> {
    let chat = projection
        .rows
        .iter()
        .find(|row| row.execution.agent_id == agent_id)
        .map(|row| row.chat_id)
        .ok_or_else(|| AccessError::unavailable("this Agent is no longer running"))?;
    agent
        .answer_question(
            chat,
            request_id,
            vec![tickloop_harness::protocol::QuestionAnswer::option(option)],
        )
        .await
}

/// The request a peeked Agent waits on. Only the options of a single question take a number: a
/// permission request, plan approval or file change shows its context and choices only in the
/// Agent's own view, so the panel names it and points there.
fn peek_request(snapshot: &ChatSnapshot) -> Option<console::PeekRequest> {
    if let Some(request) = snapshot.questions.first() {
        let options = match request.questions.as_slice() {
            [question] if !question.multi_select => question
                .options
                .iter()
                .map(|option| option.label.clone())
                .collect(),
            _ => Vec::new(),
        };
        return Some(console::PeekRequest {
            request_id: request.id,
            prompt: request
                .questions
                .first()
                .map(|question| question.question.clone())
                .unwrap_or_default(),
            options,
        });
    }
    let request = snapshot.permissions.first()?;
    Some(console::PeekRequest {
        request_id: request.id,
        prompt: format!(
            "Permission required · {}",
            presentation::tool_name(&request.name)
        ),
        options: Vec::new(),
    })
}

/// Human status text for an Agent row: its state, and why when that is not plain. The Loop state
/// names are internal.
fn agent_status_label(row: &AgentSummary) -> &'static str {
    use tickloop_code_access::ExecutionAvailability;
    use tickloop_harness::protocol::LoopState;
    if row.availability == ExecutionAvailability::Lost {
        return "Failed · lost, outcome unknown";
    }
    match row.state {
        LoopState::Running => "Running",
        LoopState::Queued => "Queued",
        LoopState::WaitingPermission => "Needs approval",
        LoopState::WaitingQuestion => "Needs answer",
        LoopState::WaitingPlan => "Plan review",
        LoopState::Failed => "Failed · run failed",
        LoopState::Interrupted => "Completed · interrupted",
        LoopState::Idle => crate::agent_console::AgentState::of(row).label(),
    }
}

fn agent_rows(rows: &[AgentSummary], aggregate: bool) -> Vec<console::AgentRow> {
    let mut result = rows
        .iter()
        .map(|row| {
            let agent_state = crate::agent_console::AgentState::of(row);
            let parent_id = row.parent.map(|parent| parent.agent_id);
            console::AgentRow {
                agent_id: row.execution.agent_id,
                parent_id,
                title: safe_line(&row.title),
                status: agent_status_label(row).into(),
                state: row.state.clone(),
                project: shown_folder(row.folder.as_deref())
                    .to_string_lossy()
                    .into_owned(),
                workspace: row.workspace.clone(),
                approvals: row.waiting_permissions,
                questions: row.waiting_questions,
                agent_state,
                group: agent_state.group(),
                glyph: console::status_glyph(
                    agent_state,
                    row.state == tickloop_harness::protocol::LoopState::Interrupted,
                ),
                preview: row.preview.as_deref().map(safe_line),
                updated_at: row.updated_at,
                run_started_at: row.run_started_at,
                subagent: row.subagent.clone(),
                ended: None,
            }
        })
        .collect::<Vec<_>>();
    if aggregate {
        for root in result
            .iter_mut()
            .filter(|row| row.parent_id.is_none() && row.subagent.is_none())
        {
            let family = rows
                .iter()
                .filter(|row| {
                    row.execution.agent_id == root.agent_id
                        || row
                            .parent
                            .is_some_and(|parent| parent.agent_id == root.agent_id)
                })
                .collect::<Vec<_>>();
            root.approvals = family.iter().map(|row| row.waiting_permissions).sum();
            root.questions = family.iter().map(|row| row.waiting_questions).sum();
            root.group = family
                .iter()
                .map(|row| crate::agent_console::AttentionGroup::of(row))
                .min()
                .unwrap_or(root.group);
            root.glyph = match root.group {
                crate::agent_console::AttentionGroup::NeedsInput => Glyph::AgentNeedsInput,
                crate::agent_console::AttentionGroup::Working => Glyph::AgentRunning,
                crate::agent_console::AttentionGroup::Completed => root.glyph,
            };
        }
    } else {
        result.sort_by_key(|row| {
            (
                row.subagent.is_some() || row.parent_id.is_some(),
                row.subagent
                    .as_ref()
                    .map(|registration| registration.created_at),
                row.agent_id,
            )
        });
    }
    result
}

async fn navigate_agent(
    global: &mut crate::agent_console::AgentConsole,
    agent: &Agent,
    console: &Console,
    menus: &mut Menus,
    mut effect: crate::agent_console::NavigationEffect,
    sequences: &HashMap<uuid::Uuid, u64>,
    reply_lines: &mut HashMap<uuid::Uuid, ReplyLines>,
) -> Result<Option<ChatSnapshot>, String> {
    use crate::agent_console::{Destination, Intent};
    use tickhost_connect::herdr::HerdrPort;
    for _ in 0..2 {
        if let Destination::Unavailable(record) = &effect.destination {
            console.set_status(&format!(
                "Execution interrupted; open saved history explicitly{}",
                record
                    .as_ref()
                    .map(|record| format!(" with /resume {}", shown_id(record.chat_id)))
                    .unwrap_or_default()
            ))?;
            return Ok(None);
        }
        if let Destination::Choose(views) = &effect.destination {
            menus.locations(console, effect.generation, views.clone())?;
            return Ok(None);
        }
        if !global
            .revalidate(&effect)
            .await
            .map_err(|error| error.to_string())?
        {
            if !global.is_current(&effect) {
                return Ok(None);
            }
            effect = global
                .interpret(Intent::Open(effect.target))
                .map_err(|error| error.to_string())?
                .ok_or("missing recomputed navigation")?;
            continue;
        }
        match &effect.destination {
            Destination::Present => {
                let chat = agent
                    .chat_for_agent(effect.target.agent_id)
                    .await
                    .map_err(|error| error.to_string())?;
                let mut snapshot = agent
                    .snapshot(chat)
                    .await
                    .map_err(|error| error.to_string())?;
                if !global.is_current(&effect) {
                    return Ok(None);
                }
                let after = sequences.get(&chat).copied().unwrap_or(0);
                let observation = if sequences.contains_key(&chat) && snapshot.last_sequence > after
                {
                    Some(
                        agent
                            .observe(chat, after)
                            .await
                            .map_err(|error| error.to_string())?,
                    )
                } else {
                    None
                };
                agent
                    .display_chat(Some(chat))
                    .await
                    .map_err(|error| error.to_string())?;
                menus.dismiss(console)?;
                console.set_global_overview(None)?;
                let restored = console.activate_agent(chat)?;
                if !restored {
                    ensure_snapshot_banner(console, &snapshot)?;
                    console.reset_chat(chat)?;
                    learn_agent_colors(console, agent, &snapshot).await?;
                    workflows::write_public_history(console, &snapshot)?;
                } else if let Some(observation) = observation {
                    let mut presentation = ToolPresentation::new(!console.is_interactive());
                    let observation = render_restored_observation(
                        console,
                        &mut presentation,
                        chat.into(),
                        after,
                        reply_lines,
                        &mut MessageGroups::default(),
                        observation,
                    )?;
                    #[cfg(test)]
                    agent.record_recovery_fact(recovery_tests::RecoveryFact::Restored {
                        chat,
                        after,
                        last_sequence: observation.snapshot.last_sequence,
                        reset_required: observation.reset_required,
                    });
                    snapshot = observation.snapshot;
                }
                console.set_input_context(
                    Some(chat),
                    shown_folder(snapshot.folder.as_deref()),
                    &snapshot.additional_directories,
                )?;
                console.set_visible_delegation(snapshot.delegation.is_some())?;
                console.set_family_input_targets(family_input_targets(
                    &global.family_members(effect.target),
                    effect.target,
                ))?;
                let family = global.family(effect.target);
                console.set_agent_family(
                    if family.len() > 1 {
                        agent_rows(&family, false)
                    } else {
                        Vec::new()
                    },
                    Some(effect.target.agent_id),
                )?;
                if !global.complete(&effect) {
                    return Ok(None);
                }
                return Ok(Some(snapshot));
            }
            Destination::Focus(destination) => {
                let host = destination
                    .host
                    .as_ref()
                    .ok_or("navigation location has no host")?;
                let metadata = match std::fs::metadata(&host.socket) {
                    Ok(metadata) => metadata,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        global.location_unavailable(&effect);
                        effect = global
                            .interpret(Intent::Open(effect.target))
                            .map_err(|error| error.to_string())?
                            .ok_or("missing recomputed navigation")?;
                        continue;
                    }
                    Err(error) => {
                        console.set_status(&safe_line(&format!(
                            "Herdr location cannot be verified: {error}"
                        )))?;
                        return Ok(None);
                    }
                };
                if host.host_instance != endpoint_instance(&metadata) {
                    global.location_unavailable(&effect);
                    effect = global
                        .interpret(Intent::Open(effect.target))
                        .map_err(|error| error.to_string())?
                        .ok_or("missing recomputed navigation")?;
                    continue;
                }
                let pane_id = host
                    .pane
                    .as_deref()
                    .ok_or("navigation location has no Pane")?;
                let port = SocketHerdr::new(HerdrContext::without_view(host.socket.clone()));
                let pane = match port
                    .request(
                        "pane.get",
                        serde_json::json!({"pane_id":pane_id}),
                        tokio_util::sync::CancellationToken::new(),
                    )
                    .await
                {
                    Ok(pane) => pane,
                    Err(error)
                        if matches!(
                            error.code.as_str(),
                            "TargetGone" | "PaneNotFound" | "NotFound" | "pane_not_found"
                        ) =>
                    {
                        global.location_unavailable(&effect);
                        effect = global
                            .interpret(Intent::Open(effect.target))
                            .map_err(|error| error.to_string())?
                            .ok_or("missing recomputed navigation")?;
                        continue;
                    }
                    Err(error) => {
                        console.set_status(&safe_line(&format!(
                            "Cannot verify Pane before focus: {error}"
                        )))?;
                        return Ok(None);
                    }
                };
                let current: tickhost_connect::herdr::Pane =
                    match serde_json::from_value(pane["pane"].clone()) {
                        Ok(pane) => pane,
                        Err(error) => {
                            console.set_status(&safe_line(&format!(
                                "Herdr Pane response could not be verified: {error}"
                            )))?;
                            return Ok(None);
                        }
                    };
                if Some(&current.workspace_id) != host.workspace.as_ref()
                    || Some(&current.tab_id) != host.tab.as_ref()
                    || Some(&current.terminal_id) != host.terminal.as_ref()
                {
                    global.location_unavailable(&effect);
                    effect = global
                        .interpret(Intent::Open(effect.target))
                        .map_err(|error| error.to_string())?
                        .ok_or("missing recomputed navigation")?;
                    continue;
                }
                if !global
                    .revalidate(&effect)
                    .await
                    .map_err(|error| error.to_string())?
                {
                    continue;
                }
                match port
                    .request(
                        "pane.focus",
                        serde_json::json!({"pane_id":pane_id}),
                        tokio_util::sync::CancellationToken::new(),
                    )
                    .await
                {
                    Ok(response) => {
                        let focused = response["pane"]["focused"].as_bool() == Some(true);
                        let still_displayed = global
                            .revalidate(&effect)
                            .await
                            .map_err(|error| error.to_string())?;
                        if focused && still_displayed {
                            global.complete(&effect);
                            console.set_status(
                                "Opened the verified Agent Pane · focus scope unknown",
                            )?;
                        } else {
                            console.set_status("Pane focus result is unknown; refresh locations before another attempt")?;
                        }
                    }
                    Err(error) => console.set_status(&safe_line(&format!(
                        "{} · {}; refresh locations before another attempt",
                        if error.dispatched {
                            "Pane focus result unknown"
                        } else {
                            "Pane focus was not dispatched"
                        },
                        error
                    )))?,
                }
                return Ok(None);
            }
            Destination::Choose(_) | Destination::Unavailable(_) => unreachable!(),
        }
    }
    console.set_status("Agent locations changed during navigation; refresh the overview")?;
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::InputEvent;
    use std::sync::Arc;

    /// The finished line of a turn as `write_completion_notice` writes it, with the role of each
    /// non-blank span.
    fn finished_line(
        mouse: bool,
        shells: usize,
        debug: Option<super::CompletionDebug>,
    ) -> (String, Vec<(String, Option<super::Role>)>) {
        let console = super::Console::test_interactive_for_chat(uuid::Uuid::from_u128(1));
        if mouse {
            console.test_capture_mouse();
        }
        let run = uuid::Uuid::from_u128(5).into();
        let details =
            super::completion_details(run, Some(std::time::Duration::from_secs(8)), shells);
        super::write_completion_notice(&console, Some(&details), debug.as_ref()).unwrap();
        let rows = console.test_conversation_rows();
        let row = rows
            .iter()
            .find(|row| row.spans.iter().any(|span| span.text.contains(" for 8s")))
            .unwrap();
        (
            row.spans.iter().map(|span| span.text.as_str()).collect(),
            row.spans
                .iter()
                .filter(|span| !span.text.trim().is_empty())
                .map(|span| (span.text.clone(), span.role))
                .collect(),
        )
    }

    fn debug(cache: &str, is_warning: bool, average: Option<&str>) -> super::CompletionDebug {
        super::CompletionDebug {
            summary: super::debug_line::CacheSummary {
                cache: cache.into(),
                is_warning,
                average: average.map(Into::into),
            },
            details: "chat: fixture".into(),
        }
    }

    #[test]
    fn the_cache_summary_and_button_end_the_finished_line_after_what_it_already_says() {
        let (plain, _) = finished_line(true, 1, None);
        let verb = plain.split(" for 8s").next().unwrap().to_owned();
        assert_eq!(plain, format!("{verb} for 8s · 1 shell still running"));
        for (cache, warning, average, mouse, expected) in [
            (
                "cache 96%",
                false,
                Some("avg 91%"),
                true,
                " · cache 96% · avg 91%  [debug info]",
            ),
            (
                "cache cold",
                false,
                None,
                true,
                " · cache cold  [debug info]",
            ),
            ("cache n/a", false, None, true, " · cache n/a  [debug info]"),
            (
                "cache 4% (break: tools)",
                true,
                Some("avg 88%"),
                true,
                " · cache 4% (break: tools) · avg 88%  [debug info]",
            ),
            // Without mouse capture a button could not be clicked, so /debug stands alone.
            (
                "cache 96%",
                false,
                Some("avg 91%"),
                false,
                " · cache 96% · avg 91%",
            ),
        ] {
            let (line, roles) = finished_line(mouse, 1, Some(debug(cache, warning, average)));
            assert_eq!(line, format!("{plain}{expected}"));
            for (text, role) in roles {
                let expected_role = if text == cache && warning {
                    super::Role::Warning
                } else if text == super::debug_line::DEBUG_BUTTON {
                    super::Role::Link
                } else {
                    super::Role::MutedText
                };
                assert_eq!(role, Some(expected_role), "{text}");
            }
        }
    }

    #[test]
    fn model_setup_blocks_messages_and_skills_but_keeps_local_commands() {
        for input in [
            "hello",
            "  请帮我写代码  ",
            "/tickloop",
            "/slides draft",
            "/my-skill args",
            "/goal finish the task",
            "/compact",
            "/compact summarize this",
            "/plan add a cache",
            "/plan open a pull request",
        ] {
            assert!(super::model_setup_blocks_input(input), "{input}");
        }
        for input in [
            "",
            "  ",
            "/",
            "/provider",
            "/model",
            "/settings",
            "/exit",
            "/help advanced",
            "/resume saved-chat",
            "/clear",
            "/plan",
            "/plan  open ",
        ] {
            assert!(!super::model_setup_blocks_input(input), "{input}");
        }
    }

    #[test]
    fn model_setup_banner_does_not_present_the_preview_model_as_selected() {
        let spans = super::startup_banner_with_model(
            super::MODEL_SETUP_PROMPT.into(),
            std::path::Path::new("/project"),
        );
        let text: String = spans.into_iter().map(|(text, _)| text).collect();
        assert!(text.contains("Choose a model"));
        assert!(!text.contains("deepseek"));
    }

    #[test]
    fn global_overview_keeps_the_root_title_and_aggregates_requests_without_child_rows() {
        let epoch = uuid::Uuid::new_v4();
        let root = tickloop_code_access::ExecutionRef {
            daemon_epoch: epoch,
            agent_id: uuid::Uuid::from_u128(1),
            generation: 1,
        };
        let child = tickloop_code_access::ExecutionRef {
            agent_id: uuid::Uuid::from_u128(2),
            ..root
        };
        let row = |execution: tickloop_code_access::ExecutionRef,
                   parent: Option<tickloop_code_access::ExecutionRef>,
                   title: &str,
                   questions: usize| {
            serde_json::from_value::<tickloop_code_access::AgentSummary>(serde_json::json!({"execution":execution,"chat_id":execution.agent_id,"folder":"/test","state":"Idle","title":title,"has_work":true,"waiting_permissions":0,"waiting_questions":questions,"parent":parent,"record":null,"workspace":null,"capabilities":[],"views":[],"subagent":parent.map(|_|serde_json::json!({"child_chat_id":child.agent_id,"handle":"audit","description":"Audit source","created_at":100000}))})).unwrap()
        };
        let projection = crate::agent_console::Projection {
            overview: true,
            visible: Some(child),
            dispatch_directory: "/test".into(),
            rows: vec![
                row(root, None, "Generated root title", 0),
                row(child, Some(root), "Child task", 1),
            ],
            waiting_permissions: 0,
            waiting_questions: 1,
            generation: 0,
            closed: false,
            unconfirmed_dispatch: None,
            created: None,
            ended: Vec::new(),
        };
        let overview = super::agent_overview(&projection);
        assert_eq!(overview.rows.len(), 1);
        assert_eq!(overview.rows[0].title, "Generated root title");
        assert_eq!(overview.visible, Some(root.agent_id));
        assert_eq!(overview.rows[0].questions, 1);
        assert_eq!(overview.rows[0].status_role(), super::style::Role::Warning);
    }

    /// Refreshes `global` until the row of `chat` satisfies `ready`.
    async fn wait_for_row(
        global: &mut crate::agent_console::AgentConsole,
        chat: uuid::Uuid,
        ready: impl Fn(&tickloop_code_access::AgentSummary) -> bool,
    ) -> tickloop_code_access::AgentSummary {
        for _ in 0..500 {
            let projection = global.refresh().await.unwrap();
            if let Some(row) = projection
                .rows
                .iter()
                .find(|row| row.chat_id == chat && ready(row))
            {
                return row.clone();
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!(
            "the row of {chat} never became ready: {:?}",
            global.projection().rows
        );
    }

    fn last_request_text(model: &tickloop_harness::test_support::ScriptedModel) -> String {
        serde_json::to_string(&model.requests.lock().unwrap().last().unwrap().messages).unwrap()
    }

    #[tokio::test]
    async fn a_peek_reply_to_an_idle_agent_starts_its_next_turn_in_the_overview() {
        use tickloop_harness::protocol::LoopState;
        use tickloop_harness::test_support::{ScriptedModel, config, text};
        let directory = tempfile::tempdir().unwrap();
        let model = std::sync::Arc::new(ScriptedModel::new(vec![
            text("First pass done"),
            text("Second pass done"),
        ]));
        let agent = super::Agent::new_embedded(config(), model.clone());
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        agent.submit(chat, "First pass".into()).await.unwrap();
        let mut global = crate::agent_console::AgentConsole::new(
            agent.access(),
            agent.daemon_epoch(),
            directory.path().into(),
            None,
        );
        let finished = wait_for_row(&mut global, chat, |row| {
            row.state == LoopState::Idle && row.run_id.is_some()
        })
        .await;
        let asked = model.requests.lock().unwrap().len();

        let (status, loaded) = super::send_peek_reply(
            &agent,
            &global.projection(),
            finished.execution.agent_id,
            "Run the second pass".into(),
        )
        .await
        .unwrap();

        assert_eq!(status, format!("Sent to {}", finished.title));
        assert_eq!(loaded, None);
        wait_for_row(&mut global, chat, |row| {
            row.state == LoopState::Idle && row.run_id.is_some() && row.run_id != finished.run_id
        })
        .await;
        assert!(model.requests.lock().unwrap().len() > asked);
        assert!(last_request_text(&model).contains("Run the second pass"));
        agent.shutdown().await;
    }

    #[tokio::test]
    async fn a_peek_reply_to_an_ended_agent_loads_its_history_and_starts_a_turn() {
        use tickloop_code_settings::local_agent::LocalAgent;
        use tickloop_harness::protocol::{EventKind, LoopState};
        use tickloop_harness::test_support::{ScriptedModel, config, next_event, text};
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("root");
        let folder = directory.path().join("work");
        std::fs::create_dir_all(&folder).unwrap();
        let saved = tickloop_harness::runtime::Agent::open_embedded(
            root.clone(),
            config(),
            std::sync::Arc::new(ScriptedModel::new(vec![text("Reviewed half")])),
        )
        .unwrap();
        let chat = saved.create_chat(folder.clone()).await.unwrap();
        let mut events = saved.subscribe();
        let receipt = saved
            .submit(chat, "Review the module".into())
            .await
            .unwrap();
        loop {
            let event = next_event(&mut events).await;
            if event.run_id == receipt.run_id && matches!(event.kind, EventKind::Completed { .. }) {
                break;
            }
        }
        saved.shutdown().await;
        let model = std::sync::Arc::new(ScriptedModel::new(vec![text("Reviewed the rest")]));
        let agent = super::Agent::open_embedded(root.clone(), config(), model.clone()).unwrap();
        let mut global = crate::agent_console::AgentConsole::new(
            agent.access(),
            agent.daemon_epoch(),
            folder.clone(),
            None,
        );
        let mut projection = global.refresh().await.unwrap();
        assert!(projection.rows.is_empty(), "nothing runs before the reply");
        projection.ended = vec![tickloop_code_access::EndedAgent {
            record: tickloop_code_access::TranscriptRef {
                store_root: root.clone(),
                chat_id: chat.uuid(),
            },
            agent_id: uuid::Uuid::from_u128(1),
            folder: Some(folder.clone()),
            workspace: None,
            title: "Review the module".into(),
            preview: None,
            created_at: 1,
            updated_at: 2,
            ended_at: 3,
            last_state: LoopState::Idle,
            outcome: tickloop_code_access::EndedOutcome::Failed,
            reason: tickloop_code_access::EndReason::DaemonStopped,
            resumable: true,
        }];

        let (status, loaded) =
            super::send_peek_reply(&agent, &projection, chat.uuid(), "Finish the review".into())
                .await
                .unwrap();

        assert_eq!(status, "Resumed Review the module and sent your reply");
        let row = wait_for_row(&mut global, chat.uuid(), |row| {
            row.state == LoopState::Idle && row.run_id.is_some()
        })
        .await;
        assert_eq!(loaded, Some(row.execution.agent_id));
        let request = last_request_text(&model);
        assert!(request.contains("Review the module"), "{request}");
        assert!(request.contains("Finish the review"), "{request}");
        agent.shutdown().await;
    }

    #[tokio::test]
    async fn a_number_in_the_peek_panel_answers_the_agents_question() {
        use tickloop_harness::protocol::LoopState;
        use tickloop_harness::test_support::{ScriptedModel, call, config, single_question, text};
        let directory = tempfile::tempdir().unwrap();
        let model = std::sync::Arc::new(ScriptedModel::new(vec![
            call("ask", "AskUserQuestion", single_question("Which report?")),
            text("Writing the monthly report"),
        ]));
        let agent = super::Agent::new_embedded(config(), model.clone());
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        agent.submit(chat, "Write the report".into()).await.unwrap();
        let mut global = crate::agent_console::AgentConsole::new(
            agent.access(),
            agent.daemon_epoch(),
            directory.path().into(),
            None,
        );
        let waiting = wait_for_row(&mut global, chat, |row| {
            row.state == LoopState::WaitingQuestion
        })
        .await;
        let request = super::peek_request(&agent.snapshot(chat).await.unwrap()).unwrap();
        assert_eq!(request.prompt, "Which report?");
        assert_eq!(request.options, ["Weekly", "Monthly"]);

        super::answer_peek(
            &agent,
            &global.projection(),
            waiting.execution.agent_id,
            request.request_id,
            request.options[1].clone(),
        )
        .await
        .unwrap();

        wait_for_row(&mut global, chat, |row| row.state == LoopState::Idle).await;
        assert!(last_request_text(&model).contains("Monthly"));
        agent.shutdown().await;
    }

    /// A permission request, a plan approval or a file change is answered only in the Agent's own
    /// view, where its context and every choice show: the peek panel gets no option for it, so no
    /// number approves it and no session scope is reachable from there.
    #[tokio::test]
    async fn peek_requests_for_permissions_plans_and_file_changes_offer_no_option() {
        use tickloop_harness::protocol::{
            FileChangeAction, FileChangeArtifactRef, FileChangeReviewSummary, FileVersion,
            PermissionApprovalSource, PermissionExecution, PermissionRequest, PreparedFileChange,
        };
        use tickloop_harness::test_support::{ScriptedModel, config};
        let directory = tempfile::tempdir().unwrap();
        let agent = super::Agent::new_embedded(
            config(),
            std::sync::Arc::new(ScriptedModel::new(Vec::new())),
        );
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        let base = agent.snapshot(chat).await.unwrap();
        let version = |sha256: &str| FileVersion {
            exists: true,
            identity: None,
            byte_len: 3,
            line_count: 1,
            ends_with_newline: false,
            sha256: sha256.into(),
        };
        let file_change = PreparedFileChange {
            summary: FileChangeReviewSummary {
                action: FileChangeAction::Overwrite,
                path: directory.path().join("notes.txt"),
                before: version("before"),
                after: version("after"),
                added_lines: 1,
                removed_lines: 1,
                hunk_count: 1,
            },
            artifact: FileChangeArtifactRef {
                path: directory.path().join("review.json"),
                byte_len: 42,
                sha256: "artifact".into(),
            },
        };
        let requests = [
            ("Bash", PermissionApprovalSource::PermissionPolicy, None),
            ("Bash", PermissionApprovalSource::ExplicitAsk, None),
            ("ExitPlanMode", PermissionApprovalSource::default(), None),
            (
                "Write",
                PermissionApprovalSource::default(),
                Some(Box::new(file_change)),
            ),
        ];
        for (name, approval_source, file_change) in requests {
            let mut snapshot = base.clone();
            snapshot.permissions = vec![PermissionRequest {
                parent_call: None,
                mcp_server: None,
                id: uuid::Uuid::new_v4(),
                loop_id: uuid::Uuid::new_v4().into(),
                execution: PermissionExecution::ToolCall {
                    run_id: uuid::Uuid::new_v4().into(),
                    call_id: "call".into(),
                },
                name: name.into(),
                input: serde_json::json!({}),
                file_change,
                approval_reason: None,
                approval_source,
            }];

            let request = super::peek_request(&snapshot).unwrap();

            assert!(request.options.is_empty(), "{name}: {request:?}");
            assert!(
                request.prompt.starts_with("Permission required"),
                "{name}: {request:?}"
            );
        }
        agent.shutdown().await;
    }

    /// Claude Code's agent view states and groups, one mapping row each: what the daemon reports,
    /// the state the row shows, its status text and its group.
    #[test]
    fn overview_rows_follow_the_agent_view_state_and_group_table() {
        use crate::agent_console::{AgentState, AttentionGroup};
        use tickloop_code_access::{EndReason, EndedAgent, EndedOutcome, TranscriptRef};
        use tickloop_harness::protocol::LoopState;
        let epoch = uuid::Uuid::new_v4();
        let live = |id: u128,
                    state: &str,
                    run: bool,
                    has_work: Option<bool>,
                    lost: bool,
                    updated_at: u64| {
            let mut summary = serde_json::from_value::<tickloop_code_access::AgentSummary>(serde_json::json!({"execution":{"daemon_epoch":epoch,"agent_id":uuid::Uuid::from_u128(id),"generation":1},"chat_id":uuid::Uuid::from_u128(id),"folder":"/test","state":state,"title":format!("live {id}"),"waiting_permissions":0,"waiting_questions":0,"parent":null,"record":null,"workspace":null,"capabilities":[],"views":[],"updated_at":updated_at})).unwrap();
            summary.has_work = has_work;
            summary.run_id = run.then(|| uuid::Uuid::from_u128(id + 1000).into());
            if lost {
                summary.availability = tickloop_code_access::ExecutionAvailability::Lost;
            }
            summary
        };
        let ended =
            |id: u128, outcome: EndedOutcome, reason: EndReason, last_state, ended_at: u64| {
                EndedAgent {
                    record: TranscriptRef {
                        store_root: "/config".into(),
                        chat_id: uuid::Uuid::from_u128(id),
                    },
                    agent_id: uuid::Uuid::from_u128(id + 1000),
                    folder: Some("/test".into()),
                    workspace: None,
                    title: format!("ended {id}"),
                    preview: None,
                    created_at: 1,
                    updated_at: ended_at,
                    ended_at,
                    last_state,
                    outcome,
                    reason,
                    resumable: true,
                }
            };
        let projection = crate::agent_console::Projection {
            overview: true,
            visible: None,
            dispatch_directory: "/test".into(),
            rows: vec![
                live(1, "WaitingQuestion", true, Some(true), false, 10),
                live(2, "Running", true, Some(true), false, 20),
                live(3, "Idle", true, Some(true), false, 30),
                live(4, "Interrupted", true, Some(true), false, 40),
                live(5, "Failed", true, Some(true), false, 50),
                live(6, "Idle", true, Some(true), true, 60),
                live(7, "Idle", false, Some(true), false, 70),
                live(8, "Idle", true, None, false, 80),
                live(9, "Idle", false, Some(false), false, 90),
            ],
            ended: vec![
                ended(
                    11,
                    EndedOutcome::Failed,
                    EndReason::DaemonStopped,
                    LoopState::Running,
                    100,
                ),
                ended(
                    12,
                    EndedOutcome::Stopped,
                    EndReason::DaemonOff,
                    LoopState::Idle,
                    5,
                ),
                ended(
                    13,
                    EndedOutcome::Stopped,
                    EndReason::UserStopped,
                    LoopState::Running,
                    110,
                ),
                ended(
                    14,
                    EndedOutcome::Failed,
                    EndReason::DaemonStopped,
                    LoopState::WaitingQuestion,
                    120,
                ),
                // A permission prompt is work: it ended with the daemon.
                ended(
                    15,
                    EndedOutcome::Failed,
                    EndReason::DaemonStopped,
                    LoopState::WaitingPermission,
                    115,
                ),
                // A finished turn stays finished, a failed one stays failed.
                ended(
                    16,
                    EndedOutcome::Completed,
                    EndReason::Finished,
                    LoopState::Idle,
                    95,
                ),
                ended(
                    17,
                    EndedOutcome::Failed,
                    EndReason::Finished,
                    LoopState::Failed,
                    85,
                ),
            ],
            waiting_permissions: 0,
            waiting_questions: 0,
            generation: 0,
            closed: false,
            unconfirmed_dispatch: None,
            created: None,
        };
        let overview = super::agent_overview(&projection);
        let rows = overview
            .rows
            .iter()
            .map(|row| {
                (
                    row.title.as_str(),
                    row.agent_state,
                    row.status.as_str(),
                    row.group,
                )
            })
            .collect::<Vec<_>>();
        use AgentState::*;
        use AttentionGroup as G;
        assert_eq!(
            rows,
            [
                (
                    "ended 14",
                    NeedsInput,
                    "Needs input · ended when the daemon stopped",
                    G::NeedsInput
                ),
                ("live 1", NeedsInput, "Needs answer", G::NeedsInput),
                ("live 2", Working, "Running", G::Working),
                // Completed lists the newest end first, ended and running rows alike.
                (
                    "ended 15",
                    Failed,
                    "Failed · ended when the daemon stopped",
                    G::Completed
                ),
                ("ended 13", Stopped, "Stopped", G::Completed),
                (
                    "ended 11",
                    Failed,
                    "Failed · ended when the daemon stopped",
                    G::Completed
                ),
                ("ended 16", Completed, "Completed", G::Completed),
                ("ended 17", Failed, "Failed · run failed", G::Completed),
                ("live 8", Idle, "Idle", G::Completed),
                ("live 7", Idle, "Idle", G::Completed),
                (
                    "live 6",
                    Failed,
                    "Failed · lost, outcome unknown",
                    G::Completed
                ),
                ("live 5", Failed, "Failed · run failed", G::Completed),
                ("live 4", Completed, "Completed · interrupted", G::Completed),
                ("live 3", Completed, "Completed", G::Completed),
                (
                    "ended 12",
                    Stopped,
                    "Stopped · ended while the daemon was off",
                    G::Completed
                ),
            ],
            "an Agent that holds nothing (live 9) is not listed"
        );
    }

    #[test]
    fn family_root_aggregate_request_glyph_keeps_warning_role() {
        let root = tickloop_code_access::ExecutionRef {
            daemon_epoch: uuid::Uuid::new_v4(),
            agent_id: uuid::Uuid::from_u128(1),
            generation: 1,
        };
        let child = tickloop_code_access::ExecutionRef {
            agent_id: uuid::Uuid::from_u128(2),
            ..root
        };
        let make_row = |execution, parent, state, questions| {
            serde_json::from_value::<tickloop_code_access::AgentSummary>(serde_json::json!({"execution":execution,"chat_id":uuid::Uuid::new_v4(),"folder":"/test","state":state,"title":"Title","has_work":true,"waiting_permissions":0,"waiting_questions":questions,"parent":parent,"record":null,"workspace":null,"capabilities":[],"views":[]})).unwrap()
        };
        let rows = super::agent_rows(
            &[
                make_row(
                    root,
                    None::<tickloop_code_access::ExecutionRef>,
                    "Interrupted",
                    0,
                ),
                make_row(child, Some(root), "WaitingQuestion", 1),
            ],
            true,
        );
        assert_eq!(rows[0].glyph_role(), super::style::Role::Warning);
        assert_eq!(rows[0].status_role(), super::style::Role::Warning);
    }

    #[test]
    fn family_child_description_never_emits_terminal_controls() {
        let child = tickloop_code_access::ExecutionRef {
            daemon_epoch: uuid::Uuid::new_v4(),
            agent_id: uuid::Uuid::new_v4(),
            generation: 1,
        };
        let parent = tickloop_code_access::ExecutionRef {
            agent_id: uuid::Uuid::new_v4(),
            ..child
        };
        let row: tickloop_code_access::AgentSummary = serde_json::from_value(serde_json::json!({"execution":child,"chat_id":child.agent_id,"folder":"/test","state":"Idle","title":"Title","has_work":true,"waiting_permissions":0,"waiting_questions":0,"parent":parent,"record":null,"workspace":null,"capabilities":[],"views":[],"subagent":{"child_chat_id":child.agent_id,"handle":"audit","description":"Check \u{001b}[2J evidence","created_at":1}})).unwrap();
        let row = super::agent_rows(&[row], false).pop().unwrap();
        let display = super::display::DisplayProfile::default();
        let line = super::subagent_panel::family_line(&row, false, false, (80, 5, display, 1000));
        assert!(!line.text().chars().any(char::is_control));
    }

    #[tokio::test]
    async fn cached_event_recovery_renders_family_receipts_replies_and_completions_once() {
        let chat = uuid::Uuid::new_v4();
        let run = uuid::Uuid::new_v4();
        let loop_id = uuid::Uuid::new_v4();
        let record = tickloop_harness::protocol::FamilyInputRecord {
            message_id: uuid::Uuid::new_v4(),
            target_chat_id: uuid::Uuid::new_v4().into(),
            target_handle: "audit".into(),
            target_run_id: run.into(),
            submitted_at: 1,
            response: Some("Actual reply preview".into()),
            state: Some(tickloop_harness::protocol::LoopState::Idle),
        };
        let completion = tickloop_harness::protocol::FamilyCompletion {
            notification: tickloop_harness::protocol::SubagentNotificationDelivery::Pending,
            usage: Default::default(),
            total_tokens: None,
            child_chat_id: record.target_chat_id,
            child_loop_id: loop_id.into(),
            child_run_id: run.into(),
            handle: "audit".into(),
            state: tickloop_harness::protocol::LoopState::Idle,
            result: "Actual child completion".into(),
            error: None,
            started_at: Some(1),
            finished_at: 2,
            triggering_message_ids: vec![],
            run_status: None,
        };
        let mut snapshot: super::ChatSnapshot = serde_json::from_value(
            serde_json::json!({"id":chat,"folder":"/test","messages":[],"chat_loop":{"id":"00000000-0000-0000-0000-000000000000","state":"Idle"}}),
        )
        .unwrap();
        snapshot.events = vec![
            tickloop_harness::protocol::Event {
                sequence: 1,
                chat_id: chat.into(),
                loop_id: loop_id.into(),
                run_id: run.into(),
                kind: tickloop_harness::protocol::EventKind::FamilyInputUpdated { record },
            },
            tickloop_harness::protocol::Event {
                sequence: 2,
                chat_id: chat.into(),
                loop_id: loop_id.into(),
                run_id: run.into(),
                kind: tickloop_harness::protocol::EventKind::FamilyRunCompleted { completion },
            },
        ];
        snapshot.last_sequence = 2;
        let mut observation = super::ExecutionObservation {
            execution: tickloop_code_access::ExecutionRef {
                daemon_epoch: uuid::Uuid::nil(),
                agent_id: chat,
                generation: 1,
            },
            events: snapshot.events.clone(),
            snapshot,
            reset_required: false,
        };
        let console = super::Console::test_interactive_for_chat(chat);
        let agent = super::Agent::new_embedded(
            tickloop_harness::test_support::config(),
            std::sync::Arc::new(tickloop_harness::test_support::ScriptedModel::new(vec![])),
        );
        let mut tools = super::ToolPresentation::new(true);
        super::render_restored_observation(
            &console,
            &mut tools,
            chat.into(),
            0,
            &mut std::collections::HashMap::new(),
            &mut super::MessageGroups::default(),
            observation.clone(),
        )
        .unwrap();
        let text = console.test_conversation_text();
        assert!(
            text.contains("Reply from @audit")
                && text.contains("Actual reply preview")
                && text.contains("Actual child completion")
        );
        observation.events.retain(|event| event.sequence > 2);
        super::render_restored_observation(
            &console,
            &mut tools,
            chat.into(),
            2,
            &mut std::collections::HashMap::new(),
            &mut super::MessageGroups::default(),
            observation,
        )
        .unwrap();
        assert_eq!(
            console
                .test_conversation_text()
                .matches("Actual reply preview")
                .count(),
            1
        );
        agent.shutdown().await;
    }
    #[tokio::test]
    async fn cached_event_recovery_suppresses_foreground_completion_notifications() {
        let chat = uuid::Uuid::new_v4();
        let run = uuid::Uuid::new_v4();
        let loop_id = uuid::Uuid::new_v4();
        let record = tickloop_harness::protocol::FamilyInputRecord {
            message_id: uuid::Uuid::new_v4(),
            target_chat_id: uuid::Uuid::new_v4().into(),
            target_handle: "audit".into(),
            target_run_id: run.into(),
            submitted_at: 1,
            response: Some("Actual reply preview".into()),
            state: Some(tickloop_harness::protocol::LoopState::Idle),
        };
        let completion = tickloop_harness::protocol::FamilyCompletion {
            notification: tickloop_harness::protocol::SubagentNotificationDelivery::Foreground,
            usage: Default::default(),
            total_tokens: None,
            child_chat_id: record.target_chat_id,
            child_loop_id: loop_id.into(),
            child_run_id: run.into(),
            handle: "audit".into(),
            state: tickloop_harness::protocol::LoopState::Idle,
            result: "Actual child completion".into(),
            error: None,
            started_at: Some(1),
            finished_at: 2,
            triggering_message_ids: vec![],
            run_status: None,
        };
        let mut snapshot: super::ChatSnapshot = serde_json::from_value(
            serde_json::json!({"id":chat,"folder":"/test","messages":[],"chat_loop":{"id":"00000000-0000-0000-0000-000000000000","state":"Idle"}}),
        )
        .unwrap();
        snapshot.events = vec![
            tickloop_harness::protocol::Event {
                sequence: 1,
                chat_id: chat.into(),
                loop_id: loop_id.into(),
                run_id: run.into(),
                kind: tickloop_harness::protocol::EventKind::FamilyInputUpdated { record },
            },
            tickloop_harness::protocol::Event {
                sequence: 2,
                chat_id: chat.into(),
                loop_id: loop_id.into(),
                run_id: run.into(),
                kind: tickloop_harness::protocol::EventKind::FamilyRunCompleted { completion },
            },
        ];
        snapshot.last_sequence = 2;
        let mut observation = super::ExecutionObservation {
            execution: tickloop_code_access::ExecutionRef {
                daemon_epoch: uuid::Uuid::nil(),
                agent_id: chat,
                generation: 1,
            },
            events: snapshot.events.clone(),
            snapshot,
            reset_required: false,
        };
        let console = super::Console::test_interactive_for_chat(chat);
        let agent = super::Agent::new_embedded(
            tickloop_harness::test_support::config(),
            std::sync::Arc::new(tickloop_harness::test_support::ScriptedModel::new(vec![])),
        );
        let mut tools = super::ToolPresentation::new(true);
        super::render_restored_observation(
            &console,
            &mut tools,
            chat.into(),
            0,
            &mut std::collections::HashMap::new(),
            &mut super::MessageGroups::default(),
            observation.clone(),
        )
        .unwrap();
        let text = console.test_conversation_text();
        assert!(
            text.contains("Reply from @audit")
                && text.contains("Actual reply preview")
                && !text.contains("Actual child completion")
        );
        observation.events.retain(|event| event.sequence > 2);
        super::render_restored_observation(
            &console,
            &mut tools,
            chat.into(),
            2,
            &mut std::collections::HashMap::new(),
            &mut super::MessageGroups::default(),
            observation,
        )
        .unwrap();
        assert_eq!(
            console
                .test_conversation_text()
                .matches("Actual reply preview")
                .count(),
            1
        );
        agent.shutdown().await;
    }
    #[tokio::test]
    async fn cached_event_recovery_displays_completion_delivery_refusal() {
        let chat = uuid::Uuid::new_v4();
        let run = uuid::Uuid::new_v4();
        let loop_id = uuid::Uuid::new_v4();
        let record = tickloop_harness::protocol::FamilyInputRecord {
            message_id: uuid::Uuid::new_v4(),
            target_chat_id: uuid::Uuid::new_v4().into(),
            target_handle: "audit".into(),
            target_run_id: run.into(),
            submitted_at: 1,
            response: Some("Actual reply preview".into()),
            state: Some(tickloop_harness::protocol::LoopState::Idle),
        };
        let completion = tickloop_harness::protocol::FamilyCompletion {
            notification: tickloop_harness::protocol::SubagentNotificationDelivery::Rejected(
                tickloop_harness::protocol::FamilyMessageRejection {
                    code: tickloop_harness::protocol::FamilyMessageRejectionCode::StaleTarget,
                    message: "The receiving parent is unavailable".into(),
                },
            ),
            usage: Default::default(),
            total_tokens: None,
            child_chat_id: record.target_chat_id,
            child_loop_id: loop_id.into(),
            child_run_id: run.into(),
            handle: "audit".into(),
            state: tickloop_harness::protocol::LoopState::Idle,
            result: "Actual child completion".into(),
            error: None,
            started_at: Some(1),
            finished_at: 2,
            triggering_message_ids: vec![],
            run_status: None,
        };
        let mut snapshot: super::ChatSnapshot = serde_json::from_value(
            serde_json::json!({"id":chat,"folder":"/test","messages":[],"chat_loop":{"id":"00000000-0000-0000-0000-000000000000","state":"Idle"}}),
        )
        .unwrap();
        snapshot.events = vec![
            tickloop_harness::protocol::Event {
                sequence: 1,
                chat_id: chat.into(),
                loop_id: loop_id.into(),
                run_id: run.into(),
                kind: tickloop_harness::protocol::EventKind::FamilyInputUpdated { record },
            },
            tickloop_harness::protocol::Event {
                sequence: 2,
                chat_id: chat.into(),
                loop_id: loop_id.into(),
                run_id: run.into(),
                kind: tickloop_harness::protocol::EventKind::FamilyRunCompleted { completion },
            },
        ];
        snapshot.last_sequence = 2;
        let mut observation = super::ExecutionObservation {
            execution: tickloop_code_access::ExecutionRef {
                daemon_epoch: uuid::Uuid::nil(),
                agent_id: chat,
                generation: 1,
            },
            events: snapshot.events.clone(),
            snapshot,
            reset_required: false,
        };
        let console = super::Console::test_interactive_for_chat(chat);
        let agent = super::Agent::new_embedded(
            tickloop_harness::test_support::config(),
            std::sync::Arc::new(tickloop_harness::test_support::ScriptedModel::new(vec![])),
        );
        let mut tools = super::ToolPresentation::new(true);
        super::render_restored_observation(
            &console,
            &mut tools,
            chat.into(),
            0,
            &mut std::collections::HashMap::new(),
            &mut super::MessageGroups::default(),
            observation.clone(),
        )
        .unwrap();
        let text = console.test_conversation_text();
        assert!(
            text.contains("Reply from @audit")
                && text.contains("Actual reply preview")
                && text.contains("Actual child completion")
                && text.contains("Notification not delivered: The receiving parent is unavailable")
        );
        observation.events.retain(|event| event.sequence > 2);
        super::render_restored_observation(
            &console,
            &mut tools,
            chat.into(),
            2,
            &mut std::collections::HashMap::new(),
            &mut super::MessageGroups::default(),
            observation,
        )
        .unwrap();
        assert_eq!(
            console
                .test_conversation_text()
                .matches("Actual reply preview")
                .count(),
            1
        );
        agent.shutdown().await;
    }

    #[test]
    fn a_child_report_and_its_completion_draw_one_block_whichever_arrives_first() {
        use super::{ChatSnapshot, Console, show_consumed_family_input, show_family_completion};
        let child_run = uuid::Uuid::new_v4();
        let report = tickloop_harness::protocol::SubagentReportMessage {
            note: None,
            report: "Report line".into(),
        };
        let text = format!(
            "<agent-message>{}</agent-message>",
            serde_json::json!({"source":{"message_id":child_run,"sender_handle":"audit","sender_label":"audit","is_result":true},"message":report.text()})
        );
        let completion = tickloop_harness::protocol::FamilyCompletion {
            child_chat_id: tickloop_harness::protocol::ChatId::new(),
            child_loop_id: uuid::Uuid::new_v4().into(),
            child_run_id: child_run.into(),
            handle: "audit".into(),
            state: tickloop_harness::protocol::LoopState::Idle,
            result: tickloop_harness::protocol::SUBAGENT_REPORT_RECEIPT.into(),
            error: None,
            started_at: Some(1_000),
            finished_at: 3_000,
            triggering_message_ids: Vec::new(),
            notification: tickloop_harness::protocol::SubagentNotificationDelivery::Queued,
            usage: Default::default(),
            total_tokens: None,
            run_status: None,
        };
        for report_first in [true, false] {
            let chat = uuid::Uuid::new_v4();
            let run = uuid::Uuid::new_v4();
            let mut snapshot: ChatSnapshot = serde_json::from_value(serde_json::json!({
                "id":chat,"folder":"/work","chat_loop":{"id":"00000000-0000-0000-0000-000000000000","state":"Idle"},
                "messages":[{"role":"user","content":[{"type":"text","text":text}]}]
            }))
            .unwrap();
            snapshot.message_ids.insert(0, child_run);
            snapshot.message_runs.insert(0, run.into());
            snapshot
                .agent_message_origins
                .insert(child_run, serde_json::json!({"is_result":true}));
            let console = Console::test_interactive_for_chat(chat);
            if report_first {
                show_consumed_family_input(&console, &snapshot, run.into()).unwrap();
                assert!(
                    !console.test_conversation_text().contains("Report line"),
                    "a report whose run has not ended waits for its completion"
                );
                assert!(!console.input_is_echoed(chat, child_run).unwrap());
            }
            snapshot.family_completions.push(completion.clone());
            show_family_completion(&console, &completion, &snapshot).unwrap();
            show_consumed_family_input(&console, &snapshot, run.into()).unwrap();
            show_family_completion(&console, &completion, &snapshot).unwrap();
            assert_eq!(
                console.test_conversation_text(),
                "\n⏺ Teammate @audit finished · 2s\n  Report line\n",
                "report first: {report_first}"
            );
        }
    }

    #[test]
    fn a_tick_delivery_is_visible_once_when_its_live_run_starts() {
        use super::{ChatSnapshot, consume_input_echo_from_snapshot};
        let chat = uuid::Uuid::new_v4();
        let run = uuid::Uuid::new_v4();
        let instance = "hosted:7247ea8b-fced-497c-8e04-2885648826e";
        let payload = serde_json::json!({"items":[{"instance":instance,"title":"Stock watcher","seq":"7","updates":1,"data":{"text":"second check"}}]});
        let text = tickloop_harness::protocol::local_messages::tick_delivery(&payload);
        let mut snapshot: ChatSnapshot = serde_json::from_value(serde_json::json!({
            "id":chat,"folder":"/work","messages":[{"role":"user","content":[{"type":"text","text":text}]}],"chat_loop":{"id":"00000000-0000-0000-0000-000000000000","state":"Idle"}
        })).unwrap();
        snapshot.message_ids.insert(0, uuid::Uuid::new_v4());
        snapshot.message_runs.insert(0, run.into());
        let history = super::workflows::public_history(&snapshot, super::DisplayProfile::default());
        assert!(
            history.contains("TickUpdate: Stock watcher"),
            "history: {history:?}"
        );
        let console = Console::test_interactive_for_chat(chat);
        console.defer_input_echo(chat, run).unwrap();
        consume_input_echo_from_snapshot(&console, &snapshot, run).unwrap();
        let live = console.test_conversation_text();
        assert!(
            live.contains("TickUpdate: Stock watcher"),
            "live Tick Update source line is missing"
        );
        consume_input_echo_from_snapshot(&console, &snapshot, run).unwrap();
        assert_eq!(
            console.test_conversation_text(),
            live,
            "a repeated snapshot does not repeat the delivery"
        );
        let replay = Console::test_interactive_for_chat(chat);
        super::workflows::write_public_history(&replay, &snapshot).unwrap();
        let replayed = replay.test_conversation_text();
        consume_input_echo_from_snapshot(&replay, &snapshot, run).unwrap();
        assert_eq!(
            replay.test_conversation_text(),
            replayed,
            "history already rendered this source"
        );

        let cached = Console::test_interactive_for_chat(chat);
        cached.select_chat(Some(uuid::Uuid::new_v4())).unwrap();
        consume_input_echo_from_snapshot(&cached, &snapshot, run).unwrap();
        assert!(
            !cached
                .test_conversation_text()
                .contains("TickUpdate: Stock watcher"),
            "another Chat must not receive the label"
        );
        cached.select_chat(Some(chat)).unwrap();
        assert!(
            cached
                .test_conversation_text()
                .contains("TickUpdate: Stock watcher")
        );
        consume_input_echo_from_snapshot(&cached, &snapshot, run).unwrap();
        assert_eq!(
            cached
                .test_conversation_text()
                .matches("TickUpdate: Stock watcher")
                .count(),
            1
        );
    }

    #[test]
    fn mixed_native_deliveries_keep_both_labels_once() {
        use super::{ChatSnapshot, consume_input_echo_from_snapshot};
        let chat = uuid::Uuid::new_v4();
        let run = uuid::Uuid::new_v4();
        let tick = tickloop_harness::protocol::local_messages::tick_delivery(
            &serde_json::json!({"items":[{"instance":"hosted:feed","title":"Feed","seq":"1","updates":1,"data":{}}]}),
        );
        let mut snapshot: ChatSnapshot = serde_json::from_value(serde_json::json!({"id":chat,"folder":"/work","messages":[{"role":"user","content":[{"type":"text","text":"@tickloop request req on artifact doc. Request input JSON:\n{}"},{"type":"text","text":tick}]}],"chat_loop":{"id":"00000000-0000-0000-0000-000000000000","state":"Idle"}})).unwrap();
        snapshot.message_runs.insert(0, run.into());
        let console = Console::test_interactive_for_chat(chat);
        for _ in 0..2 {
            consume_input_echo_from_snapshot(&console, &snapshot, run).unwrap();
        }
        let shown = console.test_conversation_text();
        assert_eq!(
            shown.matches("@tickloop request: doc").count(),
            1,
            "{shown}"
        );
        assert_eq!(shown.matches("TickUpdate: Feed").count(), 1, "{shown}");
    }

    #[test]
    fn an_artifact_request_is_visible_once_when_its_live_run_starts() {
        use super::{ChatSnapshot, consume_input_echo_from_snapshot};
        let chat = uuid::Uuid::new_v4();
        let run = uuid::Uuid::new_v4();
        let text = "@tickloop request req-1 on artifact doc. Request input JSON:\n{\"message\":\"please update\"}";
        let mut snapshot: ChatSnapshot = serde_json::from_value(serde_json::json!({
            "id":chat,"folder":"/work","messages":[{"role":"user","content":[{"type":"text","text":text}]}],"chat_loop":{"id":"00000000-0000-0000-0000-000000000000","state":"Idle"}
        })).unwrap();
        snapshot.artifact_ticks.push(serde_json::from_value(serde_json::json!({"artifact_id":"doc","origin":"https://docs.example","title":"doc","format":"html","url":"https://docs.example/artifact/doc","access_role":"owner","added_at_ms":1})).unwrap());
        snapshot.monitor.mark_taken_in(
            tick_protocol::InstanceRef::artifact("https://docs.example", "doc").unwrap(),
            tickloop_harness::base::private_fs::unix_time_milliseconds(),
        );
        snapshot.message_ids.insert(0, uuid::Uuid::new_v4());
        snapshot.message_runs.insert(0, run.into());
        let history = super::workflows::public_history(&snapshot, super::DisplayProfile::default());
        assert!(
            history.contains("@tickloop request: doc"),
            "history: {history:?}"
        );
        let console = Console::test_interactive_for_chat(chat);
        console.defer_input_echo(chat, run).unwrap();
        consume_input_echo_from_snapshot(&console, &snapshot, run).unwrap();
        let flash = console
            .test_conversation_rows()
            .into_iter()
            .find(|line| line.text().contains("@tickloop request: doc"))
            .unwrap()
            .band
            .unwrap();
        assert_eq!(flash.columns.start, 2);
        assert!(matches!(
            flash.role,
            super::style::Role::TakeInFlashStrong | super::style::Role::TakeInFlashSoft
        ));
        let live = console.test_conversation_text();
        assert!(
            live.contains("@tickloop request: doc"),
            "live Artifact request label is missing"
        );
        consume_input_echo_from_snapshot(&console, &snapshot, run).unwrap();
        assert_eq!(
            console.test_conversation_text(),
            live,
            "a repeated snapshot does not repeat the delivery"
        );
        let replay = Console::test_interactive_for_chat(chat);
        super::workflows::write_public_history(&replay, &snapshot).unwrap();
        let replayed = replay.test_conversation_text();
        consume_input_echo_from_snapshot(&replay, &snapshot, run).unwrap();
        assert_eq!(
            replay.test_conversation_text(),
            replayed,
            "history already rendered this source"
        );

        let cached = Console::test_interactive_for_chat(chat);
        cached.select_chat(Some(uuid::Uuid::new_v4())).unwrap();
        consume_input_echo_from_snapshot(&cached, &snapshot, run).unwrap();
        assert!(
            !cached
                .test_conversation_text()
                .contains("@tickloop request: doc"),
            "another Chat must not receive the label"
        );
        cached.select_chat(Some(chat)).unwrap();
        assert!(
            cached
                .test_conversation_text()
                .contains("@tickloop request: doc")
        );
        consume_input_echo_from_snapshot(&cached, &snapshot, run).unwrap();
        assert_eq!(
            cached
                .test_conversation_text()
                .matches("@tickloop request: doc")
                .count(),
            1
        );
    }

    #[test]
    fn consumed_agent_message_stays_deferred_until_the_selected_view_can_render_it() {
        use super::{
            ChatSnapshot, Console, consume_input_echo_from_snapshot,
            reconcile_consumed_input_echoes,
        };
        let chat = uuid::Uuid::new_v4();
        let run = uuid::Uuid::new_v4();
        let source = uuid::Uuid::new_v4();
        let text = format!(
            "<agent-message>{}</agent-message>",
            serde_json::json!({"source":{"sender_relationship":"child","sender_handle":"audit","sender_label":"Report check","is_result":false},"message":"Which year should I use?"})
        );
        let mut snapshot:ChatSnapshot=serde_json::from_value(serde_json::json!({"id":chat,"folder":"/work","messages":[{"role":"user","content":[{"type":"text","text":text}]}],"chat_loop":{"id":"00000000-0000-0000-0000-000000000000","state":"Idle"}})).unwrap();
        snapshot.message_ids.insert(0, source);
        snapshot.message_runs.insert(0, run.into());
        snapshot
            .agent_message_origins
            .insert(source, serde_json::json!({"sender_relationship":"child"}));
        let (console, _) = Console::start(false).unwrap();
        console.select_chat(Some(chat)).unwrap();
        console.defer_input_echo(chat, run).unwrap();
        assert!(
            consume_input_echo_from_snapshot(&console, &snapshot, run)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            console.awaiting_input_echoes(chat).unwrap(),
            vec![run],
            "a successful human-source lookup must not discard the Agent echo before its separate snapshot read succeeds"
        );
        console.select_chat(None).unwrap();
        assert_eq!(console.awaiting_input_echoes(chat).unwrap(), vec![run]);
        console.select_chat(Some(chat)).unwrap();
        reconcile_consumed_input_echoes(&console, &snapshot).unwrap();
        assert!(console.awaiting_input_echoes(chat).unwrap().is_empty());
        assert!(console.input_is_echoed(chat, source).unwrap());
    }
    use super::{
        Agent, BranchRequest, CommandTiming, Console, EventKind, Menus, Renderer, TUI_SYNTAX,
        Workflow, branch_failure_message, branch_name, command_timing, help_text,
        is_branch_command, is_rewind_command, open_rewind, safe_line,
        should_dismiss_menu_before_dispatch, should_pause_input_for_summary,
        switch_to_saved_branch, tui_renderer_request,
    };
    use super::{
        BackgroundRecovery, BackgroundRunNotice, BackgroundWaitKind, ContextWarnings, Receipt,
        UNLABELED_BACKGROUND_AGENT, background_wait, context_usage, input_closed_while_waiting,
        observe_user_shell_record, readable_background_label, recover_background_chat,
        track_run_requests,
    };
    use std::{
        collections::{HashMap, HashSet},
        path::PathBuf,
    };
    use tickloop_harness::protocol::{Decision, PermissionRule};
    use tickloop_harness::protocol::{ModelRequestKind, RequestOutcome, RequestOwner};
    use uuid::Uuid;

    #[tokio::test]
    async fn a_chat_is_warned_once_as_its_context_nears_the_compaction_line_and_again_after_compacting()
     {
        let agent = Agent::new_embedded(
            tickloop_harness::test_support::config_with_window(200_000),
            Arc::new(tickloop_harness::test_support::ScriptedModel::new(
                Vec::new(),
            )),
        );
        let chat_id = agent.create_chat(PathBuf::from("/tmp")).await.unwrap();
        let mut status =
            tickloop_harness::protocol::ChatStatus::of(&agent.snapshot(chat_id).await.unwrap());
        status.auto_compact_tokens = Some(167_000);
        let mut warnings = ContextWarnings::default();
        let mut observe = |used: u64| {
            status.context_tokens = Some(used);
            let gauge = context_usage(&status).unwrap();
            (gauge.is_nearly_full, warnings.crossing_notice(&status))
        };
        assert_eq!(observe(146_999), (false, None), "below the warning");
        assert_eq!(
            observe(147_000),
            (
                true,
                Some(
                    "Context nearly full · auto-compact at 167k tokens · /compact to compact now"
                        .into()
                )
            ),
            "crossing into the warning buffer warns"
        );
        assert_eq!(
            observe(160_000),
            (true, None),
            "the same crossing warns once"
        );
        assert_eq!(observe(20_000), (false, None), "compaction resets it");
        assert!(
            observe(150_000).1.is_some(),
            "the next crossing warns again"
        );
        agent.shutdown().await;
    }

    #[tokio::test]
    async fn context_starts_at_zero_and_uses_the_effective_model_window() {
        let agent = Agent::new_embedded(
            tickloop_harness::test_support::config(),
            Arc::new(tickloop_harness::test_support::ScriptedModel::new(
                Vec::new(),
            )),
        );
        let chat_id = agent.create_chat(PathBuf::from("/tmp")).await.unwrap();
        let mut snapshot = agent.snapshot(chat_id).await.unwrap();
        assert_eq!(
            context_usage(&tickloop_harness::protocol::ChatStatus::of(&snapshot)),
            None
        );
        snapshot.model = tickloop_harness::model::cards::ModelCatalog::defaults()
            .get("anthropic-api", "claude-haiku-5-5")
            .unwrap();
        assert_eq!(
            context_usage(&tickloop_harness::protocol::ChatStatus::of(&snapshot)),
            Some((0, 1_000_000).into())
        );

        let request = snapshot
            .request_usage
            .start(
                RequestOwner {
                    chat_id: chat_id.into(),
                    loop_id: Some(Uuid::new_v4().into()),
                    run_id: Some(Uuid::new_v4().into()),
                },
                ModelRequestKind::Conversation,
                snapshot.model.identity(),
            )
            .unwrap();
        assert_eq!(
            context_usage(&tickloop_harness::protocol::ChatStatus::of(&snapshot)),
            Some((0, 1_000_000).into())
        );
        let mut without_report = snapshot.clone();
        without_report
            .request_usage
            .finish(request, RequestOutcome::Accepted)
            .unwrap();
        assert_eq!(
            context_usage(&tickloop_harness::protocol::ChatStatus::of(&without_report)),
            Some((0, 1_000_000).into())
        );
        let measurement =
            tickloop_harness::protocol::ContextMeasurement::capture(&snapshot, 1000, 1000);
        snapshot
            .request_usage
            .record_context_measurement(request, measurement)
            .unwrap();
        snapshot
            .request_usage
            .observe(
                request,
                tickloop_harness::protocol::Usage {
                    input_tokens: Some(10_000),
                    output_tokens: None,
                    cache_read_input_tokens: Some(2_000),
                    cache_write_input_tokens: Some(500),
                    reasoning_output_tokens: None,
                },
            )
            .unwrap();
        assert_eq!(
            context_usage(&tickloop_harness::protocol::ChatStatus::of(&snapshot)),
            Some((12_500, 1_000_000).into())
        );
        snapshot
            .request_usage
            .finish(request, RequestOutcome::Accepted)
            .unwrap();
        snapshot
            .request_usage
            .start(
                RequestOwner {
                    chat_id: chat_id.into(),
                    loop_id: Some(Uuid::new_v4().into()),
                    run_id: Some(Uuid::new_v4().into()),
                },
                ModelRequestKind::Conversation,
                snapshot.model.identity(),
            )
            .unwrap();
        assert_eq!(
            context_usage(&tickloop_harness::protocol::ChatStatus::of(&snapshot)),
            Some((12_500, 1_000_000).into())
        );
    }

    fn user_shell_record(
        chat_id: Uuid,
        id: Uuid,
        state: tickloop_harness::protocol::UserShellState,
        starts_in_background: bool,
    ) -> tickloop_harness::protocol::UserShellRecord {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "chat_id": chat_id,
            "command": "sleep 20",
            "cwd": "/work",
            "starts_in_background": starts_in_background,
            "state": state,
            "pid": null,
            "exit_code": null,
            "signal": null,
            "reason": null,
            "output_ref": null,
            "output_bytes": 0,
            "submitted_at_message": 0,
        }))
        .unwrap()
    }

    #[test]
    fn a_running_update_received_after_backgrounding_keeps_the_shell_in_the_background() {
        use tickloop_harness::protocol::UserShellState;
        let chat = Uuid::new_v4();
        let command = Uuid::new_v4();
        let mut foreground_shells = HashMap::from([(chat, command)]);
        let mut backgrounded_shells = HashSet::new();
        let stale_running = user_shell_record(chat, command, UserShellState::Running, false);

        foreground_shells.remove(&chat);
        backgrounded_shells.insert(command);
        let stale = observe_user_shell_record(
            &mut foreground_shells,
            &mut backgrounded_shells,
            &stale_running,
        );
        assert!(!stale.render_in_chat);
        assert_eq!(stale.foreground, None);
        assert!(foreground_shells.is_empty());

        let backgrounded = user_shell_record(chat, command, UserShellState::Running, true);
        let observed = observe_user_shell_record(
            &mut foreground_shells,
            &mut backgrounded_shells,
            &backgrounded,
        );
        assert!(!observed.render_in_chat);
        assert!(foreground_shells.is_empty());

        let finished = user_shell_record(chat, command, UserShellState::Completed, true);
        observe_user_shell_record(&mut foreground_shells, &mut backgrounded_shells, &finished);
        assert!(backgrounded_shells.is_empty());
    }

    #[test]
    fn a_shell_backgrounded_elsewhere_leaves_the_foreground() {
        use tickloop_harness::protocol::UserShellState;
        let chat = Uuid::new_v4();
        let command = Uuid::new_v4();
        let mut foreground_shells = HashMap::new();
        let mut backgrounded_shells = HashSet::new();

        let running = user_shell_record(chat, command, UserShellState::Running, false);
        let started =
            observe_user_shell_record(&mut foreground_shells, &mut backgrounded_shells, &running);
        assert_eq!(started.foreground, Some(true));
        assert!(started.render_in_chat);
        assert_eq!(foreground_shells.get(&chat), Some(&command));

        let backgrounded = user_shell_record(chat, command, UserShellState::Running, true);
        let observed = observe_user_shell_record(
            &mut foreground_shells,
            &mut backgrounded_shells,
            &backgrounded,
        );
        assert_eq!(observed.foreground, Some(false));
        assert!(!observed.render_in_chat);
        assert!(foreground_shells.is_empty());
        let stale =
            observe_user_shell_record(&mut foreground_shells, &mut backgrounded_shells, &running);
        assert_eq!(stale.foreground, None);
        assert!(foreground_shells.is_empty());
    }

    #[test]
    fn safe_lines_escape_every_terminal_control_character() {
        assert_eq!(safe_line("line\nrow\t\x1b"), "line\\nrow\\t\\u{1b}");
    }

    #[test]
    fn command_timing_separates_live_controls_and_settings_from_after_turn_work() {
        for command in [
            "/mode plan",
            "/model config.json",
            "/websearch",
            "/settings",
            "/interrupt",
            "/diff",
            "/allow 00000000-0000-0000-0000-000000000000 once",
            "/status",
            "/branch experiment",
        ] {
            assert_eq!(
                command_timing(command),
                CommandTiming::Immediate,
                "{command}"
            );
        }
        for command in ["/compact", "/cwd elsewhere", "/add-dir ../shared"] {
            assert_eq!(
                command_timing(command),
                CommandTiming::AfterTurn,
                "{command}"
            );
        }
    }

    #[test]
    fn tui_without_an_argument_asks_for_the_active_renderer() {
        for command in ["/tui", "/tui   "] {
            assert_eq!(tui_renderer_request(command).unwrap(), None, "{command}");
        }
    }

    #[test]
    fn tui_accepts_both_renderer_names() {
        assert_eq!(
            tui_renderer_request("/tui inline").unwrap(),
            Some(Renderer::Inline)
        );
        assert_eq!(
            tui_renderer_request("/tui fullscreen").unwrap(),
            Some(Renderer::Fullscreen)
        );
    }

    #[test]
    fn tui_rejects_unknown_renderers_and_extra_arguments() {
        for command in [
            "/tui default",
            "/tui classic",
            "/tui fullscreen extra",
            "/tui 1",
        ] {
            assert_eq!(
                tui_renderer_request(command).unwrap_err(),
                TUI_SYNTAX,
                "{command}"
            );
        }
    }

    #[test]
    fn tui_runs_immediately_rather_than_after_the_current_turn() {
        assert_eq!(command_timing("/tui fullscreen"), CommandTiming::Immediate);
    }

    #[test]
    fn help_advertises_ctrl_n_p_for_lists_and_never_j_k() {
        let help = help_text();
        assert!(!help.contains("j/k"), "{help}");
        assert!(help.contains("Focused lists also accept ctrl+n/p"));
        assert!(help.contains("ctrl+n/p or arrows scroll"));
    }

    #[test]
    fn branch_parses_the_optional_display_name_without_a_fork_alias() {
        assert!(is_branch_command("/branch"));
        assert!(is_branch_command("/branch 研究路径 β"));
        assert!(!is_branch_command("/branching"));
        assert!(!is_branch_command("/fork"));
        assert!(help_text().contains("/branch [NAME]"));
        assert_eq!(branch_name("/branch").unwrap(), None);
        assert_eq!(branch_name("/branch   ").unwrap(), None);
        assert_eq!(
            branch_name("/branch 研究路径 β").unwrap(),
            Some("研究路径 β".into())
        );
        assert_eq!(branch_name("/branching").unwrap_err(), "use /branch [NAME]");
        assert_eq!(branch_name("/fork").unwrap_err(), "use /branch [NAME]");
    }

    #[test]
    fn branch_bypasses_summary_input_deferral_for_immediate_busy_rejection() {
        assert!(!should_pause_input_for_summary(&InputEvent::Prompt {
            family_target: None,
            text: "/branch summary-busy".into(),
            file_references: Vec::new(),
            images: Vec::new(),
            send_now: false,
        }));
        assert!(!should_pause_input_for_summary(&InputEvent::Line(
            "/branch".into()
        )));
        assert!(should_pause_input_for_summary(&InputEvent::Prompt {
            family_target: None,
            text: "continue after summary".into(),
            file_references: Vec::new(),
            images: Vec::new(),
            send_now: false,
        }));
    }

    #[test]
    fn branch_keeps_the_current_menu_until_dispatch_succeeds() {
        assert!(!should_dismiss_menu_before_dispatch("/branch"));
        assert!(!should_dismiss_menu_before_dispatch(
            "/branch retryable name"
        ));
        assert!(should_dismiss_menu_before_dispatch("/status"));
    }

    #[test]
    fn branch_failure_message_escapes_controls_and_stays_on_one_terminal_line() {
        let output = branch_failure_message(&"directory\nforged\rrow\ttab\x1b[2J");

        assert_eq!(output.lines().count(), 1);
        assert_eq!(
            output,
            "Failed to branch conversation: directory\\nforged\\rrow\\ttab\\u{1b}[2J"
        );
        assert!(!output.contains('\x1b'));
    }

    #[tokio::test]
    async fn saved_branch_survives_terminal_switch_failure_and_chat_picker_reopens_it() {
        let directory = tempfile::tempdir().unwrap();
        let archive = directory.path().join("state");
        let agent = Agent::open_embedded(
            archive.clone(),
            tickloop_harness::test_support::config(),
            Arc::new(tickloop_harness::test_support::ScriptedModel::new(vec![
                tickloop_harness::test_support::text("source answer"),
            ])),
        )
        .unwrap();
        let source_chat_id = agent.create_chat(directory.path().into()).await.unwrap();
        let mut events = agent.subscribe();
        agent
            .submit(source_chat_id, "source prompt".into())
            .await
            .unwrap();
        loop {
            match tickloop_harness::test_support::next_event(&mut events)
                .await
                .kind
            {
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("source fixture failed: {error}"),
                _ => {}
            }
        }
        let outcome = agent
            .branch_chat(BranchRequest {
                request_id: Uuid::new_v4(),
                source_chat_id: source_chat_id.into(),
                name: Some("recoverable child".into()),
            })
            .await
            .unwrap();
        let child_chat_id = outcome.child_chat_id;
        let durable_child = serde_json::to_value(&outcome.snapshot).unwrap();
        let (console, _) = Console::start(false).unwrap();
        console.reset_chat(source_chat_id).unwrap();
        console
            .fail_next_reset_chat("renderer\nfailed\x1b[2J")
            .unwrap();
        console
            .fail_next_branch_commit("editor\ncleanup\x1b[3J")
            .unwrap();

        let failure = switch_to_saved_branch(&console, child_chat_id).unwrap_err();

        assert_eq!(failure.lines().count(), 1);
        assert!(failure.contains(&format!("Branch {} was saved", child_chat_id.public())));
        assert!(failure.contains("renderer\\nfailed\\u{1b}[2J"));
        assert!(failure.contains("editor\\ncleanup\\u{1b}[3J"));
        assert!(failure.contains("Use /resume to find it"));
        assert!(failure.contains(&format!("/resume {}", child_chat_id.public())));
        assert!(!failure.contains('\x1b'));
        assert_eq!(agent.list_chats().await.len(), 2);
        assert_eq!(
            serde_json::to_value(agent.snapshot(child_chat_id.into()).await.unwrap()).unwrap(),
            durable_child
        );
        agent.shutdown().await;
        drop(agent);

        let reopened = Agent::open_embedded(
            archive,
            tickloop_harness::test_support::config(),
            Arc::new(tickloop_harness::test_support::ScriptedModel::new(
                Vec::new(),
            )),
        )
        .unwrap();
        let (picker, workflow) = Workflow::chats(&reopened, directory.path(), None, false)
            .await
            .unwrap();
        let Workflow::Chats { choices, .. } = &workflow else {
            panic!("the Chat picker returned the wrong workflow");
        };
        let child_index = choices
            .iter()
            .position(|chat| *chat == child_chat_id)
            .expect("the Chat picker did not list the saved child");
        assert!(
            picker.entries().iter().all(|entry| !entry
                .plain_row()
                .contains(&child_chat_id.uuid().to_string())),
            "picker rows must not show Chat IDs"
        );
        let effect = workflow
            .choose(&reopened, &console, Some(child_index))
            .await
            .unwrap();
        assert_eq!(effect.selection, Some(Into::into(child_chat_id)));
        // A Branch's creation request and copy seed are valid only in the process that created it;
        // on resume, metadata restores only its source and name.
        let mut restored_child = durable_child.clone();
        restored_child["branch_origin"]["creation"] = serde_json::Value::Null;
        assert_eq!(
            serde_json::to_value(effect.restored.unwrap()).unwrap(),
            restored_child
        );
        reopened.shutdown().await;
    }

    #[test]
    fn removed_task_and_subscription_commands_are_not_dispatched() {
        for command in ["/tasks", "/subscriptions"] {
            assert_eq!(
                command_timing(command),
                CommandTiming::AfterTurn,
                "{command}"
            );
            assert!(!help_text().contains(command), "{command}");
        }
        assert!(help_text().contains(
            "Monitor lists this Chat's Ticks, Artifacts, background Shells and scheduled tasks"
        ));
        assert!(help_text().contains("opens Monitor once without moving input focus"));
    }

    #[test]
    fn help_offers_daemon_stop_and_hides_the_process_entries() {
        let help = help_text();
        assert!(help.contains("tickloop daemon stop"));
        assert!(help.contains("all of its workers, which is every running Agent"));
        assert!(help.contains("Ticks in the Tickhost keep running"));
        for hidden in [
            "daemon run",
            "tickloop worker",
            "tickhost run",
            "service stop",
        ] {
            assert!(!help.contains(hidden), "{hidden}");
        }
    }

    #[test]
    fn rewind_command_dispatches_immediately_and_has_help() {
        {
            let command = "/rewind";
            assert!(is_rewind_command(command), "{command}");
            assert_eq!(
                command_timing(command),
                CommandTiming::Immediate,
                "{command}"
            );
            assert!(help_text().contains(command), "missing help for {command}");
        }
        for command in [
            "/rew",
            "/checkpoint",
            "/undo",
            "/checkpoint extra",
            "/undo 2",
            "undo",
            "u",
        ] {
            assert!(!is_rewind_command(command), "unexpected alias {command}");
        }
    }

    #[tokio::test]
    async fn rewind_entry_keeps_busy_and_permission_guards_without_starting_model_work() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("approval-required.txt");
        let model = Arc::new(tickloop_harness::test_support::ScriptedModel::new(vec![
            tickloop_harness::test_support::text("Recorded a real prompt"),
            tickloop_harness::test_support::call(
                "write",
                "Write",
                serde_json::json!({
                    "file_path": "approval-required.txt", "content": "requires approval"
                }),
            ),
        ]));
        let agent = Agent::new_embedded(tickloop_harness::test_support::config(), model.clone());
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        agent
            .set_permission_rules(
                chat,
                vec![PermissionRule {
                    id: Uuid::new_v4(),
                    decision: Decision::Ask,
                    tool: "Write".into(),
                    specifier: None,
                    input: None,
                    loop_id: None,
                    persistent: false,
                }],
            )
            .await
            .unwrap();
        let (console, _) = Console::start(false).unwrap();
        let mut menus = Menus::default();
        open_rewind(&agent, &console, &mut menus, None, false)
            .await
            .unwrap();
        open_rewind(&agent, &console, &mut menus, Some(chat.into()), false)
            .await
            .unwrap();
        assert!(menus.id().is_none());
        assert!(model.requests.lock().unwrap().is_empty());
        assert!(
            agent
                .snapshot(chat)
                .await
                .unwrap()
                .rewind_points()
                .is_empty()
        );

        let mut events = agent.subscribe();
        agent
            .submit(chat, "Record this prompt".into())
            .await
            .unwrap();
        loop {
            match tickloop_harness::test_support::next_event(&mut events)
                .await
                .kind
            {
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("rewind entry fixture failed: {error}"),
                _ => {}
            }
        }
        let before = agent.snapshot(chat).await.unwrap();
        open_rewind(&agent, &console, &mut menus, Some(chat.into()), true)
            .await
            .unwrap();
        assert!(menus.id().is_none());
        {
            let command = "/rewind";
            assert!(is_rewind_command(command));
            open_rewind(&agent, &console, &mut menus, Some(chat.into()), false)
                .await
                .unwrap();
            assert!(
                menus.id().is_some(),
                "{command} did not open the recorded selector"
            );
            let opened = menus.id();
            open_rewind(&agent, &console, &mut menus, Some(chat.into()), true)
                .await
                .unwrap();
            assert_eq!(menus.id(), opened, "busy guard dismissed the current menu");
            menus.dismiss(&console).unwrap();
        }
        assert_eq!(model.requests.lock().unwrap().len(), 1);
        assert_eq!(
            serde_json::to_value(agent.snapshot(chat).await.unwrap()).unwrap(),
            serde_json::to_value(before).unwrap()
        );

        agent
            .submit(chat, "Request a file write".into())
            .await
            .unwrap();
        loop {
            match tickloop_harness::test_support::next_event(&mut events)
                .await
                .kind
            {
                EventKind::PermissionRequested { .. } => break,
                EventKind::Failed(error) => panic!("permission fixture failed: {error}"),
                _ => {}
            }
        }
        let pending = agent.snapshot(chat).await.unwrap();
        open_rewind(&agent, &console, &mut menus, Some(chat.into()), false)
            .await
            .unwrap();
        assert!(menus.id().is_none());
        assert_eq!(
            serde_json::to_value(agent.snapshot(chat).await.unwrap()).unwrap(),
            serde_json::to_value(pending).unwrap()
        );
        assert_eq!(model.requests.lock().unwrap().len(), 2);
        assert!(!target.exists());
        agent.shutdown().await;
    }

    #[tokio::test]
    async fn an_interrupt_before_any_reply_puts_the_prompt_back_only_into_an_empty_editor() {
        use tickloop_harness::model::{ModelClient, ModelDelta, ModelError, ModelOutput};
        use tickloop_harness::protocol::{ImageContext, ImageOrigin, InputContext};

        struct UntilInterrupted(tokio::sync::Notify);

        #[async_trait::async_trait]
        impl ModelClient for UntilInterrupted {
            async fn generate_title(
                &self,
                request: tickloop_harness::model::ModelRequest,
                _: tokio::sync::mpsc::UnboundedSender<ModelDelta>,
                _: tokio_util::sync::CancellationToken,
            ) -> Result<ModelOutput, ModelError> {
                Ok(tickloop_harness::test_support::title_output(request))
            }

            fn identity(&self) -> tickloop_harness::protocol::ModelIdentity {
                tickloop_harness::protocol::ModelIdentity {
                    provider: tickloop_harness::protocol::ModelProvider::Api,
                    profile: "fixture".into(),
                    protocol: tickloop_harness::protocol::Protocol::Messages,
                    requested_model: "fixture".into(),
                }
            }

            async fn complete(
                &self,
                _: tickloop_harness::model::ModelRequest,
                _: tokio::sync::mpsc::UnboundedSender<ModelDelta>,
                cancel: tokio_util::sync::CancellationToken,
            ) -> Result<ModelOutput, ModelError> {
                self.0.notify_one();
                cancel.cancelled().await;
                Err(ModelError::Cancelled)
            }
        }

        let directory = tempfile::tempdir().unwrap();
        let model = Arc::new(UntilInterrupted(tokio::sync::Notify::new()));
        let agent = Agent::new_embedded(tickloop_harness::test_support::config(), model.clone());
        let chat = agent.create_chat(directory.path().into()).await.unwrap();
        let mut png = Vec::new();
        ::image::DynamicImage::ImageRgba8(::image::RgbaImage::new(4, 3))
            .write_to(
                &mut std::io::Cursor::new(&mut png),
                ::image::ImageFormat::Png,
            )
            .unwrap();
        let image = ImageContext {
            origin: ImageOrigin::Pasted { id: 1 },
            image: tickloop_harness::base::image::prepare(&png).unwrap(),
        };
        let prompt = "Explain [Image #1]";
        let mut events = agent.subscribe();
        let receipt = agent
            .submit_with_input_contexts(
                chat,
                prompt.into(),
                vec![InputContext::Image(image.clone())],
                Vec::new(),
            )
            .await
            .unwrap();
        model.0.notified().await;
        agent.interrupt(chat).await.unwrap();
        loop {
            let event = tickloop_harness::test_support::next_event(&mut events).await;
            if event.run_id == receipt.run_id && matches!(event.kind, EventKind::Interrupted) {
                break;
            }
        }
        let console = Console::test_interactive_for_chat(chat);
        super::workflows::write_public_history(&console, &agent.snapshot(chat).await.unwrap())
            .unwrap();
        assert!(console.test_conversation_text().contains(prompt));

        console
            .restore_rewind_draft("typed after the interrupt", &[])
            .unwrap();
        assert!(
            super::take_back_interrupted_prompt(&agent, &console, chat.into(), receipt.run_id)
                .await
                .unwrap()
                .is_none(),
            "a draft typed after the interrupt keeps the prompt in the conversation"
        );
        assert!(console.test_conversation_text().contains(prompt));
        assert_eq!(agent.snapshot(chat).await.unwrap().rewind_points().len(), 1);

        let console = Console::test_interactive_for_chat(chat);
        super::workflows::write_public_history(&console, &agent.snapshot(chat).await.unwrap())
            .unwrap();
        let snapshot =
            super::take_back_interrupted_prompt(&agent, &console, chat.into(), receipt.run_id)
                .await
                .unwrap()
                .expect("an empty editor gets the prompt back");
        assert!(snapshot.messages.is_empty());
        assert!(!console.test_conversation_text().contains(prompt));
        assert_eq!(console.test_draft(), prompt);
        assert_eq!(console.test_draft_images(), [image]);
        assert_eq!(
            console.test_status_text(),
            "Interrupted prompt restored for editing"
        );
        agent.shutdown().await;
    }

    #[tokio::test]
    async fn lagged_recovery_restores_background_requests_and_reports_runs_that_ended() {
        let directory = tempfile::tempdir().unwrap();
        let model = Arc::new(tickloop_harness::test_support::ScriptedModel::new(vec![
            tickloop_harness::test_support::call(
                "write",
                "Write",
                serde_json::json!({"file_path": "background.txt", "content": "needs approval"}),
            ),
            tickloop_harness::test_support::text("Wrote the background file"),
        ]));
        let agent = Agent::new_embedded(tickloop_harness::test_support::config(), model);
        let background_chat = agent.create_chat(directory.path().into()).await.unwrap();
        let foreground_chat = Some(agent.create_chat(directory.path().into()).await.unwrap());
        agent
            .set_permission_rules(
                background_chat,
                vec![PermissionRule {
                    id: Uuid::new_v4(),
                    decision: Decision::Ask,
                    tool: "Write".into(),
                    specifier: None,
                    input: None,
                    loop_id: None,
                    persistent: false,
                }],
            )
            .await
            .unwrap();
        let mut events = agent.subscribe();
        let receipt = agent
            .submit(background_chat, "Write in the background".into())
            .await
            .unwrap();
        let request = loop {
            match tickloop_harness::test_support::next_event(&mut events)
                .await
                .kind
            {
                EventKind::PermissionRequested { request } => break request,
                EventKind::Failed(error) => panic!("permission fixture failed: {error}"),
                _ => {}
            }
        };
        let (console, _) = Console::start(false).unwrap();
        assert_eq!(
            readable_background_label(&agent, &console, Uuid::new_v4().into()),
            UNLABELED_BACKGROUND_AGENT,
            "an Agent this View has not seen is not given an invented name"
        );
        agent.snapshot(background_chat).await.unwrap();
        let label = readable_background_label(&agent, &console, background_chat.into());
        assert_eq!(label, "Agent Write in the background");

        // Lag dropped the permission request event: the terminal only knows the submission receipt.
        let mut pending = HashMap::from([(receipt.run_id.uuid(), receipt.clone())]);
        let mut permissions = HashMap::new();
        let mut questions = HashMap::new();
        let snapshot = agent.snapshot(background_chat).await.unwrap();
        let recovery = recover_background_chat(
            &tickloop_harness::protocol::ChatStatus::of(&snapshot),
            &label,
            &mut pending,
            &mut permissions,
            &mut questions,
        );
        assert_eq!(
            recovery,
            BackgroundRecovery {
                run_notices: Vec::new(),
                request_notices: vec![format!(
                    "{label} · permission request {}",
                    tickloop_ids::base62::encode(request.id)
                )],
                failed: false,
            }
        );
        assert!(pending.contains_key(&receipt.run_id.uuid()));
        assert_eq!(
            permissions.get(&request.id),
            Some(&super::PermissionRequestOwner::new(
                background_chat.into(),
                &request
            ))
        );
        let wait =
            background_wait(foreground_chat.map(Into::into), &permissions, &questions).unwrap();
        assert_eq!(wait.kind, BackgroundWaitKind::Approval);
        assert_eq!(wait.chat_id, background_chat);
        assert_eq!(wait.request_id, request.id);
        let closed =
            input_closed_while_waiting(foreground_chat.map(Into::into), &permissions, &questions)
                .unwrap();
        assert!(
            closed.contains(&format!(
                "background Chat {}",
                tickloop_ids::base62::encode(background_chat)
            )) && closed.contains(&tickloop_ids::base62::encode(request.id)),
            "{closed}"
        );
        assert!(background_wait(Some(background_chat.into()), &permissions, &questions).is_none());

        agent
            .answer_permission(background_chat, request.id, true)
            .await
            .unwrap();
        loop {
            match tickloop_harness::test_support::next_event(&mut events)
                .await
                .kind
            {
                EventKind::Completed { .. } => break,
                EventKind::Failed(error) => panic!("approved fixture failed: {error}"),
                _ => {}
            }
        }
        // Lag also dropped the run-end event: leftover requests and pending runs are cleared from
        // the snapshot and the end notice is restored.
        let snapshot = agent.snapshot(background_chat).await.unwrap();
        let recovery = recover_background_chat(
            &tickloop_harness::protocol::ChatStatus::of(&snapshot),
            &label,
            &mut pending,
            &mut permissions,
            &mut questions,
        );
        assert_eq!(
            recovery,
            BackgroundRecovery {
                run_notices: vec![BackgroundRunNotice {
                    message: "reply completed".into(),
                    compact_message: None,
                    necessary: false
                }],
                request_notices: Vec::new(),
                failed: false,
            }
        );
        assert!(pending.is_empty() && permissions.is_empty());
        assert!(
            input_closed_while_waiting(foreground_chat.map(Into::into), &permissions, &questions)
                .is_none()
        );
        agent.shutdown().await;
    }

    #[test]
    fn merged_echoes_join_only_consecutive_merges_of_confirmed_echoes_into_one_run() {
        use super::{MergedEchoes, scrollback::Scrollback};
        let mut scrollback = Scrollback::new();
        let mut echo = || scrollback.mark_input(0..1);
        let (alpha, bravo, charlie) = (echo(), echo(), echo());
        let chat = Uuid::from_u128(1);
        let into = Uuid::from_u128(2);
        let merged = |sequence: u64, run: u128, into: Uuid| tickloop_harness::protocol::Event {
            sequence,
            chat_id: chat.into(),
            loop_id: Uuid::from_u128(3).into(),
            run_id: Uuid::from_u128(run).into(),
            kind: EventKind::InputMerged {
                into_run_id: into.into(),
            },
        };
        let accept = |echoes: &mut MergedEchoes| {
            echoes.accepted(Uuid::from_u128(10).into(), Some(alpha));
            echoes.accepted(Uuid::from_u128(11).into(), Some(bravo));
            echoes.accepted(Uuid::from_u128(12).into(), Some(charlie));
        };

        let mut echoes = MergedEchoes::default();
        accept(&mut echoes);
        assert_eq!(echoes.observe(&merged(5, 10, into), 0), None);
        assert_eq!(
            echoes.observe(&merged(6, 11, into), 0),
            Some((alpha, bravo))
        );
        assert_eq!(
            echoes.observe(&merged(7, 12, into), 0),
            Some((bravo, charlie))
        );

        let mut echoes = MergedEchoes::default();
        accept(&mut echoes);
        echoes.observe(&merged(5, 10, into), 0);
        assert_eq!(
            echoes.observe(&merged(7, 11, into), 0),
            None,
            "a sequence gap is an event this View did not follow"
        );

        let mut echoes = MergedEchoes::default();
        accept(&mut echoes);
        echoes.observe(&merged(5, 10, into), 0);
        assert_eq!(echoes.observe(&merged(6, 99, into), 0), None);
        assert_eq!(
            echoes.observe(&merged(7, 12, into), 0),
            None,
            "an input merged without a local echo ends the block"
        );

        let mut echoes = MergedEchoes::default();
        accept(&mut echoes);
        echoes.observe(&merged(5, 10, into), 0);
        assert_eq!(echoes.observe(&merged(6, 11, Uuid::from_u128(4)), 0), None);
        echoes.observe(
            &tickloop_harness::protocol::Event {
                kind: EventKind::Text("reply".into()),
                ..merged(7, 11, into)
            },
            0,
        );
        assert_eq!(echoes.observe(&merged(8, 12, into), 0), None);

        let mut echoes = MergedEchoes::default();
        accept(&mut echoes);
        echoes.observe(&merged(5, 10, into), 0);
        echoes.close_all();
        assert_eq!(echoes.observe(&merged(6, 11, into), 0), None);
    }

    #[test]
    fn a_merge_a_restore_already_showed_never_joins_the_block() {
        use super::{MergedEchoes, scrollback::Scrollback};
        let mut scrollback = Scrollback::new();
        let (alpha, bravo) = (scrollback.mark_input(0..1), scrollback.mark_input(1..2));
        let into = Uuid::from_u128(2);
        let merged = |sequence: u64, run: u128| tickloop_harness::protocol::Event {
            sequence,
            chat_id: Uuid::from_u128(1).into(),
            loop_id: Uuid::from_u128(3).into(),
            run_id: Uuid::from_u128(run).into(),
            kind: EventKind::InputMerged {
                into_run_id: into.into(),
            },
        };
        let mut echoes = MergedEchoes::default();
        echoes.accepted(Uuid::from_u128(10).into(), Some(alpha));
        echoes.accepted(Uuid::from_u128(11).into(), Some(bravo));
        // Alpha's merge is shown live; then a restore displays the Agent through sequence 21, so
        // bravo's merge arrives afterwards as an event the View already shows.
        assert_eq!(echoes.observe(&merged(20, 10), 19), None);
        assert_eq!(
            echoes.observe(&merged(21, 11), 21),
            None,
            "only live events the View has not yet shown may join echoes"
        );
        assert!(
            echoes.accepted.is_empty(),
            "a merge already shown still releases its echo"
        );
        let mut live = MergedEchoes::default();
        live.accepted(Uuid::from_u128(10).into(), Some(alpha));
        live.accepted(Uuid::from_u128(11).into(), Some(bravo));
        live.observe(&merged(20, 10), 19);
        assert_eq!(
            live.observe(&merged(21, 11), 20),
            Some((alpha, bravo)),
            "the same merges, both live, still read as one block"
        );
    }

    #[tokio::test]
    async fn direct_shell_permission_tracks_its_owner_without_a_pending_model_run() {
        let chat = Uuid::new_v4();
        let request = tickloop_harness::protocol::PermissionRequest {
            parent_call: None,
            mcp_server: None,
            id: Uuid::new_v4(),
            loop_id: Uuid::new_v4().into(),
            execution: tickloop_harness::protocol::PermissionExecution::UserShell {
                command_id: Uuid::new_v4(),
            },
            name: "Bash".into(),
            input: serde_json::json!({"command":"python3 -c 'print(1)'"}),
            file_change: None,
            approval_reason: None,
            approval_source: tickloop_harness::protocol::PermissionApprovalSource::PermissionPolicy,
        };
        let event = tickloop_harness::protocol::Event {
            sequence: 1,
            chat_id: chat.into(),
            loop_id: request.loop_id,
            run_id: Uuid::nil().into(),
            kind: EventKind::PermissionRequested {
                request: request.clone(),
            },
        };
        let mut pending = HashMap::new();
        let mut permissions = HashMap::new();
        let mut questions = HashMap::new();
        track_run_requests(&event, &mut pending, &mut permissions, &mut questions);
        assert!(pending.is_empty());
        let wait = background_wait(Some(Uuid::new_v4().into()), &permissions, &questions).unwrap();
        assert_eq!(wait.chat_id, chat);
        assert_eq!(wait.request_id, request.id);
        let directory = tempfile::tempdir().unwrap();
        let core = tickloop_harness::runtime::Agent::new_embedded(
            tickloop_harness::test_support::config(),
            Arc::new(tickloop_harness::test_support::ScriptedModel::new(vec![])),
        );
        let other = core.create_chat(directory.path().into()).await.unwrap();
        let mut snapshot = core.snapshot(other).await.unwrap();
        super::sync_status_requests(
            &tickloop_harness::protocol::ChatStatus::of(&snapshot),
            &mut permissions,
            &mut questions,
        );
        assert!(permissions.contains_key(&request.id));
        let lost = tickloop_code_access::AgentSummary {
            availability: tickloop_code_access::ExecutionAvailability::Lost,
            execution: tickloop_code_access::ExecutionRef {
                daemon_epoch: Uuid::new_v4(),
                agent_id: chat,
                generation: 1,
            },
            chat_id: chat,
            folder: Some(PathBuf::new()),
            state: tickloop_harness::protocol::LoopState::WaitingPermission,
            run_id: None,
            title: "Lost child".into(),
            subagent: None,
            has_work: Some(true),
            pending_work: None,
            updated_at: None,
            run_started_at: None,
            preview: None,
            waiting_permissions: 1,
            waiting_questions: 0,
            parent: None,
            record: None,
            workspace: None,
            capabilities: vec![],
            views: vec![],
        };
        assert!(
            super::end_runs_of_lost_executions(
                &[lost],
                &mut pending,
                &mut permissions,
                &mut questions
            )
            .is_empty()
        );
        assert!(
            permissions.is_empty(),
            "lost direct Shell approval must not remain actionable"
        );
        snapshot.id = chat.into();
        super::sync_status_requests(
            &tickloop_harness::protocol::ChatStatus::of(&snapshot),
            &mut permissions,
            &mut questions,
        );
        assert!(permissions.is_empty());
        core.shutdown().await;
    }

    #[test]
    fn background_run_events_clear_pending_work_even_when_only_merged_or_taken_back() {
        let chat_id = Uuid::from_u128(1);
        let receipt = |run: u128| Receipt {
            chat_id: chat_id.into(),
            loop_id: Uuid::from_u128(2).into(),
            run_id: Uuid::from_u128(run).into(),
        };
        let mut pending = HashMap::from([
            (Uuid::from_u128(3), receipt(3)),
            (Uuid::from_u128(4), receipt(4)),
        ]);
        let mut permissions = HashMap::new();
        let mut questions = HashMap::new();
        for (run, kind) in [
            (
                3,
                EventKind::InputMerged {
                    into_run_id: Uuid::from_u128(5).into(),
                },
            ),
            (4, EventKind::InputTakenBack),
        ] {
            let event = tickloop_harness::protocol::Event {
                sequence: 1,
                chat_id: chat_id.into(),
                loop_id: Uuid::from_u128(2).into(),
                run_id: Uuid::from_u128(run).into(),
                kind,
            };
            track_run_requests(&event, &mut pending, &mut permissions, &mut questions);
        }
        assert!(pending.is_empty());
    }
}

#[cfg(test)]
mod status_agents_help_tests {
    #[test]
    fn help_describes_subagents_and_shared_tasks() {
        let help = super::help_text();
        assert!(help.contains("Subagents N") && help.contains("expanded by default"));
        assert!(help.contains("Click Subagents N") && help.contains("returns to the editor"));
        assert!(help.contains("x dismisses a finished subagent"));
        assert!(!help.contains("esc returns to the entry"));
        assert!(help.contains("Monitor's Shells group"));
        assert!(help.contains("ctrl+t") && help.contains("shared tasks"));
        assert!(!help.contains("Footer Tasks/Todo") && !help.contains("starts folded"));
    }

    #[test]
    fn help_names_the_hosted_tick_keys_without_a_monitor_command() {
        let help = super::help_text();
        assert!(!help.contains("tickloop monitor"));
        for expected in [
            "Press x twice within 3 seconds",
            "restoring its subscriptions at their frequencies",
            "Press x twice to remove a scheduled task or unlink an Artifact",
            "h includes earlier feedback",
        ] {
            assert!(help.contains(expected), "missing: {expected}");
        }

        assert!(help.contains(
            "esc esc leaves the card back to the list, which shows other cards with space"
        ));
        assert!(help.contains(
            "a click outside leaves too and then acts where it landed, so a click on another row shows its card"
        ));
    }
}
